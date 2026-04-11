// SPDX-License-Identifier: MIT
//
// main.rs — The kernel entry point for my-ai-os
//
// This is a bare-metal kernel for the Raspberry Pi 3/4 (AArch64).
// It runs directly on the hardware with NO operating system underneath.
// Every byte of output goes through our own UART driver talking to
// real hardware registers via MMIO.
//
// Architecture (inspired by macOS/XNU):
//   Layer 0: boot.rs     — Hardware init, core parking, BSS zeroing
//   Layer 1: uart.rs     — PL011 UART driver (MMIO)
//   Layer 2: main.rs     — Kernel main + interactive shell
//   (Future) Layer 3: scheduler, memory manager, AI inference engine

#![no_std]
#![no_main]

mod boot;
mod uart;
mod mailbox;
mod framebuffer;

use core::panic::PanicInfo;

// Board name for display
#[cfg(feature = "bsp_rpi3")]
const BOARD_NAME: &str = "Raspberry Pi 3";

#[cfg(feature = "bsp_rpi4")]
const BOARD_NAME: &str = "Raspberry Pi 4";

/// The Rust entry point, called by boot.rs after hardware init.
#[no_mangle]
pub fn kernel_main() -> ! {
    // Initialize the UART hardware
    uart::init();

    // Initialize the framebuffer (HDMI display). Silently continues on
    // failure — all output falls back to UART in that case.
    framebuffer::init();

    // Print the boot banner
    kprintln!();
    kprintln!("========================================");
    kprintln!("  my-ai-os v0.1.0");
    kprintln!("  A bare-metal AI Operating System");
    kprintln!("  Running on AArch64 ({})", BOARD_NAME);
    kprintln!("========================================");
    kprintln!();
    kprintln!("[kernel] UART initialized at 0x{:08X}", uart::base_address());
    kprintln!("[kernel] BSS section zeroed");
    kprintln!("[kernel] Core 0 active, cores 1-3 parked");
    kprintln!();

    // Print system info
    let el = current_exception_level();
    kprintln!("[kernel] Exception Level: EL{}", el);
    kprintln!();

    // Start the interactive shell
    kprintln!("[kernel] Starting shell...");
    kprintln!("Type 'help' for available commands.");
    kprintln!();

    shell_loop();
}

/// Read the current Exception Level from the CPU.
fn current_exception_level() -> u64 {
    let el: u64;
    unsafe {
        core::arch::asm!("mrs {}, CurrentEL", out(reg) el);
    }
    (el >> 2) & 0x3
}

/// A minimal interactive shell running on bare metal.
fn shell_loop() -> ! {
    let mut buf = [0u8; 256];
    loop {
        kprint!("ai-os> ");
        let len = read_line(&mut buf);
        let cmd = core::str::from_utf8(&buf[..len]).unwrap_or("");
        let cmd = cmd.trim();

        match cmd {
            "" => {}
            "help" => {
                kprintln!("Available commands:");
                kprintln!("  help     - Show this help message");
                kprintln!("  info     - Show system information");
                kprintln!("  echo <s> - Echo back the argument");
                kprintln!("  halt     - Halt the CPU");
                kprintln!();
                kprintln!("Future commands (to be implemented):");
                kprintln!("  mem      - Show memory map");
                kprintln!("  timer    - Read system timer");
                kprintln!("  infer    - Run AI inference");
            }
            "info" => {
                kprintln!("my-ai-os v0.1.0");
                kprintln!("Architecture: AArch64 (ARM Cortex-A53/A72)");
                kprintln!("Board: {}", BOARD_NAME);
                kprintln!("UART: PL011 at 0x{:08X}", uart::base_address());
                let el = current_exception_level();
                kprintln!("Exception Level: EL{}", el);
                kprintln!("Cores: 4 (1 active, 3 parked)");
            }
            "halt" => {
                kprintln!("[kernel] Halting CPU...");
                loop {
                    unsafe { core::arch::asm!("wfe") };
                }
            }
            _ if cmd.starts_with("echo ") => {
                kprintln!("{}", &cmd[5..]);
            }
            _ => {
                kprintln!("Unknown command: '{}'. Type 'help' for commands.", cmd);
            }
        }
    }
}

/// Read a line from UART into the buffer. Returns the number of bytes read.
fn read_line(buf: &mut [u8]) -> usize {
    let mut i = 0;
    loop {
        let c = uart::getc();
        match c {
            b'\r' | b'\n' => {
                uart::putc(b'\r');
                uart::putc(b'\n');
                return i;
            }
            // Backspace (0x7F or 0x08)
            0x7F | 0x08 => {
                if i > 0 {
                    i -= 1;
                    uart::putc(0x08);
                    uart::putc(b' ');
                    uart::putc(0x08);
                }
            }
            _ => {
                if i < buf.len() {
                    buf[i] = c;
                    i += 1;
                    uart::putc(c);
                }
            }
        }
    }
}

/// Panic handler — prints the panic message over UART and halts.
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    kprintln!();
    kprintln!("!!! KERNEL PANIC !!!");
    kprintln!("{}", info);
    kprintln!("System halted.");
    loop {
        unsafe { core::arch::asm!("wfe") };
    }
}
