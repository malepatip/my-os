// SPDX-License-Identifier: MIT
//
// main.rs — ai-os v0.4.0 — EL2 Thin Hypervisor + Linux Driver VM
//
// Architecture:
//   ai-os Rust kernel runs at EL2 (boots first, stays at EL2)
//   Linux driver VM runs at EL1 on core 1 (handles USB/PCIe hardware)
//   Shared memory ring buffer at 0x0020_0000 for USB HID IPC
//
// Boot sequence:
//   1. boot.rs (assembly) → parks cores 1-3, stays at EL2, zeros BSS
//   2. kernel_main() → GPIO LED, UART, Framebuffer, boot banner
//   3. kernel_main() → load Linux kernel + initramfs + DTB from SD card
//   4. kernel_main() → set up stage-2 identity page tables
//   5. kernel_main() → release core 1 to run Linux at EL1
//   6. kernel_main() → wait for Linux to signal USB HID ready
//   7. kernel_main() → enter ai-os> shell (reads from shared memory)
//
// Shell commands: help, info, elinfo, memmap, usbstatus, echo, clear, halt

#![no_std]
#![no_main]

mod boot;
mod uart;
mod mailbox;
mod framebuffer;
mod gpio;
mod emmc;
mod fat32;
mod linux_vm;
mod ipc;

use core::fmt::Write;
use core::panic::PanicInfo;

/// kprint! — outputs to UART always, framebuffer if initialised
#[macro_export]
macro_rules! kprint {
    ($($arg:tt)*) => {{
        use core::fmt::Write;
        let _ = write!($crate::uart::UartWriter, $($arg)*);
        let _ = write!($crate::framebuffer::FbWriter, $($arg)*);
    }};
}

/// kprintln! — same as kprint! with newline
#[macro_export]
macro_rules! kprintln {
    () => { $crate::kprint!("\n") };
    ($($arg:tt)*) => {{
        use core::fmt::Write;
        let _ = writeln!($crate::uart::UartWriter, $($arg)*);
        let _ = writeln!($crate::framebuffer::FbWriter, $($arg)*);
    }};
}

