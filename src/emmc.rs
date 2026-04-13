// EMMC2 driver for Raspberry Pi 4 (BCM2711)
//
// VALIDATED against:
//   1. Circle/addon/SDCard/emmc.cpp (R. Stange) — proven working on Pi 4
//   2. jncronin/rpi-boot emmc.c — full SDHCI 3.0 reference
//   3. bztsrc/raspi3-tutorial sd.c — command constant reference
//   4. BCM2711 ARM Peripherals datasheet (EMMC2 = SDHCI 3.0 at 0x7E340000)
//   5. Python emulator (bcm2711_emmc_emulator.py) — sequence validated
//
// Pi 4 specific requirements (NOT needed on Pi 1-3):
//   A. Mailbox: disable 1.8V supply (GPIO 132 = 0) before init
//   B. CONTROL0 bits [11:8] = 0x0F (VDD1 = 3.3V) after reset
//   C. Mailbox clock ID = 12 (CLOCK_ID_EMMC2), NOT 1 (CLOCK_ID_EMMC)
//
// EMMC2 base: 0xFE340000 (ARM physical = BCM bus 0x7E340000)
// Confirmed via bcm2711-rpi-4-b.dtb: /emmc2bus/mmc@7e340000

// ─── EMMC2 register addresses ─────────────────────────────────────────────────
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

// ─── STATUS register bits ─────────────────────────────────────────────────────
const SR_CMD_INHIBIT:  u32 = 1 << 0;  // Command line busy
const SR_DAT_INHIBIT:  u32 = 1 << 1;  // Data line busy
const SR_APP_CMD:      u32 = 1 << 5;  // Card in APP_CMD mode

// ─── INTERRUPT register bits ──────────────────────────────────────────────────
const INT_CMD_DONE:    u32 = 1 << 0;   // Command complete
const INT_DATA_DONE:   u32 = 1 << 1;   // Data transfer complete
const INT_READ_RDY:    u32 = 1 << 5;   // Buffer read ready
const INT_CMD_TIMEOUT: u32 = 1 << 16;  // Command timeout
const INT_DATA_TIMEOUT:u32 = 1 << 20;  // Data timeout
// Error mask: bits [31:16] = all error bits
const INT_ERROR_MASK:  u32 = 0xFFFF_0000;

// ─── CONTROL0 register bits ───────────────────────────────────────────────────
// [11:8] SD Bus Voltage VDD1: 0x07=3.3V (spec), 0x0F=3.3V (Circle/Pi4 value)
const C0_VDD1_3V3:     u32 = 0x0F << 8;  // 3.3V bus power (Pi 4 uses 0x0F)

// ─── CONTROL1 register bits ───────────────────────────────────────────────────
const C1_CLK_INTLEN:   u32 = 1 << 0;   // Internal clock enable
const C1_CLK_STABLE:   u32 = 1 << 1;   // Internal clock stable
const C1_CLK_EN:       u32 = 1 << 2;   // SD clock enable
// Data timeout: bits [19:16]. Value 11 = TMCLK * 2^24 (Circle uses 11 for Pi 4)
const C1_TOUNIT_MAX:   u32 = 11 << 16; // TMCLK * 2^24 (validated: Circle emmc.cpp)
const C1_SRST_HC:      u32 = 1 << 24;  // Reset entire host controller

// ─── CMDTM bits (SDHCI 3.0 spec, verified against rpi_boot_emmc.c) ───────────
const CMD_ISDATA:      u32 = 1 << 21;  // Data present select
const CMD_IXCHK_EN:    u32 = 1 << 20;  // Index check enable
const CMD_CRCCHK_EN:   u32 = 1 << 19;  // CRC check enable
const CMD_RSPNS_NONE:  u32 = 0 << 16;  // No response
const CMD_RSPNS_136:   u32 = 1 << 16;  // 136-bit response (R2)
const CMD_RSPNS_48:    u32 = 2 << 16;  // 48-bit response (R1, R6, R7)
const CMD_RSPNS_48B:   u32 = 3 << 16;  // 48-bit response + busy (R1b)
const CMD_DAT_DIR_RD:  u32 = 1 << 4;   // Data direction: card to host (read)
const CMD_NEED_APP:    u32 = 1 << 31;  // Internal flag: prefix with CMD55

