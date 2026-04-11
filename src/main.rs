// SPDX-License-Identifier: MIT
//
// main.rs — Kernel entry point for my-ai-os
//
// Boot sequence:
//   1. boot.rs (assembly) → parks cores 1-3, sets up stack, zeros BSS
//   2. kernel_main() → init GPIO LED (visual heartbeat)
//   3. kernel_main() → blink LED 3 times = "kernel alive"
//   4. kernel_main() → init UART
//   5. kernel_main() → init framebuffer (HDMI)
//   6. kernel_main() → init SD card (EMMC2) + FAT32
//   7. kernel_main() → print boot banner + enter shell
//
// Shell commands:
//   help, info, echo, clear, blink, halt
//   sdinfo  — show SD card status
//   ls      — list root directory of SD card
//   cat     — print first 2KB of a file (8.3 name, e.g. "README.TXT")

#![no_std]
#![no_main]

mod boot;
mod uart;
mod mailbox;
mod framebuffer;
mod gpio;
mod emmc;
mod fat32;

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
    // ── Step 1: GPIO LED init ─────────────────────────────────────────────
    gpio::init_led();
    gpio::blink(3, 200, 200); // 3 short blinks = "kernel alive"

    // ── Step 2: UART init ─────────────────────────────────────────────────
    uart::init();

    // ── Step 3: Framebuffer init ──────────────────────────────────────────
    let fb_ok = framebuffer::init();
    if fb_ok {
        gpio::blink(5, 100, 100);  // 5 rapid = FB OK
    } else {
        gpio::blink(2, 500, 300);  // 2 long = FB failed
    }

    // ── Step 4: Boot banner ───────────────────────────────────────────────
    kprintln!("========================================");
    kprintln!("  my-ai-os v0.2.0");
    kprintln!("  Bare-Metal AArch64 AI Operating System");
    #[cfg(feature = "bsp_rpi4")]
    kprintln!("  Board: Raspberry Pi 4 (BCM2711)");
    #[cfg(feature = "bsp_rpi3")]
    kprintln!("  Board: Raspberry Pi 3 (BCM2837)");
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

    // ── Step 5: SD card + FAT32 init ─────────────────────────────────────
    kprint!("[kernel] SD card:     ");
    let sd_ok = emmc::sd_init();
    if sd_ok {
        kprintln!("OK");
        kprint!("[kernel] FAT32:       ");
        let fat_ok = fat32::fat32_init();
        if fat_ok {
            kprintln!("OK");
        } else {
            kprintln!("FAILED -- no FAT32 partition found");
        }
    } else {
        kprintln!("FAILED -- check SD card");
    }

    kprintln!("");
    kprintln!("[kernel] Entering shell...");
    kprintln!("Type 'help' for available commands.");
    kprintln!("");

    // ── Step 6: Shell ─────────────────────────────────────────────────────
    shell();
}

