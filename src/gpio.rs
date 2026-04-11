// SPDX-License-Identifier: MIT
//
// gpio.rs — Minimal GPIO driver for Raspberry Pi 3/4
//
// Used for the activity LED (GPIO 42 on Pi 4, GPIO 47 on Pi 3)
// as a visual heartbeat when no serial cable or HDMI is available.
//
// Pi 4 BCM2711 GPIO base: 0xFE200000
// Pi 3 BCM2837 GPIO base: 0x3F200000

#[cfg(feature = "bsp_rpi3")]
const GPIO_BASE: usize = 0x3F20_0000;

#[cfg(feature = "bsp_rpi4")]
const GPIO_BASE: usize = 0xFE20_0000;

// GPIO Function Select registers (3 bits per pin, 10 pins per register)
const GPFSEL0: *mut u32 = (GPIO_BASE + 0x00) as *mut u32;
const GPFSEL1: *mut u32 = (GPIO_BASE + 0x04) as *mut u32;
const GPFSEL2: *mut u32 = (GPIO_BASE + 0x08) as *mut u32;
const GPFSEL3: *mut u32 = (GPIO_BASE + 0x0C) as *mut u32;
const GPFSEL4: *mut u32 = (GPIO_BASE + 0x10) as *mut u32;

// GPIO Output Set/Clear registers
const GPSET0: *mut u32 = (GPIO_BASE + 0x1C) as *mut u32;
const GPSET1: *mut u32 = (GPIO_BASE + 0x20) as *mut u32;
const GPCLR0: *mut u32 = (GPIO_BASE + 0x28) as *mut u32;
const GPCLR1: *mut u32 = (GPIO_BASE + 0x2C) as *mut u32;

// Activity LED pin
// Pi 4: GPIO 42 (in GPFSEL4, bit group 2)
// Pi 3: GPIO 47 (in GPFSEL4, bit group 7)
#[cfg(feature = "bsp_rpi4")]
const ACT_LED_PIN: u32 = 42;

#[cfg(feature = "bsp_rpi3")]
const ACT_LED_PIN: u32 = 47;

/// Configure the activity LED pin as output.
pub fn init_led() {
    unsafe {
        // GPFSEL register index = pin / 10
        // bit position = (pin % 10) * 3
        let reg_idx = ACT_LED_PIN / 10;
        let bit_pos = (ACT_LED_PIN % 10) * 3;

        let reg = match reg_idx {
            0 => GPFSEL0,
            1 => GPFSEL1,
            2 => GPFSEL2,
            3 => GPFSEL3,
            4 => GPFSEL4,
            _ => return,
        };

        let mut val = reg.read_volatile();
        val &= !(0b111 << bit_pos);  // clear the 3-bit field
        val |= 0b001 << bit_pos;     // set to output (001)
        reg.write_volatile(val);
    }
}

/// Turn the activity LED on.
pub fn led_on() {
    unsafe {
        if ACT_LED_PIN < 32 {
            GPSET0.write_volatile(1 << ACT_LED_PIN);
        } else {
            GPSET1.write_volatile(1 << (ACT_LED_PIN - 32));
        }
    }
}

/// Turn the activity LED off.
pub fn led_off() {
    unsafe {
        if ACT_LED_PIN < 32 {
            GPCLR0.write_volatile(1 << ACT_LED_PIN);
        } else {
            GPCLR1.write_volatile(1 << (ACT_LED_PIN - 32));
        }
    }
}

/// Busy-wait for approximately `ms` milliseconds.
/// Calibrated for ~1.5GHz ARM Cortex-A72 (Pi 4).
/// Not cycle-accurate — just good enough for LED blinking.
pub fn delay_ms(ms: u32) {
    // ~1500 iterations ≈ 1ms at 1.5GHz with simple loop overhead
    let count: u32 = ms * 1500;
    for _ in 0..count {
        unsafe { core::arch::asm!("nop", options(nomem, nostack)); }
    }
}

/// Blink the activity LED `n` times with a given on/off duration in ms.
pub fn blink(n: u32, on_ms: u32, off_ms: u32) {
    for _ in 0..n {
        led_on();
        delay_ms(on_ms);
        led_off();
        delay_ms(off_ms);
    }
}
