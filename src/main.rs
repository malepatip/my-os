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
//   6. kernel_main() → blink LED pattern to signal FB result
//   7. kernel_main() → print boot banner + enter shell

#![no_std]
#![no_main]

mod boot;
mod uart;
mod mailbox;
mod framebuffer;
mod gpio;

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
    // This is the FIRST thing we do — before UART, before framebuffer.
    // If you see the green LED blink 3 times, the kernel is alive.
    gpio::init_led();
    gpio::blink(3, 200, 200); // 3 short blinks = "kernel alive"

    // ── Step 2: UART init ─────────────────────────────────────────────────
    uart::init();

    // ── Step 3: Framebuffer init ──────────────────────────────────────────
    let fb_ok = framebuffer::init();

    // Blink LED to signal framebuffer result:
    //   5 rapid blinks = framebuffer OK
    //   2 long blinks  = framebuffer FAILED (UART-only mode)
    if fb_ok {
        gpio::blink(5, 100, 100);
    } else {
        gpio::blink(2, 500, 300);
    }

    // ── Step 4: Boot banner ───────────────────────────────────────────────
    kprintln!("========================================");
    kprintln!("  my-ai-os v0.1.0");
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
        kprintln!("[kernel] Framebuffer: FAILED — check HDMI cable");
        kprintln!("[kernel] LED blinked 2 long pulses to confirm");
    }

    let el: u64;
    unsafe {
        core::arch::asm!("mrs {el}, CurrentEL", el = out(reg) el);
    }
    kprintln!("[kernel] Exception Level: EL{}", (el >> 2) & 0x3);
    kprintln!("[kernel] Cores 1-3:   parked");
    kprintln!("");
    kprintln!("[kernel] Entering shell...");
    kprintln!("Type 'help' for available commands.");
    kprintln!("");

    // ── Step 5: Shell ─────────────────────────────────────────────────────
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
            kprintln!("  halt          - Halt the CPU");
        }

        "info" => {
            kprintln!("my-ai-os v0.1.0");
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
    // Rapid SOS blink pattern on panic
    loop {
        gpio::blink(3, 100, 100); // 3 short
        gpio::blink(3, 300, 100); // 3 long
        gpio::blink(3, 100, 100); // 3 short
        gpio::delay_ms(500);
    }
}
