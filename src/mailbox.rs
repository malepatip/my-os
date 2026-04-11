// SPDX-License-Identifier: MIT
//
// mailbox.rs — VideoCore GPU mailbox driver for Raspberry Pi 3/4
//
// The ARM↔GPU mailbox is the only way to configure the display on
// Raspberry Pi. We use channel 8 (the property interface) to ask the
// GPU to allocate a framebuffer.
//
// Key hardware invariants:
//   - The property buffer MUST be 16-byte aligned. The 4 LSBs of the
//     address carry the channel number, so the buffer address must have
//     its bottom 4 bits clear.
//   - A DMB (data memory barrier) must be issued before writing to the
//     mailbox to ensure the GPU sees a fully-written buffer.
//   - On Pi 4 (BCM2711), the GPU returns a direct physical address.
//     No bus-address masking needed.
//   - On Pi 3 (BCM2837), mask returned address with 0x3FFFFFFF.
//
// Reference: https://github.com/isometimes/rpi4-osdev/tree/master/part5-framebuffer

#[cfg(feature = "bsp_rpi3")]
const PERIPHERAL_BASE: usize = 0x3F00_0000;

#[cfg(feature = "bsp_rpi4")]
const PERIPHERAL_BASE: usize = 0xFE00_0000;

const VIDEOCORE_MBOX: usize = PERIPHERAL_BASE + 0x0000_B880;

const MBOX_READ:   *const u32 = (VIDEOCORE_MBOX + 0x00) as *const u32;
const MBOX_STATUS: *const u32 = (VIDEOCORE_MBOX + 0x18) as *const u32;
const MBOX_WRITE:  *mut   u32 = (VIDEOCORE_MBOX + 0x20) as *mut u32;

const MBOX_FULL:  u32 = 0x8000_0000;
const MBOX_EMPTY: u32 = 0x4000_0000;

pub const MBOX_REQUEST:  u32 = 0x0000_0000;
pub const MBOX_RESPONSE: u32 = 0x8000_0000;
pub const MBOX_CH_PROP:  u8  = 8;

// Mailbox property tags
pub const MBOX_TAG_SETPHYWH:   u32 = 0x0004_8003;
pub const MBOX_TAG_SETVIRTWH:  u32 = 0x0004_8004;
pub const MBOX_TAG_SETVIRTOFF: u32 = 0x0004_8009;
pub const MBOX_TAG_SETDEPTH:   u32 = 0x0004_8005;
pub const MBOX_TAG_SETPXLORDR: u32 = 0x0004_8006;
pub const MBOX_TAG_GETFB:      u32 = 0x0004_0001;
pub const MBOX_TAG_GETPITCH:   u32 = 0x0004_0008;
pub const MBOX_TAG_LAST:       u32 = 0x0000_0000;

/// Send a 16-byte-aligned property buffer to the GPU on channel 8
/// and wait for the response. Returns true on success.
///
/// # Safety
/// buf must be 16-byte aligned. buf[0] must be the total buffer size in bytes.
pub unsafe fn call(buf: *mut u32, channel: u8) -> bool {
    let addr = buf as u32;
    if addr & 0xF != 0 {
        return false;
    }

    let msg = (addr & !0xF) | (channel as u32 & 0xF);

    // Ensure all buffer writes are visible to GPU before posting
    core::arch::asm!("dmb sy", options(nostack, nomem, preserves_flags));

    // Wait until write slot is available
    while MBOX_STATUS.read_volatile() & MBOX_FULL != 0 {
        core::hint::spin_loop();
    }
    MBOX_WRITE.write_volatile(msg);

    // Wait for response on our channel
    loop {
        while MBOX_STATUS.read_volatile() & MBOX_EMPTY != 0 {
            core::hint::spin_loop();
        }
        let resp = MBOX_READ.read_volatile();
        if resp & 0xF == channel as u32 {
            let code = buf.add(1).read_volatile();
            return code == MBOX_RESPONSE;
        }
    }
}
