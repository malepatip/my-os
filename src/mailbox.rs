// SPDX-License-Identifier: MIT
//
// mailbox.rs — VideoCore GPU mailbox driver
//
// The ARM↔GPU mailbox is the only way to configure the display on
// Raspberry Pi. We use channel 8 (the property interface) to ask the
// GPU to allocate a framebuffer.
//
// Hardware invariants relied upon:
//   - The property buffer MUST be 16-byte aligned (bits [3:0] of the
//     address are used to carry the channel number).
//   - We add a DMB barrier before writing to ensure the GPU sees a
//     fully-written buffer (important even without D-cache, as the
//     interconnect may reorder writes).
//   - On Pi 4 (BCM2711), GPU bus address == physical address (no
//     0xC0000000 offset needed unlike Pi 1/2/3).

#[cfg(feature = "bsp_rpi3")]
const PERIPHERAL_BASE: usize = 0x3F00_0000;

#[cfg(feature = "bsp_rpi4")]
const PERIPHERAL_BASE: usize = 0xFE00_0000;

const MBOX_BASE: usize = PERIPHERAL_BASE + 0x0000_B880;

// Mailbox 0 (GPU → ARM): used for reading responses
const MBOX_READ:   *mut u32 = (MBOX_BASE + 0x00) as *mut u32;
// Shared status register
const MBOX_STATUS: *const u32 = (MBOX_BASE + 0x18) as *const u32;
// Mailbox 1 (ARM → GPU): used for sending requests
const MBOX_WRITE:  *mut u32 = (MBOX_BASE + 0x20) as *mut u32;

const STATUS_FULL:  u32 = 1 << 31; // mailbox 1 full — cannot write
const STATUS_EMPTY: u32 = 1 << 30; // mailbox 0 empty — nothing to read

pub const PROP_CHANNEL: u32 = 8;

pub const REQUEST:          u32 = 0x0000_0000;
pub const RESPONSE_SUCCESS: u32 = 0x8000_0000;

/// Send a 16-byte-aligned property buffer to the GPU and wait for the
/// response. Returns `true` if the GPU acknowledged with success.
///
/// # Safety
/// `buf` must point to a valid, 16-byte-aligned u32 slice whose first
/// element contains the total byte length of the buffer. The slice
/// must remain valid for the duration of this call.
pub unsafe fn call(buf: *mut u32) -> bool {
    let addr = buf as u32;
    debug_assert_eq!(addr & 0xF, 0, "mailbox buffer must be 16-byte aligned");

    // DMB: ensure all writes to the buffer are visible before we signal the GPU
    core::arch::asm!("dmb sy", options(nostack, nomem, preserves_flags));

    // Wait until mailbox 1 has space, then post our message
    while MBOX_STATUS.read_volatile() & STATUS_FULL != 0 {
        core::hint::spin_loop();
    }
    MBOX_WRITE.write_volatile((addr & !0xF) | PROP_CHANNEL);

    // Wait for a response on our channel
    loop {
        while MBOX_STATUS.read_volatile() & STATUS_EMPTY != 0 {
            core::hint::spin_loop();
        }
        let resp = MBOX_READ.read_volatile();
        if resp & 0xF == PROP_CHANNEL {
            // buf[1] holds the response code
            let code = buf.add(1).read_volatile();
            return code == RESPONSE_SUCCESS;
        }
    }
}
