// emmc.rs — Bare-metal EMMC2 SD card driver for Raspberry Pi 4 (BCM2711)
//
// Key fixes over the previous version:
//   1. Queries the actual EMMC clock rate from the VideoCore GPU via the
//      property mailbox (tag 0x00030002, clock ID 0x1 = EMMC).
//      On Pi 4 the firmware typically sets this to 200 MHz, NOT 100 MHz.
//      Using the wrong base clock causes all divisor calculations to be off
//      by 2x, which makes the SD card receive double the expected clock
//      frequency during identification, causing CMD0/CMD8 timeouts.
//
//   2. Uses the SDHCI 3.0 "10-bit divided clock mode" divisor encoding
//      (bits [15:8] = lower 8 bits, bits [7:6] = upper 2 bits of divisor).
//      The old "8-bit power-of-2" encoding is for SDHCI < 3.0 only.
//
//   3. Clears CONTROL2 before init (required on BCM2711).
//
// All timeouts use pure NOP-count loops.
// Pi 4 Cortex-A72 @ 1.5 GHz: 1 NOP ≈ 0.67 ns → 1,500,000 NOPs ≈ 1 ms.

use core::ptr::{read_volatile, write_volatile};

// ─── EMMC2 Register Map (BCM2711, peripheral base 0xFE000000) ────────────────
const EMMC_BASE:       u64 = 0xFE34_0000;
const EMMC_ARG2:       u64 = EMMC_BASE + 0x00;
const EMMC_BLKSIZECNT: u64 = EMMC_BASE + 0x04;
const EMMC_ARG1:       u64 = EMMC_BASE + 0x08;
const EMMC_CMDTM:      u64 = EMMC_BASE + 0x0C;
const EMMC_RESP0:      u64 = EMMC_BASE + 0x10;
const EMMC_RESP1:      u64 = EMMC_BASE + 0x14;
const EMMC_RESP2:      u64 = EMMC_BASE + 0x18;
const EMMC_RESP3:      u64 = EMMC_BASE + 0x1C;
const EMMC_DATA:       u64 = EMMC_BASE + 0x20;
const EMMC_STATUS:     u64 = EMMC_BASE + 0x24;
const EMMC_CONTROL0:   u64 = EMMC_BASE + 0x28;
const EMMC_CONTROL1:   u64 = EMMC_BASE + 0x2C;
const EMMC_INTERRUPT:  u64 = EMMC_BASE + 0x30;
const EMMC_IRPT_MASK:  u64 = EMMC_BASE + 0x34;
const EMMC_IRPT_EN:    u64 = EMMC_BASE + 0x38;
const EMMC_CONTROL2:   u64 = EMMC_BASE + 0x3C;

// ─── VideoCore Mailbox (for clock query) ─────────────────────────────────────
const MBOX_BASE:    u64 = 0xFE00_B880;
const MBOX_READ:    u64 = MBOX_BASE + 0x00;
const MBOX_STATUS:  u64 = MBOX_BASE + 0x18;
const MBOX_WRITE:   u64 = MBOX_BASE + 0x20;
const MBOX_FULL:    u32 = 0x8000_0000;
const MBOX_EMPTY:   u32 = 0x4000_0000;
const MBOX_CH_PROP: u32 = 8;

// ─── CMDTM flags ──────────────────────────────────────────────────────────────
const CMD_NEED_APP:  u32 = 0x8000_0000;
const CMD_RSPNS_48:  u32 = 0x0002_0000;
const CMD_RSPNS_48B: u32 = 0x0003_0000;
const CMD_RSPNS_136: u32 = 0x0001_0000;
const CMD_DATA_READ: u32 = 0x0010_0000;
const CMD_IXCHK_EN:  u32 = 0x0010_0000;
const CMD_CRCCHK_EN: u32 = 0x0008_0000;

// ─── STATUS bits ─────────────────────────────────────────────────────────────
const SR_DAT_INHIBIT:    u32 = 0x0000_0002;
const SR_CMD_INHIBIT:    u32 = 0x0000_0001;
const SR_APP_CMD:        u32 = 0x0000_0020;

// ─── INTERRUPT bits ───────────────────────────────────────────────────────────
const INT_READ_RDY:   u32 = 0x0000_0020;
const INT_CMD_DONE:   u32 = 0x0000_0001;
const INT_ERROR_MASK: u32 = 0x017E_8000;

