// SPDX-License-Identifier: MIT
//
// hv_console.rs — EL2 hypervisor fault console
//
// Displays a full-screen crash report on HDMI when Linux causes a stage-2
// fault (or any EL2 exception).  Survives Linux boot because:
//
//   1. We write directly to the GPU framebuffer address captured before
//      ERET'ing to Linux.  Linux requests a NEW framebuffer address from
//      the GPU mailbox; ours stays mapped by the GPU scan-out until power-off.
//
//   2. The framebuffer region is marked S2AP=read-only in the stage-2 tables
//      (linux_vm::protect_framebuffer).  EL1 CPU writes fault silently at EL2
//      (instruction skipped, ERET) — our pixels are preserved even if Linux
//      initialises a simple-framebuffer on the same GPU buffer.
//
//   3. hv_fault_screen() repaints the full buffer first, so any partial Linux
//      corruption is overwritten immediately.
//
// Rendering at 3× scale, 1280×720 → 53 columns × 30 rows.

use crate::font::FONT;

// ── Rendering constants ───────────────────────────────────────────────────────

const SCALE:  u32 = 3;
const FONT_W: u32 = 8;
const FONT_H: u32 = 8;
const CHAR_W: u32 = FONT_W * SCALE; // 24 screen pixels per char column
const CHAR_H: u32 = FONT_H * SCALE; // 24 screen pixels per char row

// Colour palette (0xAARRGGBB)
const C_BG:    u32 = 0xFF1A0000; // dark red background
const C_FG:    u32 = 0xFFE0E0E0; // light grey general text
const C_TITLE: u32 = 0xFFFF4040; // bright red title
const C_REG:   u32 = 0xFF40FF90; // green for raw register values
const C_FIELD: u32 = 0xFFFFAA00; // orange for decoded field names/values
const C_WARN:  u32 = 0xFFFFFF44; // yellow for the final "halted" line

// ── Framebuffer state ─────────────────────────────────────────────────────────

static mut HV_BASE:   *mut u32 = core::ptr::null_mut();
static mut HV_PITCH:  u32 = 0;
static mut HV_WIDTH:  u32 = 0;
static mut HV_HEIGHT: u32 = 0;

/// Store the mailbox-allocated framebuffer so the fault handler can find it.
/// Must be called from kernel_main after framebuffer::init() succeeds,
/// and before ERET'ing to Linux.
pub fn init(base: *mut u32, pitch: u32, width: u32, height: u32) {
    unsafe {
        HV_BASE   = base;
        HV_PITCH  = pitch;
        HV_WIDTH  = width;
        HV_HEIGHT = height;
    }
}

/// Return the framebuffer base physical address (for stage-2 protection).
pub fn base_addr() -> usize {
    unsafe { HV_BASE as usize }
}

// ── Fault entry point — called directly from assembly ────────────────────────