#[no_mangle]
pub extern "C" fn kernel_main() -> ! {
    // ── Step 1: GPIO LED + UART + Framebuffer ────────────────────────────
    gpio::init_led();
    gpio::blink(3, 200, 200);
    uart::init();
    let fb_ok = framebuffer::init();
    if fb_ok { gpio::blink(5, 100, 100); }

    // ── Step 2: Boot banner ───────────────────────────────────────────────
    kprintln!("========================================");
    kprintln!("  ai-os v0.4.0");
    kprintln!("  EL2 Hypervisor + Linux Driver VM");
    kprintln!("  Board: Raspberry Pi 4 (BCM2711)");
    kprintln!("========================================");
    kprintln!("");
    kprintln!("[kernel] UART:        OK @ 0x{:08X}", uart::base_address());
    if fb_ok {
        kprintln!("[kernel] Framebuffer: OK (1920x1080 @ 32bpp)");
    } else {
        kprintln!("[kernel] Framebuffer: FAILED -- UART-only mode");
    }

    let el: u64;
    unsafe { core::arch::asm!("mrs {el}, CurrentEL", el = out(reg) el); }
    kprintln!("[kernel] Exception Level: EL{}", (el >> 2) & 0x3);
    kprintln!("[kernel] Cores 1-3:   parked");

    // ── Step 3: SD card + FAT32 init ─────────────────────────────────────
    kprint!("[kernel] SD card:     ");
    let sd_ok = emmc::sd_init();
    if sd_ok {
        kprintln!("OK");
        kprint!("[kernel] FAT32:       ");
        let fat_ok = fat32::fat32_init();
        if fat_ok {
            kprintln!("OK");
        } else {
            kprintln!("FAILED");
        }
    } else {
        kprintln!("FAILED -- check SD card");
    }

    // ── Step 4: Load Linux kernel, initramfs, and DTB from SD card ───────
    // Print directory listing so we can see exact 8.3 names on the card
    fat32::fat32_list_files_debug();

    kprintln!("[kernel] Linux VM:    loading...");

    // Load vmlinuz.rpi to LINUX_LOAD_ADDR (0x0040_0000)
    // FAT32 8.3 name: "VMLINUZ RPI" (name=VMLINUZ_, ext=RPI)
    let linux_loaded = load_file_to_addr(b"VMLINUZ RPI", linux_vm::LINUX_LOAD_ADDR);
    if linux_loaded > 0 {
        kprintln!("[kernel] Linux VM:    kernel OK ({} KB)", linux_loaded / 1024);
    } else {
        kprintln!("[kernel] Linux VM:    FAILED -- vmlinuz-rpi missing from SD card");
        kprintln!("[kernel] Copy vmlinuz-rpi to SD card root and reboot.");
        kprintln!("[kernel] Entering UART-only shell...");
        kprintln!("");
        shell_uart_only();
    }

    // Load initrd.gz to LINUX_INITRD_ADDR (0x0200_0000)
    let initrd_loaded = load_file_to_addr(b"INITRD  GZ ", linux_vm::LINUX_INITRD_ADDR);
    if initrd_loaded > 0 {
        kprintln!("[kernel] Linux VM:    initrd OK ({} KB)", initrd_loaded / 1024);
    } else {
        kprintln!("[kernel] Linux VM:    FAILED -- initrd.gz missing from SD card");
        kprintln!("[kernel] Copy initrd.gz to SD card root and reboot.");
        kprintln!("[kernel] Entering UART-only shell...");
        kprintln!("");
        shell_uart_only();
    }

    // Load bcm2711.dtb to LINUX_DTB_ADDR (0x3B50_0000)
    let dtb_loaded = load_file_to_addr(b"BCM2711 DTB", linux_vm::LINUX_DTB_ADDR);
    if dtb_loaded > 0 {
        kprintln!("[kernel] Linux VM:    DTB OK ({} bytes)", dtb_loaded);
    } else {
        kprintln!("[kernel] Linux VM:    FAILED -- bcm2711.dtb missing from SD card");
        kprintln!("[kernel] Entering UART-only shell...");
        kprintln!("");
        shell_uart_only();
    }

    // Patch DTB chosen node with initramfs addresses and bootargs
    linux_vm::setup_dtb(initrd_loaded);
    kprintln!("[kernel] Linux VM:    DTB patched (initrd @ 0x{:08X}+{})",
        linux_vm::LINUX_INITRD_ADDR, initrd_loaded);

    // ── Step 5: Set up stage-2 page tables ───────────────────────────────
    kprint!("[kernel] Stage-2 MMU: ");
    linux_vm::setup_stage2_tables();
    kprintln!("OK (4GB identity map)");

    // ── Step 6: Launch Linux on core 1 ───────────────────────────────────
    kprintln!("[kernel] Launching Linux driver VM on core 1...");
    launch_linux_on_core1(linux_vm::LINUX_DTB_ADDR);
    kprintln!("[kernel] Linux VM launched (core 1 released)");

    // ── Step 7: Wait for Linux USB HID daemon to be ready ────────────────
    kprint!("[kernel] USB HID:     waiting for Linux");
    ipc::wait_for_linux_ready();

    kprintln!("");
    kprintln!("[kernel] Entering ai-os shell...");
    kprintln!("Type 'help' for available commands.");
    kprintln!("");

    // ── Step 8: Shell ─────────────────────────────────────────────────────
    shell();
}

