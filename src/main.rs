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

/// Release core 1 to run Linux at EL1 using the official armstub8.S spin table.
///
/// The Pi 4 start4.elf firmware embeds armstub8.S at physical address 0x0.
/// That stub parks cores 1-3 in a WFE loop reading from:
///   Core 1: physical 0xE0  (spin_cpu1)
///   Core 2: physical 0xE8  (spin_cpu2)
///   Core 3: physical 0xF0  (spin_cpu3)
///
/// The stub's secondary_spin loop is:
///   adr x5, spin_cpu0       // x5 = 0xD8
///   wfe
///   ldr x4, [x5, x6, lsl #3]  // x4 = *(0xD8 + core_id*8)
///   cbz x4, secondary_spin
///   mov x0, #0
///   b boot_kernel           // br x4 with x0=0, x1=x2=x3=0
///
/// So we write TRAMPOLINE_ADDR to 0xE0, flush the D-cache line covering
/// 0xE0 (so core 1 sees the new value), then send SEV to wake it.
/// Core 1 will jump to our trampoline at EL2 with x0=0.
/// The trampoline sets HCR_EL2, SPSR_EL2, ELR_EL2 and ERETSs to Linux at EL1.
fn launch_linux_on_core1(dtb_addr: usize) {
    // Trampoline lives at 1MB — well above our kernel (~57KB) and below Linux (0x80000+)
    const TRAMPOLINE_ADDR: usize = 0x0010_0000;
    // armstub8.S spin_cpu1 is at physical 0xE0
    const SPIN_CPU1: usize = 0xE0;

    // HCR_EL2: RW=1 (AArch64 EL1), plus VM/SWIO/PTW/FMO/IMO/AMO
    let hcr_value:  u64 = (1u64 << 31) | (1 << 5) | (1 << 4) | (1 << 3) | (1 << 2) | (1 << 1) | 1;
    // SPSR_EL2: EL1h (0x5), DAIF all masked (bits 9:6 = 0b1111 = 0x3C0) => 0x3C5
    let spsr_value: u64 = 0x3C5;

    // ── Trampoline layout at TRAMPOLINE_ADDR ─────────────────────────────────
    // The armstub delivers core 1 here at EL2 with x0=0, x1=x2=x3=0.
    // We must set up HCR_EL2/SPSR_EL2/ELR_EL2 then ERET to Linux at EL1.
    //
    // Byte offsets:
    //   [0..51]  13 x u32 instructions
    //   [52]     nop (pad to 8-byte align)
    //   [56]     dtb_addr    (u64)  — LDR PC-relative offset from instr[0]
    //   [64]     linux_entry (u64)
    //   [72]     hcr_value   (u64)
    //   [80]     spsr_value  (u64)
    //
    // LDR literal encoding: 0x58000000 | (imm19 << 5) | Rt
    //   imm19 = (data_byte_offset - instr_byte_offset) / 4
    //   instr[0] @ byte 0,  data @ byte 56: imm19 = 56/4 = 14 → 0x580001C9 (x9)
    //   instr[1] @ byte 4,  data @ byte 64: imm19 = 60/4 = 15 → 0x580001EA (x10)
    //   instr[2] @ byte 8,  data @ byte 72: imm19 = 64/4 = 16 → 0x5800020B (x11)
    //   instr[3] @ byte 12, data @ byte 80: imm19 = 68/4 = 17 → 0x5800022C (x12)
    //
    // MSR encodings (from ARM ARM, confirmed correct):
    //   msr spsr_el2, x12 = 0xD51C400C
    //   msr elr_el2,  x10 = 0xD51C402A
    //   msr hcr_el2,  x11 = 0xD5110C0B  ← S3_4_C1_C1_0
    //
    // mov x0, x9  = 0xAA0903E0
    // mov x1, xzr = 0xAA1F03E1  (Linux boot protocol: x1=x2=x3=0)
    // mov x2, xzr = 0xAA1F03E2
    // mov x3, xzr = 0xAA1F03E3
    let trampoline_code: [u32; 13] = [
        0x580001C9u32, // [0]  ldr x9,  [pc, #56]  ; dtb_addr
        0x580001EAu32, // [4]  ldr x10, [pc, #60]  ; linux_entry
        0x5800020Bu32, // [8]  ldr x11, [pc, #64]  ; hcr_value
        0x5800022Cu32, // [12] ldr x12, [pc, #68]  ; spsr_value
        0xAA0903E0u32, // [16] mov x0, x9          ; x0 = dtb_addr (Linux ABI)
        0xAA1F03E1u32, // [20] mov x1, xzr
        0xAA1F03E2u32, // [24] mov x2, xzr
        0xAA1F03E3u32, // [28] mov x3, xzr
        0xD51C400Cu32, // [32] msr spsr_el2, x12
        0xD51C402Au32, // [36] msr elr_el2,  x10
        0xD51C110Bu32, // [40] msr hcr_el2,  x11  (S3_4_C1_C1_0 = 0xD51C110B)
        0xD5033FDFu32, // [44] isb
        0xD69F03E0u32, // [48] eret
    ];

    unsafe {
        // 1. Write trampoline instructions to TRAMPOLINE_ADDR
        let t = TRAMPOLINE_ADDR as *mut u32;
        for (i, &word) in trampoline_code.iter().enumerate() {
            t.add(i).write_volatile(word);
        }
        t.add(13).write_volatile(0xD503201Fu32); // nop padding at byte 52

        // 2. Write data literals (u64 array, base = TRAMPOLINE_ADDR)
        let data = TRAMPOLINE_ADDR as *mut u64;
        data.add(7).write_volatile(dtb_addr as u64);                   // byte 56
        data.add(8).write_volatile(linux_vm::LINUX_LOAD_ADDR as u64); // byte 64
        data.add(9).write_volatile(hcr_value);                         // byte 72
        data.add(10).write_volatile(spsr_value);                       // byte 80

        // 3. Clean D-cache for the trampoline range so core 1 sees it,
        //    then invalidate I-cache so core 1 fetches fresh instructions.
        //    We flush 2 cache lines (64 bytes each) to cover 88 bytes of trampoline.
        // dc civac = clean AND invalidate to point of coherency.
        // This is required on Cortex-A72 because each core has its own L1
        // data cache. cvac only cleans (writes back) but doesn't invalidate
        // other cores' caches. civac does both, ensuring core 1 sees our writes.
        core::arch::asm!(
            "dc civac, {a0}",
            "dc civac, {a1}",
            "dsb sy",
            "ic ialluis",
            "dsb sy",
            "isb",
            a0 = in(reg) TRAMPOLINE_ADDR,
            a1 = in(reg) TRAMPOLINE_ADDR + 64,
        );

        // 4. Write TRAMPOLINE_ADDR to the armstub spin table entry for core 1.
        //    Physical address 0xE0 (spin_cpu1 in armstub8.S).
        //    Use dc civac to ensure core 1 sees the new value.
        let spin = SPIN_CPU1 as *mut u64;
        spin.write_volatile(TRAMPOLINE_ADDR as u64);
        core::arch::asm!(
            "dc civac, {addr}",
            "dsb sy",
            "sev",
            addr = in(reg) SPIN_CPU1,
        );

        crate::kprintln!("[kernel] Core 1: spin table written, SEV sent");
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