/// Draw a full-screen crash report and halt.
///
/// Registers on entry (AAPCS64):
///   x0 = ESR_EL2   — syndrome
///   x1 = ELR_EL2   — faulting PC (lower EL)
///   x2 = FAR_EL2   — faulting virtual address
///   x3 = HPFAR_EL2 — faulting IPA[47:12] in bits[39:4] (stage-2 faults)
///
/// This function never returns.
#[no_mangle]
pub extern "C" fn hv_fault_screen(esr: u64, elr: u64, far: u64, hpfar: u64) -> ! {
    unsafe {
        if HV_BASE.is_null() {
            loop { core::arch::asm!("wfe", options(nomem, nostack)); }
        }

        // Paint background before anything else so partial Linux output is wiped
        fill(C_BG);

        let mut col: u32;
        let mut row: u32 = 0;

        // ── Title ──────────────────────────────────────────────────────────
        col = 1;
        put_str("  EL2 HYPERVISOR FAULT  ", &mut col, &mut row, C_TITLE);
        row += 1; col = 0;
        put_str("================================================", &mut col, &mut row, C_FG);

        // ── ESR_EL2 ────────────────────────────────────────────────────────
        let ec  = ((esr >> 26) & 0x3F) as u8;
        let iss =   esr        & 0x00FF_FFFF;

        row += 1; col = 0;
        put_str("ESR_EL2  : 0x", &mut col, &mut row, C_FG);
        put_hex(esr, 16, &mut col, &mut row, C_REG);

        row += 1; col = 2;
        put_str("EC  = 0x", &mut col, &mut row, C_FG);
        put_hex(ec as u64, 2, &mut col, &mut row, C_FIELD);
        put_str("  ", &mut col, &mut row, C_FG);
        put_str(decode_ec(ec), &mut col, &mut row, C_FIELD);

        row += 1; col = 2;
        put_str("ISS = 0x", &mut col, &mut row, C_FG);
        put_hex(iss, 6, &mut col, &mut row, C_FIELD);

        // Extra ISS decode for data/instruction aborts
        if ec == 0x24 || ec == 0x25 {
            let dfsc = (iss & 0x3F) as u8;
            let wnr  = (iss >> 6) & 1;
            row += 1; col = 4;
            put_str("DFSC=0x", &mut col, &mut row, C_FG);
            put_hex(dfsc as u64, 2, &mut col, &mut row, C_FIELD);
            put_str("  ", &mut col, &mut row, C_FG);
            put_str(decode_fsc(dfsc), &mut col, &mut row, C_FIELD);
            put_str(if wnr != 0 { "  WRITE" } else { "  READ" }, &mut col, &mut row, C_FIELD);
        } else if ec == 0x20 || ec == 0x21 {
            let ifsc = (iss & 0x3F) as u8;
            row += 1; col = 4;
            put_str("IFSC=0x", &mut col, &mut row, C_FG);
            put_hex(ifsc as u64, 2, &mut col, &mut row, C_FIELD);
            put_str("  ", &mut col, &mut row, C_FG);
            put_str(decode_fsc(ifsc), &mut col, &mut row, C_FIELD);
        }

        // ── ELR_EL2 ────────────────────────────────────────────────────────
        row += 1; col = 0;
        put_str("ELR_EL2  : 0x", &mut col, &mut row, C_FG);
        put_hex(elr, 16, &mut col, &mut row, C_REG);
        put_str("  <- faulting PC", &mut col, &mut row, C_FG);

        // ── FAR_EL2 ────────────────────────────────────────────────────────
        row += 1; col = 0;
        put_str("FAR_EL2  : 0x", &mut col, &mut row, C_FG);
        put_hex(far, 16, &mut col, &mut row, C_REG);
        put_str("  <- virtual address", &mut col, &mut row, C_FG);

        // ── HPFAR_EL2 — IPA of the faulting stage-2 access ────────────────
        // HPFAR[39:4] = IPA[47:12]; shift left 12 to recover the IPA base.
        let ipa = (hpfar >> 4) << 12;
        row += 1; col = 0;
        put_str("HPFAR_EL2: 0x", &mut col, &mut row, C_FG);
        put_hex(hpfar, 16, &mut col, &mut row, C_REG);
        put_str("  IPA=0x", &mut col, &mut row, C_FG);
        put_hex(ipa, 16, &mut col, &mut row, C_REG);

        // ── IPA hint ───────────────────────────────────────────────────────
        let hint: &str = if ipa == 0 {
            "  (IPA=0 likely not a stage-2 fault)"
        } else if ipa < 0x8_0000 {
            "  below kernel (0x80000) — null ptr / early boot"
        } else if ipa >= 0xFC00_0000 {
            "  device region — bad peripheral access"
        } else if ipa >= 0x3B40_0000 && ipa < 0x3C00_0000 {
            "  EL2 page-table pool — stage-2 corruption!"
        } else {
            ""
        };
        if !hint.is_empty() {
            row += 1; col = 2;
            put_str(hint, &mut col, &mut row, C_FIELD);
        }

        // ── Footer ─────────────────────────────────────────────────────────
        row += 1; col = 0;
        put_str("------------------------------------------------", &mut col, &mut row, C_FG);
        row += 1; col = 0;
        put_str("Linux crashed at EL1.  CPU halted.", &mut col, &mut row, C_WARN);
        row += 1; col = 0;
        put_str("Power-cycle or reset to reboot.", &mut col, &mut row, C_WARN);

        loop { core::arch::asm!("wfe", options(nomem, nostack)); }
    }
}

// ── Syndrome decoders ─────────────────────────────────────────────────────────