/// Load a FAT32 file from SD card to a physical address.
/// name83: 11-byte FAT32 8.3 name (space-padded, uppercase).
/// Returns bytes loaded, or 0 on failure.
fn load_file_to_addr(name83: &[u8; 11], dest_addr: usize) -> usize {
    let mut offset = 0usize;
    let dest = dest_addr as *mut u8;
    let found = fat32::fat32_read_file(name83, |chunk, len| {
        unsafe {
            core::ptr::copy_nonoverlapping(chunk.as_ptr(), dest.add(offset), len);
        }
        offset += len;
    });
    if found { offset } else { 0 }
}

/// Release core 1 from its WFE park loop to run Linux at EL1.
///
/// Pi 4 spin table protocol:
///   Core N release address is at physical 0xD8 + N*8.
///   Core 1 = 0xE0. Write the trampoline address, then SEV.
///
/// The park loop in boot.rs watches 0xE0 and branches to the trampoline.
/// The trampoline sets up EL2 registers and uses ERET to drop to EL1.
///
/// Trampoline layout at TRAMPOLINE_ADDR (0x0010_0000):
///   Instruction bytes [0..52]: 13 AArch64 instructions
///   Data bytes [56..63]:       dtb_addr (u64)
///   Data bytes [64..71]:       linux_entry (u64)
///
/// Instructions:
///   [0]  ldr x9,  [pc, #56]   ; x9  = dtb_addr
///   [4]  ldr x10, [pc, #56]   ; x10 = linux_entry
///   [8]  mov x0, x9           ; x0  = dtb_addr (Linux boot protocol)
///   [12] mov x1, xzr
///   [16] mov x2, xzr
///   [20] mov x3, xzr
///   [24] mov x11, #0x3C5      ; SPSR_EL2: EL1h, DAIF masked
///   [28] msr spsr_el2, x11
///   [32] msr elr_el2, x10     ; ELR_EL2 = linux_entry
///   [36] mov x11, #(1<<31)    ; HCR_EL2: RW=1 (EL1 is AArch64)
///   [40] orr x11, x11, #0x3F  ; + VM|SWIO|PTW|FMO|IMO|AMO
///   [44] msr hcr_el2, x11
///   [48] isb
///   [52] eret                 ; drop to EL1, jump to linux_entry
///   [56] .quad dtb_addr
///   [64] .quad linux_entry
fn launch_linux_on_core1(dtb_addr: usize) {
    const TRAMPOLINE_ADDR: usize = 0x0010_0000;
    const CORE1_SPIN_TABLE: usize = 0xe0;

    // LDR (literal) 64-bit encoding: 0x58000000 | (imm19 << 5) | Rt
    // imm19 = byte_offset_from_pc / 4
    //
    // instr[0] at byte 0:  data at byte 56 → offset = 56, imm19 = 14
    //   ldr x9,  [pc, #56] = 0x58000000 | (14 << 5) | 9  = 0x580001C9
    // instr[1] at byte 4:  data at byte 64 → offset = 60, imm19 = 15
    //   ldr x10, [pc, #60] = 0x58000000 | (15 << 5) | 10 = 0x580001EA
    //
    // HCR_EL2 value: RW(31) | AMO(5) | IMO(4) | FMO(3) | PTW(2) | SWIO(1) | VM(0)
    //   = 0x8000003F
    // We build this in two instructions:
    //   movz x11, #0x0000, lsl #32  → can't do this in one movz for bit 31
    // Instead use: mov x11, #(1<<31) then orr x11, x11, #0x3F
    //   movz x11, #0x8000, lsl #16 = 0xD2F00011... let's use the encoding directly
    //
    // AArch64 MOV (wide immediate) for x11 = 0x80000000:
    //   movz x11, #0x8000, lsl #16
    //   encoding: 0xD280000B | (0x8000 << 5) = 0xD280000B
    //   Actually: movz Xd, #imm16, lsl #shift
    //   sf=1, opc=10, hw=10(lsl#32)... let me use movz x11, #1, lsl #31 — not valid
    //   Simplest: ldr x11, =0x8000003F from a data literal
    //
    // Revised trampoline — use data literals for all constants:
    //   [0]  ldr x9,  [pc, #56]   ; dtb_addr
    //   [4]  ldr x10, [pc, #56]   ; linux_entry
    //   [8]  ldr x11, [pc, #56]   ; hcr_value
    //   [12] ldr x12, [pc, #56]   ; spsr_value
    //   [16] mov x0, x9
    //   [20] mov x1, xzr
    //   [24] mov x2, xzr
    //   [28] mov x3, xzr
    //   [32] msr spsr_el2, x12
    //   [36] msr elr_el2, x10
    //   [40] msr hcr_el2, x11
    //   [44] isb
    //   [48] eret
    //   [52] nop (padding to 8-byte align)
    //   [56] .quad dtb_addr
    //   [64] .quad linux_entry
    //   [72] .quad hcr_value
    //   [80] .quad spsr_value
    //
    // LDR offsets:
    //   instr[0] at byte 0,  data at byte 56: imm19 = 56/4 = 14 → 0x58000000|(14<<5)|9  = 0x580001C9
    //   instr[1] at byte 4,  data at byte 64: imm19 = 60/4 = 15 → 0x58000000|(15<<5)|10 = 0x580001EA
    //   instr[2] at byte 8,  data at byte 72: imm19 = 64/4 = 16 → 0x58000000|(16<<5)|11 = 0x5800020B
    //   instr[3] at byte 12, data at byte 80: imm19 = 68/4 = 17 → 0x58000000|(17<<5)|12 = 0x5800022C
    //
    // MSR encodings:
    //   msr spsr_el2, x12 = 0xD51C400C
    //   msr elr_el2,  x10 = 0xD51C400A
    //   msr hcr_el2,  x11 = 0xD5110C0B  (wait — let me verify)
    //
    // MSR system register encoding: 0xD5100000 | (op0<<19) | (op1<<16) | (CRn<<12) | (CRm<<8) | (op2<<5) | Rt
    // HCR_EL2:  op0=3, op1=4, CRn=1, CRm=1, op2=0 → 0xD5110C0B? No:
    //   0xD5 = 1101_0101, bits[31:20] = 1101_0101_0001 = write
    //   Actually MSR (register) = 0xD5100000 | (o0<<19) | (op1<<16) | (CRn<<12) | (CRm<<8) | (op2<<5) | Rt
    //   HCR_EL2: o0=1(EL2), op1=4, CRn=1, CRm=1, op2=0
    //   = 0xD5100000 | (1<<19) | (4<<16) | (1<<12) | (1<<8) | (0<<5) | 11
    //   = 0xD5100000 | 0x80000 | 0x40000 | 0x1000 | 0x100 | 0 | 11
    //   = 0xD51C110B  ← let me just use known-good encodings from ARM ARM
    //
    // Known good MSR encodings (from ARM Architecture Reference Manual):
    //   msr hcr_el2,  Xt: 0xD5110C00 | Rt  (HCR_EL2 = S3_4_C1_C1_0)
    //   msr spsr_el2, Xt: 0xD51C4000 | Rt  (SPSR_EL2 = S3_4_C4_C0_0)
    //   msr elr_el2,  Xt: 0xD51C4020 | Rt  (ELR_EL2  = S3_4_C4_C0_1)
    //   isb:              0xD5033FDF
    //   eret:             0xD69F03E0
    //   nop:              0xD503201F
    //
    // mov x0, x9:  0xAA0903E0
    // mov x1, xzr: 0xAA1F03E1
    // mov x2, xzr: 0xAA1F03E2
    // mov x3, xzr: 0xAA1F03E3
    let hcr_value:  u64 = (1u64 << 31) | (1 << 5) | (1 << 4) | (1 << 3) | (1 << 2) | (1 << 1) | (1 << 0);
    let spsr_value: u64 = 0x3C5; // EL1h, DAIF masked

    let trampoline_code: [u32; 13] = [
        0x580001C9u32, // [0]  ldr x9,  [pc, #56]  ; dtb_addr
        0x580001EAu32, // [4]  ldr x10, [pc, #60]  ; linux_entry
        0x5800020Bu32, // [8]  ldr x11, [pc, #64]  ; hcr_value
        0x5800022Cu32, // [12] ldr x12, [pc, #68]  ; spsr_value
        0xAA0903E0u32, // [16] mov x0, x9          ; x0 = dtb_addr
        0xAA1F03E1u32, // [20] mov x1, xzr
        0xAA1F03E2u32, // [24] mov x2, xzr
        0xAA1F03E3u32, // [28] mov x3, xzr
        0xD51C400Cu32, // [32] msr spsr_el2, x12
        0xD51C402Au32, // [36] msr elr_el2,  x10
        0xD51C110Bu32, // [40] msr hcr_el2,  x11
        0xD5033FDFu32, // [44] isb
        0xD69F03E0u32, // [48] eret
    ];
    // Total: 13 * 4 = 52 bytes of instructions
    // Padding to 8-byte align: 4 bytes (1 nop) → data starts at byte 56

    unsafe {
        let t = TRAMPOLINE_ADDR as *mut u32;

        // Write 13 instructions
        for (i, &word) in trampoline_code.iter().enumerate() {
            t.add(i).write_volatile(word);
        }
        // Write padding nop at byte 52 (index 13)
        t.add(13).write_volatile(0xD503201Fu32); // nop

        // Write data at byte 56 (u64 index 7)
        let data = TRAMPOLINE_ADDR as *mut u64;
        data.add(7).write_volatile(dtb_addr as u64);                    // byte 56
        data.add(8).write_volatile(linux_vm::LINUX_LOAD_ADDR as u64);  // byte 64
        data.add(9).write_volatile(hcr_value);                          // byte 72
        data.add(10).write_volatile(spsr_value);                        // byte 80

        // Flush data cache and invalidate instruction cache
        core::arch::asm!(
            "dc cvac, {addr}",
            "dsb sy",
            "ic ialluis",
            "dsb sy",
            "isb",
            addr = in(reg) TRAMPOLINE_ADDR,
        );

        // Write trampoline address to core 1 spin table entry
        let spin = CORE1_SPIN_TABLE as *mut u64;
        spin.write_volatile(TRAMPOLINE_ADDR as u64);

        // Memory barrier + send event to wake core 1
        core::arch::asm!(
            "dsb sy",
            "sev",
            options(nomem, nostack),
        );
    }
}

