// SPDX-License-Identifier: MIT
//
// uart.rs — PL011 UART driver for Raspberry Pi 3 and Pi 4
//
// This driver talks DIRECTLY to hardware registers via MMIO.
// No OS, no drivers, no abstractions — just raw memory-mapped I/O.
//
// The PL011 UART sits at the same offset (0x201000) from the
// peripheral base, but the base address differs between boards:
//
//   Raspberry Pi 3:  peripheral base = 0x3F000000  →  UART at 0x3F201000
//   Raspberry Pi 4:  peripheral base = 0xFE000000  →  UART at 0xFE201000
//
// We select the correct base at compile time using Cargo feature flags.
//
// NOTE: kprint!/kprintln! output UART only. Framebuffer output is
// intentionally removed until UART is confirmed working on real hardware.

use core::fmt;

// ---- Peripheral base address (board-specific) ----

#[cfg(feature = "bsp_rpi3")]
const PERIPHERAL_BASE: usize = 0x3F00_0000;

#[cfg(feature = "bsp_rpi4")]
const PERIPHERAL_BASE: usize = 0xFE00_0000;

// ---- PL011 UART registers (same offset on both boards) ----

const UART_BASE: usize = PERIPHERAL_BASE + 0x0020_1000;

const UART_DR:   *mut u32   = UART_BASE as *mut u32;                // Data Register
const UART_FR:   *const u32 = (UART_BASE + 0x18) as *const u32;    // Flag Register
const UART_IBRD: *mut u32   = (UART_BASE + 0x24) as *mut u32;      // Integer Baud Rate Divisor
const UART_FBRD: *mut u32   = (UART_BASE + 0x28) as *mut u32;      // Fractional Baud Rate Divisor
const UART_LCRH: *mut u32   = (UART_BASE + 0x2C) as *mut u32;      // Line Control Register
const UART_CR:   *mut u32   = (UART_BASE + 0x30) as *mut u32;      // Control Register
const UART_ICR:  *mut u32   = (UART_BASE + 0x44) as *mut u32;      // Interrupt Clear Register

// Flag Register bits
const FR_TXFF: u32 = 1 << 5;  // Transmit FIFO full
const FR_RXFE: u32 = 1 << 4;  // Receive FIFO empty

/// Initialize the PL011 UART.
/// In QEMU this is optional (UART works without init), but on real
/// hardware you need this to configure baud rate and line settings.
pub fn init() {
    unsafe {
        // Disable UART while configuring
        UART_CR.write_volatile(0);

        // Clear all pending interrupts
        UART_ICR.write_volatile(0x7FF);

        // Set baud rate to 115200
        // Pi 3: UART clock = 48 MHz → Divider = 48000000 / (16 * 115200) = 26.042
        // Pi 4: UART clock = 48 MHz (default) → same divider
        // Integer part = 26, Fractional part = round(0.042 * 64) = 3
        UART_IBRD.write_volatile(26);
        UART_FBRD.write_volatile(3);

        // 8 data bits, no parity, 1 stop bit, enable FIFOs
        UART_LCRH.write_volatile((1 << 4) | (1 << 5) | (1 << 6)); // FEN | WLEN[1] | WLEN[0]

        // Enable UART, TX, and RX
        UART_CR.write_volatile((1 << 0) | (1 << 8) | (1 << 9)); // UARTEN | TXE | RXE
    }
}

/// Send a single byte over UART.
pub fn putc(c: u8) {
    unsafe {
        // Wait until the transmit FIFO is not full
        while UART_FR.read_volatile() & FR_TXFF != 0 {
            core::hint::spin_loop();
        }
        // Write the byte to the data register
        UART_DR.write_volatile(c as u32);
    }
}

/// Read a single byte from UART (blocking).
pub fn getc() -> u8 {
    unsafe {
        // Wait until the receive FIFO is not empty
        while UART_FR.read_volatile() & FR_RXFE != 0 {
            core::hint::spin_loop();
        }
        // Read the byte from the data register
        UART_DR.read_volatile() as u8
    }
}

/// Read a single byte from UART without blocking.
/// Returns 0 immediately if no data is available in the receive FIFO.
pub fn getc_nonblocking() -> u8 {
    unsafe {
        if UART_FR.read_volatile() & FR_RXFE != 0 {
            return 0; // FIFO empty
        }
        UART_DR.read_volatile() as u8
    }
}

/// Print a string over UART.
pub fn puts(s: &str) {
    for byte in s.bytes() {
        if byte == b'\n' {
            putc(b'\r'); // UART needs \r\n for newlines
        }
        putc(byte);
    }
}

/// Return the UART base address for display purposes.
pub fn base_address() -> usize {
    UART_BASE
}

/// A writer struct that implements core::fmt::Write for UART output.
pub struct UartWriter;

impl fmt::Write for UartWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        puts(s);
        Ok(())
    }
}
