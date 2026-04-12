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
    kprintln!("[kernel] Linux VM:    loading...");

    // Load vmlinuz-rpi to LINUX_LOAD_ADDR (0x0040_0000)
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
///   Core N release address is at physical 0xd8 + N*8.
///   Core 1 = 0xe0. Write the entry address, then SEV.
///
/// We place a small AArch64 trampoline at 0x0010_0000 that:
///   1. Sets x0 = DTB address (Linux boot protocol)
///   2. Clears x1, x2, x3
///   3. Jumps to LINUX_LOAD_ADDR
///
/// Then we write the trampoline address to the spin table and SEV.
fn launch_linux_on_core1(dtb_addr: usize) {
    const TRAMPOLINE_ADDR: usize = 0x0010_0000;
    const CORE1_SPIN_TABLE: usize = 0xe0;

    // AArch64 trampoline (8 instructions + 2 data words):
    //   ldr x0, dtb_ptr     ; x0 = DTB address
    //   ldr x18, entry_ptr  ; x18 = Linux entry point
    //   mov x1, xzr
    //   mov x2, xzr
    //   mov x3, xzr
    //   br x18
    //   .quad dtb_addr
    //   .quad LINUX_LOAD_ADDR
    //
    // PC-relative offsets for ldr (literal):
    //   instruction 0 at +0, data at +24: offset = +24, encoded as (24/4)=6 → imm19=6
    //   instruction 1 at +4, data at +28: offset = +24, encoded as (24/4)=6 → imm19=6
    //   ldr x0, [pc, #24]  = 0x58000300  (imm19 = 6 << 5, Rt = 0)
    //   ldr x18, [pc, #24] = 0x58000312  (imm19 = 6 << 5, Rt = 18 = 0x12)
    let trampoline_code: [u32; 6] = [
        0x58000300u32, // ldr x0, [pc, #24]   ; load dtb_addr
        0x58000312u32, // ldr x18, [pc, #24]  ; load linux entry
        0xAA1F03E1u32, // mov x1, xzr
        0xAA1F03E2u32, // mov x2, xzr
        0xAA1F03E3u32, // mov x3, xzr
        0xD61F0240u32, // br x18
    ];

    unsafe {
        let t = TRAMPOLINE_ADDR as *mut u32;

        // Write instructions
        for (i, &word) in trampoline_code.iter().enumerate() {
            t.add(i).write_volatile(word);
        }

        // Write data: dtb_addr and linux_entry at offset +24 (6 words)
        let data = TRAMPOLINE_ADDR as *mut u64;
        data.add(3).write_volatile(dtb_addr as u64);
        data.add(4).write_volatile(linux_vm::LINUX_LOAD_ADDR as u64);

        // Clean data cache and invalidate instruction cache
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