// ─── CONTROL1 bits ────────────────────────────────────────────────────────────
const C1_SRST_HC:    u32 = 0x0100_0000;
const C1_TOUNIT_MAX: u32 = 0x000E_0000;
const C1_CLK_EN:     u32 = 0x0000_0004;
const C1_CLK_STABLE: u32 = 0x0000_0002;
const C1_CLK_INTLEN: u32 = 0x0000_0001;

// ─── SD Commands ─────────────────────────────────────────────────────────────
const CMD0:   u32 = 0x0000_0000;
const CMD2:   u32 = 0x0200_0000 | CMD_RSPNS_136;
const CMD3:   u32 = 0x0300_0000 | CMD_RSPNS_48;
const CMD7:   u32 = 0x0700_0000 | CMD_RSPNS_48B;
const CMD8:   u32 = 0x0800_0000 | CMD_RSPNS_48;
const CMD16:  u32 = 0x1000_0000 | CMD_RSPNS_48;
const CMD17:  u32 = 0x1100_0000 | CMD_RSPNS_48
                  | CMD_DATA_READ | CMD_IXCHK_EN | CMD_CRCCHK_EN;
const CMD55:  u32 = 0x3700_0000 | CMD_RSPNS_48;
const ACMD41: u32 = CMD_NEED_APP | 0x2900_0000 | CMD_RSPNS_48;

// ─── Timing constants ─────────────────────────────────────────────────────────
// Pi 4 Cortex-A72 @ 1.5 GHz: 1 NOP ≈ 0.67 ns → 1,500,000 NOPs ≈ 1 ms.
const ITERS_PER_MS: u32 = 1_500_000;

// ─── Global state ─────────────────────────────────────────────────────────────
static mut SD_RCA:  u32  = 0;
static mut SD_HCCS: bool = false;

// ─── MMIO helpers ─────────────────────────────────────────────────────────────
#[inline(always)]
fn rd(reg: u64) -> u32 {
    unsafe { read_volatile(reg as *const u32) }
}

#[inline(always)]
fn wr(reg: u64, val: u32) {
    unsafe { write_volatile(reg as *mut u32, val) }
}

/// Spin for approximately `ms` milliseconds.
#[inline(never)]
fn sleep_ms(ms: u32) {
    let total = ms as u64 * ITERS_PER_MS as u64;
    for _ in 0..total {
        unsafe { core::arch::asm!("nop", options(nostack, nomem)) };
    }
}

// ─── Mailbox: query EMMC base clock ──────────────────────────────────────────
/// Query the VideoCore GPU for the EMMC base clock rate (clock ID 0x1).
/// Returns the clock in Hz, or 200_000_000 as a safe fallback.
fn get_emmc_clock_hz() -> u32 {
    #[repr(C, align(16))]
    struct MboxBuf { data: [u32; 9] }
    static mut MBOX_BUF: MboxBuf = MboxBuf { data: [0u32; 9] };
    unsafe {
        let buf = &mut MBOX_BUF.data;
        buf[0] = 9 * 4;          // total size
        buf[1] = 0x0000_0000;    // request
        buf[2] = 0x0003_0002;    // GET_CLOCK_RATE
        buf[3] = 8;              // value buffer size
        buf[4] = 4;              // request value length
        buf[5] = 0x0000_0001;    // clock ID: EMMC
        buf[6] = 0;              // response: rate in Hz
        buf[7] = 0;              // end tag
        buf[8] = 0;              // padding
        core::arch::asm!("dmb sy", options(nostack, nomem, preserves_flags));
        let addr = buf.as_ptr() as u32;
        let msg = (addr & !0xF) | MBOX_CH_PROP;
        let mut t = 1000 * ITERS_PER_MS;
        while rd(MBOX_STATUS) & MBOX_FULL != 0 {
            core::arch::asm!("nop", options(nostack, nomem));
            t -= 1; if t == 0 { return 200_000_000; }
        }
        wr(MBOX_WRITE, msg);
        let mut t = 1000 * ITERS_PER_MS;
        loop {
            while rd(MBOX_STATUS) & MBOX_EMPTY != 0 {
                core::arch::asm!("nop", options(nostack, nomem));
                t -= 1; if t == 0 { return 200_000_000; }
            }
            let resp = rd(MBOX_READ);
            if resp & 0xF == MBOX_CH_PROP {
                if buf[1] != 0x8000_0000 { return 200_000_000; }
                let rate = buf[6];
                if rate == 0 { return 200_000_000; }
                return rate;
            }
        }
    }
}

