// SPDX-License-Identifier: MIT
//
// framebuffer.rs — Pi 3/4 HDMI framebuffer driver + text console
//
// FIXES vs the previous broken version:
//   1. MBOX_TAG_GETFB alignment = 4096 (was 16 — GPU rejected it)
//   2. Pixel order = 1 (RGB, not 0=BGR) — Pi 4 GPU requires this
//   3. Pi 4: no bus address masking needed (GPU returns physical addr)
//   4. Pi 3: mask returned address with 0x3FFFFFFF
//   5. Resolution 1280x720 (was 1920x1080 — fails on many HDMI monitors)
//   6. mailbox::call() now takes channel arg (updated API)
//
// Reference: https://github.com/isometimes/rpi4-osdev/blob/master/part5-framebuffer/fb.c

use core::fmt;
use crate::mailbox;
use crate::font::FONT;

const WIDTH:  u32 = 1280;
const HEIGHT: u32 = 720;
const BPP:    u32 = 32;

// Colours (0xAARRGGBB)
const BG: u32 = 0xFF1A_1A2E; // dark navy
const FG: u32 = 0xFFFF_FFFF; // white

const FONT_W: u32 = 8;
const FONT_H: u32 = 8;
const SCALE:  u32 = 2;              // each font pixel → 2×2 screen pixels
const CHAR_W: u32 = FONT_W * SCALE; // 16 screen pixels wide per char
const CHAR_H: u32 = FONT_H * SCALE; // 16 screen pixels tall per char

// Framebuffer state
struct Fb {
    base:   *mut u32,
    width:  u32,
    height: u32,
    pitch:  u32,
    cols:   u32,
    rows:   u32,
    col:    u32,
    row:    u32,
}

static mut FB: Option<Fb> = None;

// Static 16-byte-aligned mailbox buffer — NOT stack allocated
#[repr(C, align(16))]
struct MboxBuf([u32; 36]);
static mut MBOX_BUF: MboxBuf = MboxBuf([0u32; 36]);

pub fn init() -> bool {
    unsafe { init_inner() }
}

unsafe fn init_inner() -> bool {
    let m = &mut MBOX_BUF.0;

    // Build property message — exact format from rpi4-osdev fb.c
    m[0]  = 35 * 4;                           // total size in bytes
    m[1]  = mailbox::MBOX_REQUEST;

    m[2]  = mailbox::MBOX_TAG_SETPHYWH;       // set physical width/height
    m[3]  = 8; m[4]  = 0;
    m[5]  = WIDTH; m[6]  = HEIGHT;

    m[7]  = mailbox::MBOX_TAG_SETVIRTWH;      // set virtual width/height
    m[8]  = 8; m[9]  = 8;
    m[10] = WIDTH; m[11] = HEIGHT;

    m[12] = mailbox::MBOX_TAG_SETVIRTOFF;     // set virtual offset
    m[13] = 8; m[14] = 8;
    m[15] = 0; m[16] = 0;

    m[17] = mailbox::MBOX_TAG_SETDEPTH;       // set colour depth
    m[18] = 4; m[19] = 4;
    m[20] = BPP;                              // 32 bpp

    m[21] = mailbox::MBOX_TAG_SETPXLORDR;     // set pixel order
    m[22] = 4; m[23] = 4;
    m[24] = 1;                                // 1 = RGB (CRITICAL: not 0=BGR)

    m[25] = mailbox::MBOX_TAG_GETFB;          // allocate framebuffer
    m[26] = 8; m[27] = 8;
    m[28] = 4096;                             // alignment (CRITICAL: must be 4096)
    m[29] = 0;                                // GPU fills: FB size

    m[30] = mailbox::MBOX_TAG_GETPITCH;       // get bytes per line
    m[31] = 4; m[32] = 4;
    m[33] = 0;                                // GPU fills: pitch

    m[34] = mailbox::MBOX_TAG_LAST;

    if !mailbox::call(m.as_mut_ptr(), mailbox::MBOX_CH_PROP) {
        return false;
    }

    // Verify GPU returned 32bpp and a valid framebuffer address
    if m[20] != BPP || m[28] == 0 {
        return false;
    }

    // The GPU returns a bus address (e.g. 0xC0000000 + phys_addr on Pi 4).
    // Mask off the top 2 bits to get the ARM physical address.
    // This is required on BOTH Pi 3 and Pi 4 — the reference rpi4-osdev
    // fb.c always does: mbox[28] &= 0x3FFFFFFF
    let base = (m[28] & 0x3FFF_FFFF) as *mut u32;

    let pitch  = m[33];
    let actual_w = m[10];
    let actual_h = m[11];

    FB = Some(Fb {
        base,
        width:  actual_w,
        height: actual_h,
        pitch,
        cols: actual_w / CHAR_W,
        rows: actual_h / CHAR_H,
        col: 0,
        row: 0,
    });

    clear_screen();
    true
}

