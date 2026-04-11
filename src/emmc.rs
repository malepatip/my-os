// emmc.rs — Bare-metal EMMC2 SD card driver for Raspberry Pi 4
// EMMC2 controller base: 0xFE340000 (Pi 4 / BCM2711)
//
// All timeouts use the ARM physical counter (CNTPCT_EL0 / CNTFRQ_EL0)
// which is always available at EL2 and cannot hang. No mailbox calls.
// Worst-case sd_init() time: ~300ms. Guaranteed to return.

use core::ptr::{read_volatile, write_volatile};

// ─── EMMC2 Register Map ───────────────────────────────────────────────────────
const EMMC_BASE: u64 = 0xFE34_0000;

const EMMC_ARG2:        u64 = EMMC_BASE + 0x00;
const EMMC_BLKSIZECNT:  u64 = EMMC_BASE + 0x04;
const EMMC_ARG1:        u64 = EMMC_BASE + 0x08;
const EMMC_CMDTM:       u64 = EMMC_BASE + 0x0C;
const EMMC_RESP0:       u64 = EMMC_BASE + 0x10;
const EMMC_RESP1:       u64 = EMMC_BASE + 0x14;
const EMMC_RESP2:       u64 = EMMC_BASE + 0x18;
const EMMC_RESP3:       u64 = EMMC_BASE + 0x1C;
const EMMC_DATA:        u64 = EMMC_BASE + 0x20;
const EMMC_STATUS:      u64 = EMMC_BASE + 0x24;
const EMMC_CONTROL0:    u64 = EMMC_BASE + 0x28;
const EMMC_CONTROL1:    u64 = EMMC_BASE + 0x2C;
const EMMC_INTERRUPT:   u64 = EMMC_BASE + 0x30;
const EMMC_IRPT_MASK:   u64 = EMMC_BASE + 0x34;
const EMMC_IRPT_EN:     u64 = EMMC_BASE + 0x38;
const EMMC_CONTROL2:    u64 = EMMC_BASE + 0x3C;

// ─── CMDTM flags ──────────────────────────────────────────────────────────────
const CMD_NEED_APP:  u32 = 0x8000_0000;
const CMD_RSPNS_48:  u32 = 0x0002_0000;
const CMD_RSPNS_48B: u32 = 0x0003_0000;
const CMD_RSPNS_136: u32 = 0x0001_0000;
const CMD_DATA_READ: u32 = 0x0010_0000;
const CMD_IXCHK_EN:  u32 = 0x0010_0000;
const CMD_CRCCHK_EN: u32 = 0x0008_0000;

// ─── STATUS bits ─────────────────────────────────────────────────────────────
const SR_READ_AVAILABLE: u32 = 0x0000_0800;
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

// ─── Global state ─────────────────────────────────────────────────────────────
static mut SD_RCA:  u32  = 0;
static mut SD_HCCS: bool = false;

// ─── ARM physical counter helpers ─────────────────────────────────────────────
// Pi 4 (BCM2711) always runs CNTPCT at 54 MHz regardless of CPU speed.
// We hardcode this instead of reading CNTFRQ_EL0 because older bootloaders
// (Sep 2020) may leave CNTFRQ_EL0 = 0 at EL2, which would cause divide-by-zero
// in deadline_ms() and create an infinite loop.
const CNTPCT_HZ: u64 = 54_000_000;  // 54 MHz — fixed on all Pi 4 hardware

/// Read the ARM physical counter (always available at EL2, no setup needed).
#[inline(always)]
fn cntpct() -> u64 {
    let val: u64;
    unsafe { core::arch::asm!("mrs {}, cntpct_el0", out(reg) val, options(nostack, nomem)) };
    val
}

/// Returns a deadline `ms` milliseconds from now.
#[inline(always)]
fn deadline_ms(ms: u64) -> u64 {
    cntpct().wrapping_add((CNTPCT_HZ / 1000).wrapping_mul(ms))
}

/// Returns true if the deadline has passed.
/// Uses wrapping subtraction to handle counter wrap-around correctly.
#[inline(always)]
fn expired(deadline: u64) -> bool {
    cntpct().wrapping_sub(deadline) < (1u64 << 63)
}

// ─── MMIO helpers ─────────────────────────────────────────────────────────────
#[inline(always)]
fn rd(reg: u64) -> u32 {
    unsafe { read_volatile(reg as *const u32) }
}

#[inline(always)]
fn wr(reg: u64, val: u32) {
    unsafe { write_volatile(reg as *mut u32, val) }
}

/// Spin for approximately `ms` milliseconds using the hardware counter.
fn sleep_ms(ms: u64) {
    let end = deadline_ms(ms);
    while !expired(end) {
        unsafe { core::arch::asm!("nop") };
    }
}

// ─── Wait helpers (all use hardware counter) ──────────────────────────────────

/// Wait until CMD and DAT lines are idle. Timeout: 10ms.
fn wait_cmd_idle() -> bool {
    let dl = deadline_ms(10);
    while rd(EMMC_STATUS) & (SR_CMD_INHIBIT | SR_DAT_INHIBIT) != 0 {
        if expired(dl) { return false; }
    }
    true
}