// ─── SD command codes (CMDTM values, verified against both references) ────────
const CMD0:   u32 = 0 << 24;  // GO_IDLE_STATE — no response
const CMD2:   u32 = (2 << 24) | CMD_RSPNS_136 | CMD_CRCCHK_EN;
const CMD3:   u32 = (3 << 24) | CMD_RSPNS_48 | CMD_IXCHK_EN | CMD_CRCCHK_EN;
const CMD7:   u32 = (7 << 24) | CMD_RSPNS_48B | CMD_CRCCHK_EN;
const CMD8:   u32 = (8 << 24) | CMD_RSPNS_48 | CMD_IXCHK_EN | CMD_CRCCHK_EN;
const CMD16:  u32 = (16 << 24) | CMD_RSPNS_48 | CMD_CRCCHK_EN;
// CMD17: READ_SINGLE_BLOCK
// = (17<<24) | R1(RESP48|CRC|IX) | DATA_PRESENT | READ
// = 0x11000000 | 0x00020000 | 0x00080000 | 0x00100000 | 0x00200000 | 0x00000010
// = 0x113a0010  (verified against rpi_boot_emmc.c and emulator)
const CMD17:  u32 = (17 << 24) | CMD_RSPNS_48 | CMD_CRCCHK_EN | CMD_IXCHK_EN
                  | CMD_ISDATA | CMD_DAT_DIR_RD;
const CMD55:  u32 = (55 << 24) | CMD_RSPNS_48 | CMD_IXCHK_EN | CMD_CRCCHK_EN;
// ACMD41: no CRC/index check, 48-bit R3 response
const ACMD41: u32 = CMD_NEED_APP | (41 << 24) | CMD_RSPNS_48;

// ─── Timing ───────────────────────────────────────────────────────────────────
// Pi 4 Cortex-A72 @ 1.5 GHz: ~1,500,000 NOPs ≈ 1 ms
const ITERS_PER_MS: u32 = 1_500_000;

// ─── Card state ───────────────────────────────────────────────────────────────
static mut SD_RCA:  u32 = 0;
static mut SD_HCCS: bool = false;

// ─── Register I/O ─────────────────────────────────────────────────────────────
#[inline(always)]
fn rd(reg: u64) -> u32 {
    unsafe { core::ptr::read_volatile(reg as *const u32) }
}

#[inline(always)]
fn wr(reg: u64, val: u32) {
    unsafe { core::ptr::write_volatile(reg as *mut u32, val) }
}

#[inline(always)]
fn sleep_ms(ms: u32) {
    let iters = ms as u64 * ITERS_PER_MS as u64;
    for _ in 0..iters {
        unsafe { core::arch::asm!("nop", options(nostack, nomem)) };
    }
}

// ─── Pi 4: Disable 1.8V supply via VideoCore mailbox ─────────────────────────
// Mailbox tag 0x00038041 = PROPTAG_SET_SET_GPIO_STATE
// GPIO 132 = EXP_GPIO_BASE(128) + 4 — controls SD card 1.8V supply
// Setting it to 0 ensures the card is powered at 3.3V.
// Source: Circle/addon/SDCard/emmc.cpp lines 541-549
//
// IMPORTANT: Must use static mut buffer, NOT stack-allocated struct.
// Rust optimizer eliminates stack-allocated mailbox buffers because it
// cannot see that the MMIO write_volatile calls have side effects on
// the VideoCore GPU. Using static mut forces the buffer into BSS and
// prevents dead-code elimination.
#[repr(C, align(16))]
struct Mbox18vBuf {
    size:    u32,
    code:    u32,
    tag:     u32,
    buf_sz:  u32,
    req_sz:  u32,
    gpio:    u32,
    state:   u32,
    end:     u32,
}
static mut MBOX_18V_BUF: Mbox18vBuf = Mbox18vBuf {
    size: 32, code: 0, tag: 0x0003_8041,
    buf_sz: 8, req_sz: 8, gpio: 132, state: 0, end: 0,
};