// ─── SDHCI 3.0 clock divisor ─────────────────────────────────────────────────
/// Calculate SDHCI 3.0 10-bit divided clock mode field for CONTROL1[15:6].
/// Divisor N: smallest power-of-2 such that base_hz/(2*N) <= target_hz.
fn calc_clock_field(base_hz: u32, target_hz: u32) -> u32 {
    if target_hz >= base_hz { return 0; }
    let mut n: u32 = 1;
    while n < 0x200 {
        if base_hz / (2 * n) <= target_hz { break; }
        n <<= 1;
    }
    if n >= 0x200 { n = 0x1FF; }
    let lo = n & 0xFF;
    let hi = (n >> 8) & 0x3;
    (lo << 8) | (hi << 6)
}

// ─── Wait helpers ─────────────────────────────────────────────────────────────
/// Wait until CMD and DAT lines are idle. Max ~10ms.
fn wait_cmd_idle() -> bool {
    let limit = 10 * ITERS_PER_MS;
    for _ in 0..limit {
        if rd(EMMC_STATUS) & (SR_CMD_INHIBIT | SR_DAT_INHIBIT) == 0 {
            return true;
        }
        unsafe { core::arch::asm!("nop", options(nostack, nomem)) };
    }
    false
}

/// Wait for an interrupt bit. Max ~100ms.

fn wait_interrupt(mask: u32) -> bool {
    let limit = 100 * ITERS_PER_MS;
    for _ in 0..limit {
        let irpt = rd(EMMC_INTERRUPT);
        if irpt & INT_ERROR_MASK != 0 {
            wr(EMMC_INTERRUPT, irpt);
            return false;
        }
        if irpt & mask != 0 {
            wr(EMMC_INTERRUPT, mask);
            return true;
        }
        unsafe { core::arch::asm!("nop", options(nostack, nomem)) };
    }
    false
}

// ─── Set SD clock ─────────────────────────────────────────────────────────────
fn set_clock(base_hz: u32, target_hz: u32) {
    // Wait for idle
    let limit = 10 * ITERS_PER_MS;
    for _ in 0..limit {
        if rd(EMMC_STATUS) & (SR_CMD_INHIBIT | SR_DAT_INHIBIT) == 0 { break; }
        unsafe { core::arch::asm!("nop", options(nostack, nomem)) };
    }
    // Disable SD clock
    wr(EMMC_CONTROL1, rd(EMMC_CONTROL1) & !C1_CLK_EN);
    sleep_ms(2);
    // Write new divisor using SDHCI 3.0 10-bit mode
    let clk_field = calc_clock_field(base_hz, target_hz);
    let c1 = (rd(EMMC_CONTROL1) & 0xFFFF_003F) | clk_field | C1_CLK_INTLEN;
    wr(EMMC_CONTROL1, c1);
    sleep_ms(2);
    // Wait for internal clock stable
    let limit = 10 * ITERS_PER_MS;
    for _ in 0..limit {
        if rd(EMMC_CONTROL1) & C1_CLK_STABLE != 0 { break; }
        unsafe { core::arch::asm!("nop", options(nostack, nomem)) };
    }
    wr(EMMC_CONTROL1, rd(EMMC_CONTROL1) | C1_CLK_EN);
    sleep_ms(2);
}

// ─── Send a command ───────────────────────────────────────────────────────────
fn send_cmd(cmd: u32, arg: u32) -> u32 {
    if cmd & CMD_NEED_APP != 0 {
        let rca = unsafe { SD_RCA };
        let r = send_cmd(CMD55, rca << 16);
        if r == 0xFFFF_FFFF { return 0xFFFF_FFFF; }
        if rd(EMMC_STATUS) & SR_APP_CMD == 0 { return 0xFFFF_FFFF; }
    }

    let cmdtm = cmd & !CMD_NEED_APP;

    if !wait_cmd_idle() { return 0xFFFF_FFFF; }

    wr(EMMC_INTERRUPT, 0xFFFF_FFFF);
    wr(EMMC_ARG1, arg);
    wr(EMMC_CMDTM, cmdtm);

    if !wait_interrupt(INT_CMD_DONE) { return 0xFFFF_FFFF; }

    rd(EMMC_RESP0)
}

// ─── Public API ───────────────────────────────────────────────────────────────