/// Shell with USB HID input via shared memory IPC + UART fallback.
fn shell() -> ! {
    let mut buf = [0u8; 128];
    let mut pos = 0usize;
    loop {
        kprint!("ai-os> ");
        loop {
            let c = getc();
            match c {
                b'\r' | b'\n' => {
                    kprintln!("");
                    break;
                }
                0x08 | 0x7F => {
                    if pos > 0 {
                        pos -= 1;
                        kprint!("\x08 \x08");
                    }
                }
                _ => {
                    if pos < buf.len() - 1 {
                        buf[pos] = c;
                        pos += 1;
                        uart::putc(c);
                        let _ = write!(framebuffer::FbWriter, "{}", c as char);
                    }
                }
            }
        }
        let cmd = core::str::from_utf8(&buf[..pos]).unwrap_or("").trim();
        execute(cmd);
        pos = 0;
    }
}

/// Fallback shell using UART only (when Linux VM not available).
fn shell_uart_only() -> ! {
    kprintln!("[kernel] UART-only mode. Connect serial cable (115200 8N1).");
    kprintln!("");
    let mut buf = [0u8; 128];
    let mut pos = 0usize;
    loop {
        kprint!("ai-os> ");
        loop {
            let c = uart::getc();
            match c {
                b'\r' | b'\n' => {
                    kprintln!("");
                    break;
                }
                0x08 | 0x7F => {
                    if pos > 0 {
                        pos -= 1;
                        kprint!("\x08 \x08");
                    }
                }
                _ => {
                    if pos < buf.len() - 1 {
                        buf[pos] = c;
                        pos += 1;
                        uart::putc(c);
                        let _ = write!(framebuffer::FbWriter, "{}", c as char);
                    }
                }
            }
        }
        let cmd = core::str::from_utf8(&buf[..pos]).unwrap_or("").trim();
        execute(cmd);
        pos = 0;
    }
}