#[inline(never)]
fn disable_18v_supply() {
    let phys = unsafe { &MBOX_18V_BUF as *const Mbox18vBuf as u64 };
    // Re-initialize (in case of re-entry)
    unsafe {
        core::ptr::write_volatile(&mut MBOX_18V_BUF.code, 0);
        core::ptr::write_volatile(&mut MBOX_18V_BUF.state, 0);
    }
    unsafe { core::arch::asm!("dsb sy", options(nostack, nomem)) };
    const MBOX_BASE:   u64 = 0xFE00_B880;
    const MBOX_STATUS: u64 = MBOX_BASE + 0x18;
    const MBOX_WRITE:  u64 = MBOX_BASE + 0x20;
    const MBOX_READ:   u64 = MBOX_BASE + 0x00;
    const MBOX_FULL:   u32 = 0x8000_0000;
    const MBOX_EMPTY:  u32 = 0x4000_0000;
    const MBOX_CH:     u32 = 8;
    let mut t = 10_000u32;
    while rd(MBOX_STATUS) & MBOX_FULL != 0 {
        t -= 1; if t == 0 { return; }
    }
    wr(MBOX_WRITE, ((phys & !0xF) as u32) | MBOX_CH);
    t = 100_000;
    loop {
        while rd(MBOX_STATUS) & MBOX_EMPTY != 0 {
            t -= 1; if t == 0 { return; }
        }
        let r = rd(MBOX_READ);
        if (r & 0xF) == MBOX_CH { break; }
    }
    unsafe { core::arch::asm!("dsb sy", options(nostack, nomem)) };
    // Ignore return value — if it fails, we continue anyway
    // Read response code to ensure the mailbox transaction completed
    let _ = unsafe { core::ptr::read_volatile(&MBOX_18V_BUF.code) };
}

// ─── Query actual EMMC2 clock from VideoCore mailbox ─────────────────────────
// Mailbox tag 0x00030002: GET_CLOCK_RATE
// CLOCK_ID_EMMC2 = 12 (0x0C) — Pi 4 specific.
// Pi 1-3 used CLOCK_ID_EMMC = 1, which returns 100 MHz.
// Pi 4 EMMC2 clock is set by firmware to 200 MHz.
// Source: Circle/include/circle/bcmpropertytags.h CLOCK_ID_EMMC2=12
//
// IMPORTANT: static mut buffer required (see disable_18v_supply comment)
#[repr(C, align(16))]
struct MboxClkBuf {
    size:    u32,
    code:    u32,
    tag:     u32,
    buf_sz:  u32,
    req_sz:  u32,
    clk_id:  u32,
    clk_hz:  u32,
    end:     u32,
}
static mut MBOX_CLK_BUF: MboxClkBuf = MboxClkBuf {
    size: 32, code: 0, tag: 0x0003_0002,
    buf_sz: 8, req_sz: 4,
    clk_id: 12,  // CLOCK_ID_EMMC2 — MUST be 12 for Pi 4, NOT 1
    clk_hz: 0, end: 0,
};

