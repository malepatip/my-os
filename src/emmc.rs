// emmc.rs — Bare-metal EMMC2 SD card driver for Raspberry Pi 4
// EMMC2 controller base: 0xFE340000 (Pi 4 / BCM2711)
//
// This driver implements the SD card initialization sequence:
//   CMD0  → GO_IDLE_STATE
//   CMD8  → SEND_IF_COND  (detect SDHC/SDXC)
//   ACMD41→ SD_SEND_OP_COND (negotiate voltage + capacity)
//   CMD2  → ALL_SEND_CID
//   CMD3  → SEND_RELATIVE_ADDR
//   CMD7  → SELECT_CARD
//   CMD16 → SET_BLOCKLEN (512 bytes)
//   CMD17 → READ_SINGLE_BLOCK

use core::ptr::{read_volatile, write_volatile};

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
const CMD_NEED_APP:     u32 = 0x8000_0000; // internal flag: send ACMD prefix
const CMD_RSPNS_48:     u32 = 0x0002_0000; // 48-bit response
const CMD_RSPNS_48B:    u32 = 0x0003_0000; // 48-bit response with busy
const CMD_RSPNS_136:    u32 = 0x0001_0000; // 136-bit response
const CMD_DATA_READ:    u32 = 0x0010_0000; // data transfer, card to host
const CMD_IXCHK_EN:     u32 = 0x0010_0000; // command index check enable
const CMD_CRCCHK_EN:    u32 = 0x0008_0000; // CRC check enable

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
const C1_SRST_HC:        u32 = 0x0100_0000; // software reset for all
const C1_TOUNIT_MAX:     u32 = 0x000E_0000; // timeout unit max
const C1_CLK_GENSEL:     u32 = 0x0000_0020; // clock generator select
const C1_CLK_EN:         u32 = 0x0000_0004; // SD clock enable
const C1_CLK_STABLE:     u32 = 0x0000_0002; // internal clock stable
const C1_CLK_INTLEN:     u32 = 0x0000_0001; // internal clock enable

// ─── SD Commands ─────────────────────────────────────────────────────────────
const CMD0:  u32 = 0x0000_0000;                              // GO_IDLE_STATE
const CMD2:  u32 = 0x0200_0000 | CMD_RSPNS_136;             // ALL_SEND_CID
const CMD3:  u32 = 0x0300_0000 | CMD_RSPNS_48;              // SEND_RELATIVE_ADDR
const CMD7:  u32 = 0x0700_0000 | CMD_RSPNS_48B;             // SELECT_CARD
const CMD8:  u32 = 0x0800_0000 | CMD_RSPNS_48;              // SEND_IF_COND
const CMD16: u32 = 0x1000_0000 | CMD_RSPNS_48;              // SET_BLOCKLEN
const CMD17: u32 = 0x1100_0000 | CMD_RSPNS_48                // READ_SINGLE_BLOCK
                 | CMD_DATA_READ | CMD_IXCHK_EN | CMD_CRCCHK_EN;
const CMD55: u32 = 0x3700_0000 | CMD_RSPNS_48;              // APP_CMD prefix
const ACMD41: u32 = CMD_NEED_APP | 0x2900_0000 | CMD_RSPNS_48; // SD_SEND_OP_COND

// ─── Global SD card state ─────────────────────────────────────────────────────
static mut SD_RCA: u32 = 0;      // Relative Card Address
static mut SD_HCCS: bool = false; // High Capacity Card (SDHC/SDXC)

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
/// On Pi 4 Cortex-A72 at ~1.5 GHz, each iteration ≈ 1 ns → 1000 iterations ≈ 1 µs.
fn delay_us(n: u32) {
    for _ in 0..(n * 150) {
        unsafe { core::arch::asm!("nop") };
    }
}

// ─── Wait helpers ─────────────────────────────────────────────────────────────

/// Wait until the CMD and DAT lines are not inhibited.
fn sd_wait_for_cmd() -> bool {
    let mut timeout = 1_000_000u32;
    while mmio_read(EMMC_STATUS) & (SR_CMD_INHIBIT | SR_DAT_INHIBIT) != 0 {
        if timeout == 0 { return false; }
        timeout -= 1;
        delay_us(1);
    }
    true
}

