// emmc.rs — Bare-metal EMMC2 SD card driver for Raspberry Pi 4
// EMMC2 controller base: 0xFE340000 (Pi 4 / BCM2711)
//
// Key fixes vs earlier version:
//   1. Clock frequency queried from GPU mailbox (CLOCK_ID_EMMC2 = 12)
//      instead of hardcoded 100 MHz — actual value varies by firmware.
//   2. All spin-wait loops have tight iteration limits so they fail fast
//      (≤ 100ms total) instead of hanging for seconds.
//   3. ACMD41 retries reduced to 50 × 5ms = 250ms max.
//   4. sd_wait_for_interrupt timeout reduced to 100ms.

use core::ptr::{read_volatile, write_volatile};
use crate::mailbox;

// ─── EMMC2 Register Map (Pi 4 base: 0xFE340000) ──────────────────────────────
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
const EMMC_SLOTISR_VER: u64 = EMMC_BASE + 0xFC;

// ─── CMDTM command flags ──────────────────────────────────────────────────────
const CMD_NEED_APP:     u32 = 0x8000_0000;
const CMD_RSPNS_48:     u32 = 0x0002_0000;
const CMD_RSPNS_48B:    u32 = 0x0003_0000;
const CMD_RSPNS_136:    u32 = 0x0001_0000;
const CMD_DATA_READ:    u32 = 0x0010_0000;
const CMD_IXCHK_EN:     u32 = 0x0010_0000;
const CMD_CRCCHK_EN:    u32 = 0x0008_0000;

// ─── STATUS register bits ─────────────────────────────────────────────────────
const SR_READ_AVAILABLE: u32 = 0x0000_0800;
const SR_DAT_INHIBIT:    u32 = 0x0000_0002;
const SR_CMD_INHIBIT:    u32 = 0x0000_0001;
const SR_APP_CMD:        u32 = 0x0000_0020;

// ─── INTERRUPT register bits ──────────────────────────────────────────────────
const INT_DATA_TIMEOUT:  u32 = 0x0010_0000;
const INT_CMD_TIMEOUT:   u32 = 0x0001_0000;
const INT_READ_RDY:      u32 = 0x0000_0020;
const INT_CMD_DONE:      u32 = 0x0000_0001;
const INT_ERROR_MASK:    u32 = 0x017E_8000;

// ─── CONTROL1 bits ────────────────────────────────────────────────────────────
const C1_SRST_HC:        u32 = 0x0100_0000;
const C1_TOUNIT_MAX:     u32 = 0x000E_0000;
const C1_CLK_GENSEL:     u32 = 0x0000_0020;
const C1_CLK_EN:         u32 = 0x0000_0004;
const C1_CLK_STABLE:     u32 = 0x0000_0002;
const C1_CLK_INTLEN:     u32 = 0x0000_0001;

// ─── SD Commands ─────────────────────────────────────────────────────────────
const CMD0:  u32 = 0x0000_0000;
const CMD2:  u32 = 0x0200_0000 | CMD_RSPNS_136;
const CMD3:  u32 = 0x0300_0000 | CMD_RSPNS_48;
const CMD7:  u32 = 0x0700_0000 | CMD_RSPNS_48B;
const CMD8:  u32 = 0x0800_0000 | CMD_RSPNS_48;
const CMD16: u32 = 0x1000_0000 | CMD_RSPNS_48;
const CMD17: u32 = 0x1100_0000 | CMD_RSPNS_48
                 | CMD_DATA_READ | CMD_IXCHK_EN | CMD_CRCCHK_EN;
const CMD55: u32 = 0x3700_0000 | CMD_RSPNS_48;
const ACMD41: u32 = CMD_NEED_APP | 0x2900_0000 | CMD_RSPNS_48;

// ─── Mailbox tag for clock rate query ────────────────────────────────────────
// PROPTAG_GET_CLOCK_RATE = 0x00030002, CLOCK_ID_EMMC2 = 12
const MBOX_TAG_GET_CLOCK_RATE: u32 = 0x0003_0002;
const CLOCK_ID_EMMC2: u32 = 12;

// ─── Global SD card state ─────────────────────────────────────────────────────
static mut SD_RCA:  u32  = 0;
static mut SD_HCCS: bool = false;

// ─── Mailbox buffer for clock query ──────────────────────────────────────────
#[repr(C, align(16))]
struct ClockMbox([u32; 8]);
static mut CLOCK_MBOX: ClockMbox = ClockMbox([0u32; 8]);

// ─── Low-level MMIO helpers ───────────────────────────────────────────────────
#[inline(always)]
fn mmio_read(reg: u64) -> u32 {
    unsafe { read_volatile(reg as *const u32) }
}

#[inline(always)]
fn mmio_write(reg: u64, val: u32) {
    unsafe { write_volatile(reg as *mut u32, val) }
}