#[inline(never)]
fn get_emmc_clock_hz() -> u32 {
    let phys = unsafe { &MBOX_CLK_BUF as *const MboxClkBuf as u64 };
    unsafe {
        core::ptr::write_volatile(&mut MBOX_CLK_BUF.code, 0);
        core::ptr::write_volatile(&mut MBOX_CLK_BUF.clk_hz, 0);
    }
    unsafe { core::arch::asm!("dsb sy", options(nostack, nomem)) };
    const MBOX_BASE:   u64 = 0xFE00_B880;
    const MBOX_STATUS: u64 = MBOX_BASE + 0x18;
    const MBOX_WRITE:  u64 = MBOX_BASE + 0x20;
    const MBOX_READ:   u64 = MBOX_BASE + 0x00;
    const MBOX_FULL:   u32 = 0x8000_0000;
    const MBOX_EMPTY:  u32 = 0x4000_0000;
    const MBOX_CH:     u32 = 8;
    let mut t = 10_000u32;
    while rd(MBOX_STATUS) & MBOX_FULL != 0 {
        t -= 1; if t == 0 { return 200_000_000; }
    }
    wr(MBOX_WRITE, ((phys & !0xF) as u32) | MBOX_CH);
    t = 100_000;
    loop {
        while rd(MBOX_STATUS) & MBOX_EMPTY != 0 {
            t -= 1; if t == 0 { return 200_000_000; }
        }
        let r = rd(MBOX_READ);
        if (r & 0xF) == MBOX_CH { break; }
    }
    unsafe { core::arch::asm!("dsb sy", options(nostack, nomem)) };
    let hz = unsafe { core::ptr::read_volatile(&MBOX_CLK_BUF.clk_hz) };
    if hz > 0 && hz <= 400_000_000 { hz } else { 200_000_000 }
}

// ─── Clock divisor calculation (SDHCI 3.0 10-bit divided clock mode) ─────────
fn calc_clock_field(base_hz: u32, target_hz: u32) -> u32 {
    if target_hz >= base_hz { return 0; }
    let mut divisor = (base_hz + 2 * target_hz - 1) / (2 * target_hz);
    if divisor > 0x3FF { divisor = 0x3FF; }
    let lo = (divisor & 0xFF) << 8;
    let hi = ((divisor >> 8) & 0x3) << 6;
    lo | hi
}

// ─── Wait for CMD and DAT lines idle ─────────────────────────────────────────
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