pub fn clear_screen() {
    unsafe {
        if let Some(ref mut fb) = FB {
            let pixels = (fb.pitch / 4) * fb.height;
            for i in 0..pixels {
                fb.base.add(i as usize).write_volatile(BG);
            }
            fb.col = 0;
            fb.row = 0;
        }
    }
}

/// Draw a character at character-grid position (char_col, char_row).
/// Each font pixel is rendered as a SCALE×SCALE block of screen pixels.
fn draw_char(c: u8, char_col: u32, char_row: u32) {
    unsafe {
        let fb = match FB { Some(ref mut f) => f, None => return };
        let idx = if c >= 0x20 && c <= 0x7F { (c - 0x20) as usize } else { 0 };
        let glyph = &FONT[idx];
        let pitch_words = fb.pitch / 4;
        let origin_x = char_col * CHAR_W;
        let origin_y = char_row * CHAR_H;
        for row in 0..FONT_H {
            let byte = glyph[row as usize];
            for col in 0..FONT_W {
                let pixel = if byte & (0x80 >> col) != 0 { FG } else { BG };
                for sy in 0..SCALE {
                    for sx in 0..SCALE {
                        let x = origin_x + col * SCALE + sx;
                        let y = origin_y + row * SCALE + sy;
                        if x < fb.width && y < fb.height {
                            fb.base.add((y * pitch_words + x) as usize).write_volatile(pixel);
                        }
                    }
                }
            }
        }
    }
}

fn scroll_up() {
    unsafe {
        let fb = match FB { Some(ref mut f) => f, None => return };
        if fb.height <= CHAR_H { return; }
        let scroll_bytes = (fb.height - CHAR_H) * fb.pitch;
        let src = (fb.base as *mut u8).add(CHAR_H as usize * fb.pitch as usize);
        let dst = fb.base as *mut u8;
        core::ptr::copy(src, dst, scroll_bytes as usize);
        // Clear the newly exposed bottom row
        let last_row_offset = (fb.height - CHAR_H) * (fb.pitch / 4);
        let last_row_words  = CHAR_H * (fb.pitch / 4);
        for i in 0..last_row_words {
            fb.base.add((last_row_offset + i) as usize).write_volatile(BG);
        }
    }
}

fn write_byte(c: u8) {
    unsafe {
        let fb = match FB { Some(ref mut f) => f, None => return };
        match c {
            b'\n' => { fb.col = 0; fb.row += 1; }
            b'\r' => { fb.col = 0; }
            0x08 | 0x7F => {
                if fb.col > 0 {
                    fb.col -= 1;
                    draw_char(b' ', fb.col, fb.row);
                }
            }
            _ => {
                draw_char(c, fb.col, fb.row);
                fb.col += 1;
                if fb.col >= fb.cols { fb.col = 0; fb.row += 1; }
            }
        }
        if fb.row >= fb.rows { scroll_up(); fb.row = fb.rows - 1; }
    }
}

pub struct FbWriter;

impl fmt::Write for FbWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for b in s.bytes() { write_byte(b); }
        Ok(())
    }
}

/// Return (base, pitch, width, height) so hv_console can mirror the buffer.
/// Returns null base if the framebuffer was never initialised.
pub fn get_info() -> (*mut u32, u32, u32, u32) {
    unsafe {
        match FB {
            Some(ref fb) => (fb.base, fb.pitch, fb.width, fb.height),
            None         => (core::ptr::null_mut(), 0, 0, 0),
        }
    }
}
