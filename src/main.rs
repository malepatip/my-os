// SPDX-License-Identifier: MIT
//
// main.rs — ai-os v0.6.0 — EL2 Thin Hypervisor + Linux Driver VM (TF-A boot)
//
// Architecture:
//   TF-A (bl31.bin) runs at EL3, handles PSCI, patches DTB
//   ai-os Rust kernel runs at EL2 (Core 0 only, dropped here by TF-A)
//   Linux driver VM runs at EL1 on all 4 cores (TF-A handles secondary cores)
//   Shared memory ring buffer at 0x0020_0000 for USB HID IPC
//
// Boot sequence:
//   1. TF-A (EL3) → patches DTB (adds psci node), drops Core 0 to EL2 at 0x80000
//   2. boot.rs (EL2 assembly) → zeros BSS, calls kernel_main()
//   3. kernel_main() → GPIO LED, UART, Framebuffer, boot banner
//   4. kernel_main() → load Linux kernel + initramfs + DTB from SD card
//   5. kernel_main() → set up stage-2 identity page tables
//   6. kernel_main() → ERET Core 0 into Linux at EL1
//   7. Linux boots, calls CPU_ON SMC for Cores 1-3
//   8. TF-A intercepts CPU_ON → drops Cores 1-3 to Linux at EL1
//   9. Linux SMP: all 4 cores at EL1, no mode mismatch
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
    // With TF-A, the GPU display subsystem needs extra time to initialise
    // before the VideoCore mailbox framebuffer request will succeed.
    // A 500ms delay here ensures the GPU has finished its own boot sequence.
    gpio::delay_ms(500);
    let fb_ok = framebuffer::init();
    if fb_ok { gpio::blink(5, 100, 100); }

    // ── Step 2: Boot banner ───────────────────────────────────────────────
    kprintln!("========================================");
    kprintln!("  ai-os v0.6.0");
    kprintln!("  EL2 Hypervisor + Linux Driver VM (TF-A)");
    kprintln!("  Board: Raspberry Pi 4 (BCM2711)");
    kprintln!("========================================");
    kprintln!("");
    kprintln!("[kernel] UART:        OK @ 0x{:08X}", uart::base_address());
    if fb_ok {
        kprintln!("[kernel] Framebuffer: OK (1280x720 @ 32bpp)");
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

    // ── Step 6: ERET Core 0 into Linux at EL1 ───────────────────────────────
    // With TF-A, Core 0 is at EL2. We ERET directly into Linux at EL1.
    // TF-A will handle CPU_ON SMC calls from Linux to wake Cores 1-3.
    // This call does NOT return — execution continues inside Linux.
    kprintln!("[kernel] Entering Linux at EL1 (Core 0)...");
    kprintln!("[kernel] TF-A will handle secondary core bring-up via PSCI.");
    unsafe { linux_vm::launch_linux(linux_vm::LINUX_DTB_ADDR); }
    // NOTE: launch_linux() does not return (it ERETSs into Linux).
    // The shell below is unreachable in normal TF-A operation.
    // It is retained so the function signature compiles as -> !.
    // If Linux somehow returns (it should not), we fall through to shell.
    kprintln!("");
    kprintln!("[kernel] WARNING: Linux returned unexpectedly.");
    kprintln!("[kernel] Entering fallback shell...");
    kprintln!("");
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
    let _ = writeln!(framebuffer::FbWriter, "\n[KERNEL PANIC] {}", info);
    loop {
        gpio::blink(3, 100, 100);
        gpio::blink(3, 300, 100);
        gpio::blink(3, 100, 100);
        gpio::delay_ms(500);
    }
}