fn decode_ec(ec: u8) -> &'static str {
    match ec {
        0x00 => "Unknown",
        0x01 => "WFI/WFE trap",
        0x07 => "SVE/SIMD/FP access trap",
        0x0E => "Illegal execution state",
        0x11 => "SVC from AArch32",
        0x12 => "HVC from AArch32",
        0x13 => "SMC from AArch32",
        0x15 => "SVC from AArch64",
        0x16 => "HVC from AArch64",
        0x17 => "SMC from AArch64",
        0x18 => "MSR/MRS/System instruction trap",
        0x20 => "Instruction abort (lower EL)",
        0x21 => "Instruction abort (same EL)",
        0x22 => "PC alignment fault",
        0x24 => "Data abort (lower EL)",
        0x25 => "Data abort (same EL)",
        0x26 => "SP alignment fault",
        0x2C => "FP exception (AArch64)",
        0x2F => "SError interrupt",
        0x30 => "Breakpoint (lower EL)",
        0x31 => "Breakpoint (same EL)",
        0x32 => "Software step (lower EL)",
        0x33 => "Software step (same EL)",
        0x34 => "Watchpoint (lower EL)",
        0x35 => "Watchpoint (same EL)",
        0x3C => "BRK instruction",
        _    => "Reserved",
    }
}

fn decode_fsc(fsc: u8) -> &'static str {
    match fsc {
        0x00 => "Address size fault L0",
        0x01 => "Address size fault L1",
        0x02 => "Address size fault L2",
        0x03 => "Address size fault L3",
        0x04 => "Translation fault L0",
        0x05 => "Translation fault L1",
        0x06 => "Translation fault L2",
        0x07 => "Translation fault L3",
        0x08 => "Access flag fault L0",
        0x09 => "Access flag fault L1",
        0x0A => "Access flag fault L2",
        0x0B => "Access flag fault L3",
        0x0C => "Permission fault L0",
        0x0D => "Permission fault L1",
        0x0E => "Permission fault L2",
        0x0F => "Permission fault L3",
        0x10 => "Synchronous external abort",
        0x14 => "Sync ext abort on table walk L0",
        0x15 => "Sync ext abort on table walk L1",
        0x16 => "Sync ext abort on table walk L2",
        0x17 => "Sync ext abort on table walk L3",
        0x21 => "Alignment fault",
        0x30 => "TLB conflict abort",
        _    => "Reserved FSC",
    }
}

// ── Rendering primitives ──────────────────────────────────────────────────────

unsafe fn fill(color: u32) {
    let words = (HV_PITCH / 4) * HV_HEIGHT;
    for i in 0..words as usize {
        HV_BASE.add(i).write_volatile(color);
    }
}

unsafe fn draw_char(c: u8, col: u32, row: u32, color: u32) {
    let idx = if c >= 0x20 && c <= 0x7F { (c - 0x20) as usize } else { 0 };
    let glyph = &FONT[idx];
    let pw = HV_PITCH / 4;
    let ox = col * CHAR_W;
    let oy = row * CHAR_H;
    for gy in 0..FONT_H {
        let byte = glyph[gy as usize];
        for gx in 0..FONT_W {
            let px = if byte & (0x80 >> gx) != 0 { color } else { C_BG };
            for sy in 0..SCALE {
                for sx in 0..SCALE {
                    let x = ox + gx * SCALE + sx;
                    let y = oy + gy * SCALE + sy;
                    if x < HV_WIDTH && y < HV_HEIGHT {
                        HV_BASE.add((y * pw + x) as usize).write_volatile(px);
                    }
                }
            }
        }
    }
}

unsafe fn put_str(s: &str, col: &mut u32, row: &mut u32, color: u32) {
    let max_col = HV_WIDTH / CHAR_W;
    let max_row = HV_HEIGHT / CHAR_H;
    for b in s.bytes() {
        if *col >= max_col { *col = 0; *row += 1; }
        if *row >= max_row { return; }
        draw_char(b, *col, *row, color);
        *col += 1;
    }
}

/// Print `digits` hex digits of `v` (most-significant first).
unsafe fn put_hex(v: u64, digits: u32, col: &mut u32, row: &mut u32, color: u32) {
    let shift_start = (digits - 1) * 4;
    for i in 0..digits {
        let nibble = ((v >> ((shift_start - i * 4) as u64)) & 0xF) as u8;
        let ch = if nibble < 10 { b'0' + nibble } else { b'A' + nibble - 10 };
        draw_char(ch, *col, *row, color);
        *col += 1;
    }
}