// ─── Wait for interrupt bit, return false on error or timeout ─────────────────
fn wait_interrupt(mask: u32) -> bool {
    let limit = 200 * ITERS_PER_MS;
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

// ─── Set SD clock frequency ───────────────────────────────────────────────────
fn set_clock(base_hz: u32, target_hz: u32) {
    let limit = 10 * ITERS_PER_MS;
    for _ in 0..limit {
        if rd(EMMC_STATUS) & (SR_CMD_INHIBIT | SR_DAT_INHIBIT) == 0 { break; }
        unsafe { core::arch::asm!("nop", options(nostack, nomem)) };
    }
    // Disable SD clock
    wr(EMMC_CONTROL1, rd(EMMC_CONTROL1) & !C1_CLK_EN);
    sleep_ms(2);
    // Write new divisor
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
    // Enable SD clock
    wr(EMMC_CONTROL1, rd(EMMC_CONTROL1) | C1_CLK_EN);
    sleep_ms(2);
}

// ─── Send a command ───────────────────────────────────────────────────────────
fn send_cmd(cmd: u32, arg: u32) -> u32 {
    if cmd & CMD_NEED_APP != 0 {
        let rca = unsafe { SD_RCA };
        let cmd55_flags = if rca != 0 {
            CMD55
        } else {
            (55 << 24) | CMD_RSPNS_48
        };
        let r = send_cmd(cmd55_flags, rca << 16);
        if r == 0xFFFF_FFFF { return 0xFFFF_FFFF; }
        if rca != 0 && rd(EMMC_STATUS) & SR_APP_CMD == 0 {
            return 0xFFFF_FFFF;
        }
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
/// Returns true on success.
pub fn sd_init() -> bool {
    // ── Pi 4 Step 0: Disable 1.8V supply ─────────────────────────────────────
    // REQUIRED on Pi 4. Sets GPIO 132 = 0 via mailbox to ensure 3.3V operation.
    // Source: Circle/addon/SDCard/emmc.cpp lines 541-549
    disable_18v_supply();
    sleep_ms(5);

    // ── Step 1: Query actual EMMC2 clock (clock_id=12) ───────────────────────
    let base_hz = get_emmc_clock_hz();

    // ── Step 2: Reset host controller ────────────────────────────────────────
    wr(EMMC_CONTROL2, 0);
    wr(EMMC_CONTROL0, 0);
    wr(EMMC_CONTROL1, C1_SRST_HC);
    sleep_ms(10);
    let limit = 100 * ITERS_PER_MS;
    let mut reset_ok = false;
    for _ in 0..limit {
        if rd(EMMC_CONTROL1) & C1_SRST_HC == 0 { reset_ok = true; break; }
        unsafe { core::arch::asm!("nop", options(nostack, nomem)) };
    }
    if !reset_ok { return false; }

    // ── Pi 4 Step 3: Enable SD Bus Power VDD1 at 3.3V ────────────────────────
    // REQUIRED on Pi 4 BCM2711 EMMC2. Without this, no commands will work.
    // Source: Circle/addon/SDCard/emmc.cpp lines 1449-1451
    wr(EMMC_CONTROL0, rd(EMMC_CONTROL0) | C0_VDD1_3V3);
    sleep_ms(2);

    // ── Step 4: Set timeout and identification clock (400 kHz) ────────────────
    wr(EMMC_CONTROL1, rd(EMMC_CONTROL1) | C1_TOUNIT_MAX);
    set_clock(base_hz, 400_000);

    // ── Step 5: Enable interrupts ─────────────────────────────────────────────
    wr(EMMC_IRPT_EN,   0xFFFF_FFFF);
    wr(EMMC_IRPT_MASK, 0xFFFF_FFFF);

    // ── Step 6: CMD0 — GO_IDLE_STATE ─────────────────────────────────────────
    send_cmd(CMD0, 0);
    sleep_ms(2);

    // ── Step 7: CMD8 — SEND_IF_COND ──────────────────────────────────────────
    let r8 = send_cmd(CMD8, 0x0000_01AA);
    let is_v2 = r8 == 0x0000_01AA;

    // ── Step 8: ACMD41 — SD_SEND_OP_COND ─────────────────────────────────────
    let acmd_arg = if is_v2 { 0x51FF_8000u32 } else { 0x00FF_8000u32 };
    let mut resp: u32 = 0;
    let mut card_ready = false;
    for _ in 0..50u32 {
        resp = send_cmd(ACMD41, acmd_arg);
        if resp == 0xFFFF_FFFF { return false; }
        if resp & 0x8000_0000 != 0 {
            card_ready = true;
            break;
        }
        sleep_ms(5);
    }
    if !card_ready { return false; }
    unsafe { SD_HCCS = resp & 0x4000_0000 != 0; }

    // ── Step 9: CMD2 — ALL_SEND_CID ──────────────────────────────────────────
    if send_cmd(CMD2, 0) == 0xFFFF_FFFF { return false; }

    // ── Step 10: CMD3 — SEND_RELATIVE_ADDR ───────────────────────────────────
    let r3 = send_cmd(CMD3, 0);
    if r3 == 0xFFFF_FFFF { return false; }
    unsafe { SD_RCA = r3 >> 16; }

    // ── Step 11: Raise clock to 25 MHz ───────────────────────────────────────
    set_clock(base_hz, 25_000_000);

    // ── Step 12: CMD7 — SELECT_CARD ──────────────────────────────────────────
    let rca = unsafe { SD_RCA };
    if send_cmd(CMD7, rca << 16) == 0xFFFF_FFFF { return false; }

    // ── Step 13: CMD16 — SET_BLOCKLEN = 512 (SDSC only) ──────────────────────
    if !unsafe { SD_HCCS } {
        let r16 = send_cmd(CMD16, 512);
        if r16 & 0xFFFF_0000 != 0 { return false; }
    }

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

    wait_interrupt(INT_DATA_DONE);

    true
}