/// Wait for a specific interrupt bit, return true on success.
fn sd_wait_for_interrupt(mask: u32) -> bool {
    let mut timeout = 1_000_000u32;
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

/// Send an SD command with a 32-bit argument.
/// Returns the RESP0 register value (for R1/R3/R6 responses).
fn sd_send_command(cmd: u32, arg: u32) -> u32 {
    // If this is an application command (ACMD), send CMD55 first
    if cmd & CMD_NEED_APP != 0 {
        let rca = unsafe { SD_RCA };
        sd_send_command(CMD55, rca << 16);
        // Check APP_CMD status bit
        if mmio_read(EMMC_STATUS) & SR_APP_CMD == 0 {
            return 0xFFFF_FFFF; // error
        }
    }

    let cmdtm = cmd & !CMD_NEED_APP; // strip our internal flag

    if !sd_wait_for_cmd() { return 0xFFFF_FFFF; }

    // Clear interrupts
    mmio_write(EMMC_INTERRUPT, 0xFFFF_FFFF);

    // Write argument then command
    mmio_write(EMMC_ARG1, arg);
    mmio_write(EMMC_CMDTM, cmdtm);

    // Wait for command complete
    if !sd_wait_for_interrupt(INT_CMD_DONE) { return 0xFFFF_FFFF; }

    mmio_read(EMMC_RESP0)
}

// ─── Clock setup ─────────────────────────────────────────────────────────────

/// Set the EMMC clock to approximately `target_hz`.
/// The EMMC2 input clock on Pi 4 is 100 MHz.
fn sd_set_clock(target_hz: u32) {
    // Wait for CMD/DAT inhibit to clear
    let mut timeout = 100_000u32;
    while mmio_read(EMMC_STATUS) & (SR_CMD_INHIBIT | SR_DAT_INHIBIT) != 0 {
        if timeout == 0 { break; }
        timeout -= 1;
        delay_us(1);
    }

    // Disable SD clock
    let ctrl1 = mmio_read(EMMC_CONTROL1);
    mmio_write(EMMC_CONTROL1, ctrl1 & !C1_CLK_EN);
    delay_us(10);

    // Calculate divisor: EMMC2 base clock = 100 MHz
    let base_hz: u32 = 100_000_000;
    let mut divisor: u32 = 1;
    while divisor < 1024 && base_hz / divisor > target_hz {
        divisor <<= 1;
    }
    let div_field = (divisor >> 1) & 0xFF; // SDCLK Frequency Select field

    let ctrl1 = mmio_read(EMMC_CONTROL1);
    let ctrl1 = (ctrl1 & 0xFFFF_003F) | (div_field << 8) | C1_CLK_INTLEN;
    mmio_write(EMMC_CONTROL1, ctrl1);
    delay_us(10);

    // Wait for internal clock stable
    let mut timeout = 10_000u32;
    while mmio_read(EMMC_CONTROL1) & C1_CLK_STABLE == 0 {
        if timeout == 0 { return; }
        timeout -= 1;
        delay_us(10);
    }

    // Enable SD clock
    mmio_write(EMMC_CONTROL1, mmio_read(EMMC_CONTROL1) | C1_CLK_EN);
    delay_us(10);
}

// ─── Public API ───────────────────────────────────────────────────────────────

/// Initialize the EMMC2 controller and the SD card.
/// Returns `true` on success.
pub fn sd_init() -> bool {
    // ── Step 1: Software reset ────────────────────────────────────────────────
    mmio_write(EMMC_CONTROL0, 0);
    mmio_write(EMMC_CONTROL1, C1_SRST_HC);
    delay_us(10_000);

    // Wait for reset to complete
    let mut timeout = 100_000u32;
    while mmio_read(EMMC_CONTROL1) & C1_SRST_HC != 0 {
        if timeout == 0 { return false; }
        timeout -= 1;
        delay_us(1);
    }

    // ── Step 2: Set clock to 400 kHz (identification mode) ───────────────────
    mmio_write(EMMC_CONTROL1,
        mmio_read(EMMC_CONTROL1) | C1_TOUNIT_MAX);
    sd_set_clock(400_000);

    // Enable all interrupts
    mmio_write(EMMC_IRPT_EN,   0xFFFF_FFFF);
    mmio_write(EMMC_IRPT_MASK, 0xFFFF_FFFF);

    // ── Step 3: CMD0 — GO_IDLE_STATE ─────────────────────────────────────────
    sd_send_command(CMD0, 0);
    delay_us(1_000);

    // ── Step 4: CMD8 — SEND_IF_COND ──────────────────────────────────────────
    // Arg: VHS=1 (2.7-3.6V), check pattern=0xAA
    let resp = sd_send_command(CMD8, 0x0000_01AA);
    let is_v2 = resp == 0x0000_01AA; // echo back means V2 card

    // ── Step 5: ACMD41 — SD_SEND_OP_COND ─────────────────────────────────────
    // Negotiate: HCS=1 (support SDHC/SDXC), voltage 3.3V
    let mut retries = 100u32;
    let mut resp: u32;
    loop {
        let arg = if is_v2 { 0x51FF_8000 } else { 0x00FF_8000 };
        resp = sd_send_command(ACMD41, arg);
        if resp & 0x8000_0000 != 0 { break; } // card ready
        if retries == 0 { return false; }
        retries -= 1;
        delay_us(10_000);
    }

    // Check if card is high-capacity (SDHC/SDXC)
    unsafe { SD_HCCS = resp & 0x4000_0000 != 0; }

    // ── Step 6: CMD2 — ALL_SEND_CID ──────────────────────────────────────────
    sd_send_command(CMD2, 0);

    // ── Step 7: CMD3 — SEND_RELATIVE_ADDR ────────────────────────────────────
    let rca_resp = sd_send_command(CMD3, 0);
    unsafe { SD_RCA = rca_resp >> 16; }

    // ── Step 8: Raise clock to 25 MHz (data transfer mode) ───────────────────
    sd_set_clock(25_000_000);

    // ── Step 9: CMD7 — SELECT_CARD ───────────────────────────────────────────
    let rca = unsafe { SD_RCA };
    sd_send_command(CMD7, rca << 16);

    // ── Step 10: CMD16 — SET_BLOCKLEN to 512 bytes ───────────────────────────
    let resp = sd_send_command(CMD16, 512);
    if resp & 0xFFFF_0000 != 0 { return false; } // error bits set

    true
}

/// Read a single 512-byte block from the SD card.
/// `lba` is the logical block address (sector number).
/// `buf` must be exactly 512 bytes.
/// Returns `true` on success.
pub fn sd_read_block(lba: u32, buf: &mut [u8; 512]) -> bool {
    // For SDSC cards, address is byte-based; for SDHC/SDXC it's block-based
    let arg = if unsafe { SD_HCCS } { lba } else { lba * 512 };

    // Set block size and count
    mmio_write(EMMC_BLKSIZECNT, (1 << 16) | 512); // 1 block of 512 bytes

    // Send CMD17 — READ_SINGLE_BLOCK
    let resp = sd_send_command(CMD17, arg);
    if resp & 0xFFFF_0000 != 0 { return false; }

    // Wait for data ready
    if !sd_wait_for_interrupt(INT_READ_RDY) { return false; }

    // Read 512 bytes (128 × 32-bit words) from the DATA register
    for i in (0..512).step_by(4) {
        let word = mmio_read(EMMC_DATA);
        buf[i]   = (word & 0xFF) as u8;
        buf[i+1] = ((word >> 8) & 0xFF) as u8;
        buf[i+2] = ((word >> 16) & 0xFF) as u8;
        buf[i+3] = ((word >> 24) & 0xFF) as u8;
    }

    // Wait for transfer complete
    sd_wait_for_interrupt(INT_CMD_DONE);

    true
}