/// Spin-delay for approximately `n` microseconds.
fn delay_us(n: u32) {
    for _ in 0..(n * 150) {
        unsafe { core::arch::asm!("nop") };
    }
}

// ─── Query EMMC2 clock from GPU mailbox ───────────────────────────────────────
/// Returns the EMMC2 base clock in Hz as reported by the GPU firmware.
/// Falls back to 100 MHz if the mailbox call fails.
fn get_emmc2_clock_hz() -> u32 {
    unsafe {
        let m = &mut CLOCK_MBOX.0;
        m[0] = 8 * 4;                      // total size: 8 words = 32 bytes
        m[1] = mailbox::MBOX_REQUEST;
        m[2] = MBOX_TAG_GET_CLOCK_RATE;    // tag: get clock rate
        m[3] = 8;                          // value buffer size
        m[4] = 4;                          // request: 4 bytes (clock ID)
        m[5] = CLOCK_ID_EMMC2;             // clock ID 12 = EMMC2
        m[6] = 0;                          // GPU fills: clock rate in Hz
        m[7] = mailbox::MBOX_TAG_LAST;

        if mailbox::call(m.as_mut_ptr(), mailbox::MBOX_CH_PROP) && m[6] > 0 {
            m[6]  // actual EMMC2 clock in Hz
        } else {
            100_000_000  // fallback: 100 MHz
        }
    }
}

// ─── Wait helpers ─────────────────────────────────────────────────────────────

/// Wait until CMD and DAT lines are not inhibited. Timeout: ~10ms.
fn sd_wait_for_cmd() -> bool {
    let mut timeout = 10_000u32;  // 10ms at 1µs per iteration
    while mmio_read(EMMC_STATUS) & (SR_CMD_INHIBIT | SR_DAT_INHIBIT) != 0 {
        if timeout == 0 { return false; }
        timeout -= 1;
        delay_us(1);
    }
    true
}

/// Wait for a specific interrupt bit. Timeout: ~100ms.
fn sd_wait_for_interrupt(mask: u32) -> bool {
    let mut timeout = 100_000u32;  // 100ms at 1µs per iteration
    loop {
        let irpt = mmio_read(EMMC_INTERRUPT);
        if irpt & INT_ERROR_MASK != 0 {
            mmio_write(EMMC_INTERRUPT, irpt);
            return false;
        }
        if irpt & mask != 0 {
            mmio_write(EMMC_INTERRUPT, mask);
            return true;
        }
        if timeout == 0 { return false; }
        timeout -= 1;
        delay_us(1);
    }
}

// ─── Send a command ───────────────────────────────────────────────────────────
fn sd_send_command(cmd: u32, arg: u32) -> u32 {
    if cmd & CMD_NEED_APP != 0 {
        let rca = unsafe { SD_RCA };
        sd_send_command(CMD55, rca << 16);
        if mmio_read(EMMC_STATUS) & SR_APP_CMD == 0 {
            return 0xFFFF_FFFF;
        }
    }

    let cmdtm = cmd & !CMD_NEED_APP;

    if !sd_wait_for_cmd() { return 0xFFFF_FFFF; }

    mmio_write(EMMC_INTERRUPT, 0xFFFF_FFFF);
    mmio_write(EMMC_ARG1, arg);
    mmio_write(EMMC_CMDTM, cmdtm);

    if !sd_wait_for_interrupt(INT_CMD_DONE) { return 0xFFFF_FFFF; }

    mmio_read(EMMC_RESP0)
}

// ─── Clock setup ─────────────────────────────────────────────────────────────
fn sd_set_clock(base_hz: u32, target_hz: u32) {
    // Wait for CMD/DAT inhibit — max 10ms
    let mut timeout = 10_000u32;
    while mmio_read(EMMC_STATUS) & (SR_CMD_INHIBIT | SR_DAT_INHIBIT) != 0 {
        if timeout == 0 { break; }
        timeout -= 1;
        delay_us(1);
    }

    // Disable SD clock
    let ctrl1 = mmio_read(EMMC_CONTROL1);
    mmio_write(EMMC_CONTROL1, ctrl1 & !C1_CLK_EN);
    delay_us(10);

    // Calculate divisor
    let mut divisor: u32 = 1;
    while divisor < 2046 && base_hz / divisor > target_hz {
        divisor <<= 1;
    }
    let div_field = (divisor >> 1) & 0xFF;

    let ctrl1 = mmio_read(EMMC_CONTROL1);
    let ctrl1 = (ctrl1 & 0xFFFF_003F) | (div_field << 8) | C1_CLK_INTLEN;
    mmio_write(EMMC_CONTROL1, ctrl1);
    delay_us(10);

    // Wait for internal clock stable — max 10ms
    let mut timeout = 10_000u32;
    while mmio_read(EMMC_CONTROL1) & C1_CLK_STABLE == 0 {
        if timeout == 0 { return; }
        timeout -= 1;
        delay_us(1);
    }

    mmio_write(EMMC_CONTROL1, mmio_read(EMMC_CONTROL1) | C1_CLK_EN);
    delay_us(10);
}