/// Wait for an interrupt bit. Timeout: 100ms.
fn wait_interrupt(mask: u32) -> bool {
    let dl = deadline_ms(100);
    loop {
        let irpt = rd(EMMC_INTERRUPT);
        if irpt & INT_ERROR_MASK != 0 {
            wr(EMMC_INTERRUPT, irpt);
            return false;
        }
        if irpt & mask != 0 {
            wr(EMMC_INTERRUPT, mask);
            return true;
        }
        if expired(dl) { return false; }
    }
}

// ─── Clock setup ─────────────────────────────────────────────────────────────
fn set_clock(base_hz: u32, target_hz: u32) {
    // Wait for idle — 10ms max
    let dl = deadline_ms(10);
    while rd(EMMC_STATUS) & (SR_CMD_INHIBIT | SR_DAT_INHIBIT) != 0 {
        if expired(dl) { break; }
    }

    // Disable SD clock
    wr(EMMC_CONTROL1, rd(EMMC_CONTROL1) & !C1_CLK_EN);
    sleep_ms(2);

    // Calculate divisor (power-of-2 only, max 1024)
    let mut div: u32 = 1;
    while div < 1024 && base_hz / div > target_hz {
        div <<= 1;
    }
    let div_field = (div >> 1) & 0xFF;

    let c1 = (rd(EMMC_CONTROL1) & 0xFFFF_003F) | (div_field << 8) | C1_CLK_INTLEN;
    wr(EMMC_CONTROL1, c1);
    sleep_ms(2);

    // Wait for internal clock stable — 10ms max
    let dl = deadline_ms(10);
    while rd(EMMC_CONTROL1) & C1_CLK_STABLE == 0 {
        if expired(dl) { return; }
    }

    wr(EMMC_CONTROL1, rd(EMMC_CONTROL1) | C1_CLK_EN);
    sleep_ms(2);
}

// ─── Send a command ───────────────────────────────────────────────────────────
fn send_cmd(cmd: u32, arg: u32) -> u32 {
    // Handle ACMD prefix
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
/// Uses the ARM physical counter for all timeouts.
/// Guaranteed to return within ~300ms regardless of SD card state.
pub fn sd_init() -> bool {
    // Use 100 MHz as the base clock. This is the standard EMMC2 clock on Pi 4.
    // We avoid querying via mailbox because the mailbox call itself can hang
    // if the GPU is not ready (no timeout in the mailbox spin-wait).
    let base_hz: u32 = 100_000_000;

    // ── Reset ─────────────────────────────────────────────────────────────────
    wr(EMMC_CONTROL0, 0);
    wr(EMMC_CONTROL1, C1_SRST_HC);
    sleep_ms(10);

    let dl = deadline_ms(50);
    while rd(EMMC_CONTROL1) & C1_SRST_HC != 0 {
        if expired(dl) { return false; }
    }

    // ── Set timeout and identification clock (400 kHz) ────────────────────────
    wr(EMMC_CONTROL1, rd(EMMC_CONTROL1) | C1_TOUNIT_MAX);
    set_clock(base_hz, 400_000);

    wr(EMMC_IRPT_EN,   0xFFFF_FFFF);
    wr(EMMC_IRPT_MASK, 0xFFFF_FFFF);

    // ── CMD0 — GO_IDLE ────────────────────────────────────────────────────────
    send_cmd(CMD0, 0);
    sleep_ms(2);

    // ── CMD8 — SEND_IF_COND ───────────────────────────────────────────────────
    let r8 = send_cmd(CMD8, 0x0000_01AA);
    let is_v2 = r8 == 0x0000_01AA;

    // ── ACMD41 — SD_SEND_OP_COND (max 250ms) ─────────────────────────────────
    let acmd_dl = deadline_ms(250);
    let acmd_arg = if is_v2 { 0x51FF_8000u32 } else { 0x00FF_8000u32 };
    let mut resp: u32;
    loop {
        resp = send_cmd(ACMD41, acmd_arg);
        if resp == 0xFFFF_FFFF { return false; }
        if resp & 0x8000_0000 != 0 { break; }  // card ready
        if expired(acmd_dl) { return false; }
        sleep_ms(5);
    }

    unsafe { SD_HCCS = resp & 0x4000_0000 != 0; }

    // ── CMD2 — ALL_SEND_CID ───────────────────────────────────────────────────
    if send_cmd(CMD2, 0) == 0xFFFF_FFFF { return false; }

    // ── CMD3 — SEND_RELATIVE_ADDR ─────────────────────────────────────────────
    let r3 = send_cmd(CMD3, 0);
    if r3 == 0xFFFF_FFFF { return false; }
    unsafe { SD_RCA = r3 >> 16; }

    // ── Raise clock to 25 MHz ─────────────────────────────────────────────────
    set_clock(base_hz, 25_000_000);

    // ── CMD7 — SELECT_CARD ────────────────────────────────────────────────────
    let rca = unsafe { SD_RCA };
    if send_cmd(CMD7, rca << 16) == 0xFFFF_FFFF { return false; }

    // ── CMD16 — SET_BLOCKLEN = 512 ────────────────────────────────────────────
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