fn shell() -> ! {
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

/// Convert a byte slice to uppercase ASCII in-place into a fixed buffer.
/// Returns the number of bytes written.
fn to_upper(src: &[u8], dst: &mut [u8]) -> usize {
    let len = src.len().min(dst.len());
    for i in 0..len {
        dst[i] = src[i].to_ascii_uppercase();
    }
    len
}

fn execute(cmd: &str) {
    let (name, args) = match cmd.find(' ') {
        Some(i) => (&cmd[..i], cmd[i+1..].trim()),
        None    => (cmd, ""),
    };

    match name {
        "" => {}

        "help" => {
            kprintln!("Available commands:");
            kprintln!("  help          - Show this help");
            kprintln!("  info          - System information");
            kprintln!("  echo <text>   - Echo text back");
            kprintln!("  clear         - Clear the screen");
            kprintln!("  blink <n>     - Blink LED n times");
            kprintln!("  sdinfo        - SD card status");
            kprintln!("  ls            - List SD card root directory");
            kprintln!("  cat <FILE.EXT>- Print file contents (8.3 name)");
            kprintln!("  halt          - Halt the CPU");
        }

        "info" => {
            kprintln!("my-ai-os v0.2.0");
            kprintln!("Architecture: AArch64 (ARM Cortex-A72)");
            #[cfg(feature = "bsp_rpi4")]
            kprintln!("Board:        Raspberry Pi 4 (BCM2711)");
            #[cfg(feature = "bsp_rpi3")]
            kprintln!("Board:        Raspberry Pi 3 (BCM2837)");
            kprintln!("UART:         PL011 @ 0x{:08X}", uart::base_address());
            kprintln!("Cores:        4 (1 active, 3 parked)");
        }

        "echo" => {
            kprintln!("{}", args);
        }

        "clear" => {
            framebuffer::clear_screen();
        }

        "blink" => {
            let n: u32 = args.parse().unwrap_or(3);
            gpio::blink(n, 200, 200);
            kprintln!("Blinked {} times", n);
        }

        "sdinfo" => {
            // Re-run init to report current state
            let sd_ok = emmc::sd_init();
            if sd_ok {
                kprintln!("SD card: OK");
                let fat_ok = fat32::fat32_init();
                if fat_ok {
                    kprintln!("FAT32:   OK");
                } else {
                    kprintln!("FAT32:   FAILED");
                }
            } else {
                kprintln!("SD card: FAILED");
            }
        }

        "ls" => {
            kprintln!("Root directory:");
            kprintln!("  {:<12} {:>10}  TYPE", "NAME", "SIZE");
            kprintln!("  ------------------------------");
            let mut count = 0u32;
            fat32::fat32_list_root(|info| {
                // Build display name from info.name bytes (null-terminated)
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
                kprintln!("  Example: cat README.TXT");
                kprintln!("  Note: 8.3 format only (max 8 char name, 3 char ext)");
                return;
            }

            // Parse "FILENAME.EXT" into an 8.3 space-padded array (uppercase).
            // FAT32 stores names as 11-byte uppercase space-padded: "README  TXT"
            let mut name83 = [b' '; 11];
            let mut upper_buf = [0u8; 16];
            let arg_bytes = args.as_bytes();
            let upper_len = to_upper(arg_bytes, &mut upper_buf);
            let upper_bytes = &upper_buf[..upper_len];

            let dot_pos = upper_bytes.iter().position(|&b| b == b'.');
            match dot_pos {
                Some(d) => {
                    // Name part: up to 8 chars before the dot
                    let name_len = d.min(8);
                    for i in 0..name_len { name83[i] = upper_bytes[i]; }
                    // Extension: up to 3 chars after the dot
                    let ext_src = &upper_bytes[(d + 1)..];
                    let ext_len = ext_src.len().min(3);
                    for i in 0..ext_len { name83[8 + i] = ext_src[i]; }
                }
                None => {
                    // No extension — just fill name part
                    let name_len = upper_len.min(8);
                    for i in 0..name_len { name83[i] = upper_bytes[i]; }
                }
            }

            kprintln!("--- {} ---", args);
            let mut bytes_shown = 0usize;
            let found = fat32::fat32_read_file(&name83, |chunk, len| {
                if bytes_shown >= 2048 { return; } // limit display to 2 KB
                for &b in &chunk[..len] {
                    if bytes_shown >= 2048 { break; }
                    if b == b'\r' { continue; } // skip CR in CRLF
                    uart::putc(b);
                    let _ = write!(framebuffer::FbWriter, "{}", b as char);
                    bytes_shown += 1;
                }
            });

            if !found {
                kprintln!("File not found: {}", args);
                kprintln!("Tip: use 8.3 format (e.g. README.TXT not readme.txt)");
            } else {
                kprintln!("");
                kprintln!("--- end ---");
            }
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
        gpio::blink(3, 100, 100); // 3 short
        gpio::blink(3, 300, 100); // 3 long
        gpio::blink(3, 100, 100); // 3 short
        gpio::delay_ms(500);
    }
}