// ─── Public API ───────────────────────────────────────────────────────────────

/// Initialize the EMMC2 controller and the SD card.
/// Returns `true` on success. Guaranteed to return within ~500ms.
pub fn sd_init() -> bool {
    // ── Step 1: Query actual EMMC2 clock from GPU ─────────────────────────────
    let base_hz = get_emmc2_clock_hz();

    // ── Step 2: Software reset ────────────────────────────────────────────────
    mmio_write(EMMC_CONTROL0, 0);
    mmio_write(EMMC_CONTROL1, C1_SRST_HC);
    delay_us(10_000);

    // Wait for reset — max 10ms
    let mut timeout = 10_000u32;
    while mmio_read(EMMC_CONTROL1) & C1_SRST_HC != 0 {
        if timeout == 0 { return false; }
        timeout -= 1;
        delay_us(1);
    }

    // ── Step 3: Set clock to 400 kHz (identification mode) ───────────────────
    mmio_write(EMMC_CONTROL1,
        mmio_read(EMMC_CONTROL1) | C1_TOUNIT_MAX);
    sd_set_clock(base_hz, 400_000);

    // Enable all interrupts
    mmio_write(EMMC_IRPT_EN,   0xFFFF_FFFF);
    mmio_write(EMMC_IRPT_MASK, 0xFFFF_FFFF);

    // ── Step 4: CMD0 — GO_IDLE_STATE ─────────────────────────────────────────
    sd_send_command(CMD0, 0);
    delay_us(1_000);

    // ── Step 5: CMD8 — SEND_IF_COND ──────────────────────────────────────────
    let resp = sd_send_command(CMD8, 0x0000_01AA);
    let is_v2 = resp == 0x0000_01AA;

    // ── Step 6: ACMD41 — SD_SEND_OP_COND ─────────────────────────────────────
    // Max 50 retries × 5ms = 250ms total
    let mut retries = 50u32;
    let mut resp: u32;
    loop {
        let arg = if is_v2 { 0x51FF_8000 } else { 0x00FF_8000 };
        resp = sd_send_command(ACMD41, arg);
        if resp == 0xFFFF_FFFF { return false; }  // command error
        if resp & 0x8000_0000 != 0 { break; }     // card ready
        if retries == 0 { return false; }
        retries -= 1;
        delay_us(5_000);
    }

    unsafe { SD_HCCS = resp & 0x4000_0000 != 0; }

    // ── Step 7: CMD2 — ALL_SEND_CID ──────────────────────────────────────────
    if sd_send_command(CMD2, 0) == 0xFFFF_FFFF { return false; }

    // ── Step 8: CMD3 — SEND_RELATIVE_ADDR ────────────────────────────────────
    let rca_resp = sd_send_command(CMD3, 0);
    if rca_resp == 0xFFFF_FFFF { return false; }
    unsafe { SD_RCA = rca_resp >> 16; }

    // ── Step 9: Raise clock to 25 MHz ────────────────────────────────────────
    sd_set_clock(base_hz, 25_000_000);

    // ── Step 10: CMD7 — SELECT_CARD ──────────────────────────────────────────
    let rca = unsafe { SD_RCA };
    if sd_send_command(CMD7, rca << 16) == 0xFFFF_FFFF { return false; }

    // ── Step 11: CMD16 — SET_BLOCKLEN to 512 bytes ───────────────────────────
    let resp = sd_send_command(CMD16, 512);
    if resp & 0xFFFF_0000 != 0 { return false; }

    true
}

/// Read a single 512-byte block from the SD card.
pub fn sd_read_block(lba: u32, buf: &mut [u8; 512]) -> bool {
    let arg = if unsafe { SD_HCCS } { lba } else { lba * 512 };

    mmio_write(EMMC_BLKSIZECNT, (1 << 16) | 512);

    let resp = sd_send_command(CMD17, arg);
    if resp & 0xFFFF_0000 != 0 { return false; }

    if !sd_wait_for_interrupt(INT_READ_RDY) { return false; }

    for i in (0..512).step_by(4) {
        let word = mmio_read(EMMC_DATA);
        buf[i]   = (word & 0xFF) as u8;
        buf[i+1] = ((word >> 8) & 0xFF) as u8;
        buf[i+2] = ((word >> 16) & 0xFF) as u8;
        buf[i+3] = ((word >> 24) & 0xFF) as u8;
    }

    sd_wait_for_interrupt(INT_CMD_DONE);
    true
}