/// Initialize the EMMC2 controller and SD card.
/// Queries actual EMMC clock from VideoCore mailbox (typically 200 MHz on Pi 4).
pub fn sd_init() -> bool {
    // ── Step 0: Query actual EMMC base clock from GPU ─────────────────────────
    let base_hz = get_emmc_clock_hz();

    // ── Step 1: Reset controller ──────────────────────────────────────────────
    wr(EMMC_CONTROL2, 0);
    wr(EMMC_CONTROL0, 0);
    wr(EMMC_CONTROL1, C1_SRST_HC);
    sleep_ms(10);
    // Wait for reset complete — 100ms max
    let limit = 100 * ITERS_PER_MS;
    let mut reset_ok = false;
    for _ in 0..limit {
        if rd(EMMC_CONTROL1) & C1_SRST_HC == 0 { reset_ok = true; break; }
        unsafe { core::arch::asm!("nop", options(nostack, nomem)) };
    }
    if !reset_ok { return false; }

    // ── Step 2: Set timeout and identification clock (400 kHz) ────────────────
    wr(EMMC_CONTROL1, rd(EMMC_CONTROL1) | C1_TOUNIT_MAX);
    set_clock(base_hz, 400_000);

    // ── Step 3: Enable all interrupts ─────────────────────────────────────────
    wr(EMMC_IRPT_EN,   0xFFFF_FFFF);
    wr(EMMC_IRPT_MASK, 0xFFFF_FFFF);

    // ── Step 4: CMD0 — GO_IDLE ────────────────────────────────────────────────
    send_cmd(CMD0, 0);
    sleep_ms(2);

    // ── Step 5: CMD8 — SEND_IF_COND ──────────────────────────────────────────
    let r8 = send_cmd(CMD8, 0x0000_01AA);
    let is_v2 = r8 == 0x0000_01AA;

    // ── Step 6: ACMD41 — SD_SEND_OP_COND (max 50 retries x 5ms = 250ms) ─────
    let acmd_arg = if is_v2 { 0x51FF_8000u32 } else { 0x00FF_8000u32 };
    let mut resp: u32 = 0;
    let mut card_ready = false;
    for _ in 0..50u32 {
        resp = send_cmd(ACMD41, acmd_arg);
        if resp == 0xFFFF_FFFF { return false; }
        if resp & 0x8000_0000 != 0 { card_ready = true; break; }
        sleep_ms(5);
    }
    if !card_ready { return false; }
    unsafe { SD_HCCS = resp & 0x4000_0000 != 0; }

    // ── Step 7: CMD2 — ALL_SEND_CID ──────────────────────────────────────────
    if send_cmd(CMD2, 0) == 0xFFFF_FFFF { return false; }

    // ── Step 8: CMD3 — SEND_RELATIVE_ADDR ────────────────────────────────────
    let r3 = send_cmd(CMD3, 0);
    if r3 == 0xFFFF_FFFF { return false; }
    unsafe { SD_RCA = r3 >> 16; }

    // ── Step 9: Raise clock to 25 MHz ─────────────────────────────────────────
    set_clock(base_hz, 25_000_000);

    // ── Step 10: CMD7 — SELECT_CARD ──────────────────────────────────────────
    let rca = unsafe { SD_RCA };
    if send_cmd(CMD7, rca << 16) == 0xFFFF_FFFF { return false; }

    // ── Step 11: CMD16 — SET_BLOCKLEN = 512 ──────────────────────────────────
    let r16 = send_cmd(CMD16, 512);
    if r16 & 0xFFFF_0000 != 0 { return false; }

    true
}

/// Read one 512-byte block at the given LBA into `buf`.
pub fn sd_read_block(lba: u32, buf: &mut [u8; 512]) -> bool {
    let arg = if unsafe { SD_HCCS } { lba } else { lba * 512 };

    wr(EMMC_BLKSIZECNT, (1 << 16) | 512);

    let resp = send_cmd(CMD17, arg);
    if resp & 0xFFFF_0000 != 0 { return false; }

    if !wait_interrupt(INT_READ_RDY) { return false; }

    for i in (0..512).step_by(4) {
        let word = rd(EMMC_DATA);
        buf[i]   = (word        & 0xFF) as u8;
        buf[i+1] = ((word >> 8) & 0xFF) as u8;
        buf[i+2] = ((word >>16) & 0xFF) as u8;
        buf[i+3] = ((word >>24) & 0xFF) as u8;
    }

    wait_interrupt(INT_CMD_DONE);
    true
}