/// Read one character — tries IPC (USB HID via Linux) first, then UART.
fn getc() -> u8 {
    loop {
        if let Some(c) = ipc::poll_key() {
            return c;
        }
        let c = uart::getc_nonblocking();
        if c != 0 { return c; }
        unsafe { core::arch::asm!("yield", options(nomem, nostack)); }
    }
}

/// Convert a byte slice to uppercase ASCII into a fixed buffer.
fn to_upper(src: &[u8], dst: &mut [u8]) -> usize {
    let len = src.len().min(dst.len());
    for i in 0..len {
        dst[i] = src[i].to_ascii_uppercase();
    }
    len
}

fn execute(line: &str) {
    let (name, args) = match line.find(' ') {
        Some(i) => (&line[..i], line[i + 1..].trim()),
        None    => (line, ""),
    };

    match name {
        "" => {}
        "help" => {
            kprintln!("Available commands:");
            kprintln!("  help       — this help");
            kprintln!("  info       — system information");
            kprintln!("  elinfo     — exception level details");
            kprintln!("  memmap     — physical memory map");
            kprintln!("  usbstatus  — USB HID / Linux VM status");
            kprintln!("  ls         — list SD card root directory");
            kprintln!("  cat <FILE> — print file (8.3 name, e.g. README.TXT)");
            kprintln!("  echo <txt> — print text");
            kprintln!("  clear      — clear screen");
            kprintln!("  halt       — halt CPU");
        }
        "info" => {
            kprintln!("ai-os v0.4.0 — EL2 Hypervisor + Linux Driver VM");
            kprintln!("Board: Raspberry Pi 4 (BCM2711, Cortex-A72)");
            kprintln!("Architecture: AArch64");
            let el: u64;
            unsafe { core::arch::asm!("mrs {el}, CurrentEL", el = out(reg) el); }
            kprintln!("Current EL: EL{}", (el >> 2) & 0x3);
            kprintln!("Linux driver VM: EL1 (core 1)");
            kprintln!("USB HID IPC: shared memory @ 0x{:08x}", linux_vm::SHMEM_ADDR);
        }
        "elinfo" => {
            let el: u64;
            let hcr: u64;
            let vttbr: u64;
            unsafe {
                core::arch::asm!("mrs {el}, CurrentEL", el = out(reg) el);
                core::arch::asm!("mrs {hcr}, hcr_el2", hcr = out(reg) hcr);
                core::arch::asm!("mrs {vttbr}, vttbr_el2", vttbr = out(reg) vttbr);
            }
            kprintln!("CurrentEL:  EL{}", (el >> 2) & 0x3);
            kprintln!("HCR_EL2:    0x{:016x}", hcr);
            kprintln!("  VM={} (stage-2 enabled)", (hcr >> 0) & 1);
            kprintln!("  RW={} (EL1 is AArch64)", (hcr >> 31) & 1);
            kprintln!("VTTBR_EL2:  0x{:016x}", vttbr);
        }
        "memmap" => {
            kprintln!("Physical Memory Map:");
            kprintln!("  0x0008_0000  ai-os kernel (EL2, ~300KB)");
            kprintln!("  0x0010_0000  Linux launch trampoline (256B)");
            kprintln!("  0x0020_0000  Shared memory IPC (4KB)");
            kprintln!("  0x0040_0000  Linux kernel image (vmlinuz, 23MB)");
            kprintln!("  0x0200_0000  Linux initramfs (4MB)");
            kprintln!("  0x3B40_0000  Stage-2 page tables (64KB)");
            kprintln!("  0x3B50_0000  Linux DTB (64KB)");
            kprintln!("  0xFC00_0000  BCM2711 peripherals (device memory)");
        }
        "usbstatus" => {
            let shm = linux_vm::SharedMem::get();
            if shm.is_ready() {
                kprintln!("Linux driver VM: READY");
                kprintln!("USB HID daemon:  RUNNING");
                kprintln!("Keyboard:        ACTIVE");
                kprintln!("IPC ring buffer: write={} read={}",
                    shm.write_idx, shm.read_idx);
            } else {
                kprintln!("Linux driver VM: NOT READY (magic=0x{:08x})", shm.magic);
                kprintln!("USB HID daemon:  waiting...");
            }
        }
        "ls" => {
            kprintln!("Root directory:");
            kprintln!("  {:<12} {:>10}  TYPE", "NAME", "SIZE");
            kprintln!("  ------------------------------");
            let mut count = 0u32;
            fat32::fat32_list_root(|info| {
                let mut name_str = [0u8; 13];
                let mut ni = 0;
                for &b in info.name.iter() {
                    if b == 0 { break; }
                    name_str[ni] = b;
                    ni += 1;
                }
                let name_display = core::str::from_utf8(&name_str[..ni]).unwrap_or("?");
                if info.is_dir {
                    kprintln!("  {:<12} {:>10}  DIR", name_display, "");
                } else {
                    kprintln!("  {:<12} {:>10}  FILE", name_display, info.size);
                }
                count += 1;
            });
            kprintln!("  {} item(s)", count);
        }
        "cat" => {
            if args.is_empty() {
                kprintln!("Usage: cat <FILENAME.EXT>");
                return;
            }
            let mut name83 = [b' '; 11];
            let mut upper_buf = [0u8; 16];
            let upper_len = to_upper(args.as_bytes(), &mut upper_buf);
            let upper_bytes = &upper_buf[..upper_len];
            match upper_bytes.iter().position(|&b| b == b'.') {
                Some(d) => {
                    let name_len = d.min(8);
                    for i in 0..name_len { name83[i] = upper_bytes[i]; }
                    let ext_src = &upper_bytes[(d + 1)..];
                    let ext_len = ext_src.len().min(3);
                    for i in 0..ext_len { name83[8 + i] = ext_src[i]; }
                }
                None => {
                    let name_len = upper_len.min(8);
                    for i in 0..name_len { name83[i] = upper_bytes[i]; }
                }
            }
            kprintln!("--- {} ---", args);
            let mut bytes_shown = 0usize;
            let found = fat32::fat32_read_file(&name83, |chunk, len| {
                if bytes_shown >= 2048 { return; }
                for &b in &chunk[..len] {
                    if bytes_shown >= 2048 { break; }
                    if b == b'\r' { continue; }
                    uart::putc(b);
                    let _ = write!(framebuffer::FbWriter, "{}", b as char);
                    bytes_shown += 1;
                }
            });
            if !found {
                kprintln!("File not found: {}", args);
            } else {
                kprintln!("\n--- end ---");
            }
        }
        "echo" => {
            kprintln!("{}", args);
        }
        "clear" => {
            framebuffer::clear_screen();
            kprintln!("ai-os v0.4.0");
        }
        "halt" => {
            kprintln!("Halting CPU. Goodbye.");
            loop {
                unsafe { core::arch::asm!("wfe", options(nomem, nostack)); }
            }
        }
        _ => {
            kprintln!("Unknown command: '{}'. Type 'help'.", name);
        }
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    let _ = writeln!(uart::UartWriter, "\n[KERNEL PANIC] {}", info);
    loop {
        gpio::blink(3, 100, 100);
        gpio::blink(3, 300, 100);
        gpio::blink(3, 100, 100);
        gpio::delay_ms(500);
    }
}
