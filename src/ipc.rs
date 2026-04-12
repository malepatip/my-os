// ipc.rs — Shared memory IPC between EL2 ai-os and EL1 Linux driver VM
//
// Layout at SHMEM_ADDR (0x0020_0000):
//   Offset 0:   magic (u32) — SHMEM_MAGIC = 0xAA55AA55 when Linux is ready
//   Offset 4:   write_idx (u32) — next slot Linux will write
//   Offset 8:   read_idx  (u32) — next slot we will read
//   Offset 12:  _pad (u32)
//   Offset 16:  ring[256] (u8) — ASCII keystrokes

use crate::linux_vm::{SharedMem, SHMEM_MAGIC};

/// Poll for a keypress from the Linux USB HID daemon.
/// Returns Some(ascii) if a key is available, None otherwise.
#[inline]
pub fn poll_key() -> Option<u8> {
    let shm = SharedMem::get();
    if !shm.is_ready() {
        return None;
    }
    shm.read_char()
}

/// Block until Linux driver VM signals ready (magic written to shared mem).
/// Prints a waiting indicator every second.
pub fn wait_for_linux_ready() {
    let shm = SharedMem::get();
    let mut dots = 0u32;
    while !shm.is_ready() {
        // Spin with a small delay (~1ms using system counter)
        crate::gpio::delay_ms(100);
        dots += 1;
        if dots % 10 == 0 {
            crate::kprint!(".");
        }
        if dots > 300 {
            // 30 seconds timeout
            crate::kprintln!(" timeout!");
            crate::kprintln!("[kernel] USB HID: Linux driver VM did not respond");
            crate::kprintln!("[kernel] Falling back to UART-only mode");
            return;
        }
    }
    crate::kprintln!(" OK");
    crate::kprintln!("[kernel] USB HID: Linux driver VM ready, keyboard active");
}

/// Block until a keypress is available (polls shared memory).
/// Used by the shell for blocking input.
pub fn read_char_blocking() -> u8 {
    loop {
        if let Some(ch) = poll_key() {
            return ch;
        }
        // Yield — small delay to avoid burning CPU
        unsafe {
            core::arch::asm!("yield", options(nomem, nostack));
        }
    }
}
