// usb_hid.rs — Rust FFI bindings to Circle's USB HID keyboard and mouse drivers
//
// On Pi 4 (bsp_rpi4 feature): calls into circle_usb_shim.cpp which drives
//   Circle's full xHCI/VL805 USB stack — the only production-quality bare-metal
//   USB host implementation available for Pi 4.
//
// On Pi 3 / QEMU (bsp_rpi3 feature): all functions are no-op stubs so the
//   kernel still compiles and boots cleanly in QEMU.

// ─── C struct layout — must match CMouseState in circle_usb_shim.cpp ─────────

#[repr(C)]
pub struct MouseState {
    pub buttons: u32,
    pub x:       i32,
    pub y:       i32,
    pub dx:      i32,
    pub dy:      i32,
}

// ─── FFI declarations (Pi 4 only) ─────────────────────────────────────────────

#[cfg(feature = "bsp_rpi4")]
mod ffi {
    use super::MouseState;

    extern "C" {
        /// Initialise Circle's interrupt system, timer, and xHCI host controller.
        /// Returns 1 on success, 0 on failure.
        pub fn circle_usb_init() -> i32;

        /// Must be called repeatedly from the main loop (task level).
        /// Drives Circle's plug-and-play device scan and binds keyboard/mouse.
        pub fn circle_usb_update();

        /// Returns the next ASCII character from the keyboard buffer, or 0 if none.
        pub fn circle_keyboard_getc() -> u8;

        /// Returns 1 if a USB keyboard is connected and bound.
        pub fn circle_keyboard_ready() -> i32;

        /// Fills `out` with the latest mouse position and button state.
        pub fn circle_mouse_get_state(out: *mut MouseState);

        /// Returns 1 if a USB mouse is connected and bound.
        pub fn circle_mouse_ready() -> i32;
    }
}

// ─── Public API ───────────────────────────────────────────────────────────────

/// Initialise the USB subsystem.  Call once at kernel startup.
/// Returns (keyboard_found_eventually, mouse_found_eventually) — note that
/// on first call devices may not be enumerated yet; call `usb_update()` in
/// the main loop and check `keyboard_ready()` / `mouse_ready()` there.
pub fn usb_init() -> bool {
    #[cfg(feature = "bsp_rpi4")]
    {
        unsafe { ffi::circle_usb_init() == 1 }
    }
    #[cfg(not(feature = "bsp_rpi4"))]
    {
        false // QEMU / Pi 3 — no USB host
    }
}

/// Drive Circle's plug-and-play loop.  Call every iteration of the main loop.
pub fn usb_update() {
    #[cfg(feature = "bsp_rpi4")]
    unsafe { ffi::circle_usb_update(); }
}

/// Returns true if a USB keyboard is currently connected and enumerated.
pub fn keyboard_ready() -> bool {
    #[cfg(feature = "bsp_rpi4")]
    { unsafe { ffi::circle_keyboard_ready() == 1 } }
    #[cfg(not(feature = "bsp_rpi4"))]
    { false }
}

/// Returns the next ASCII character typed on the USB keyboard, or 0 if none.
pub fn keyboard_getc() -> u8 {
    #[cfg(feature = "bsp_rpi4")]
    { unsafe { ffi::circle_keyboard_getc() } }
    #[cfg(not(feature = "bsp_rpi4"))]
    { 0 }
}

/// Returns true if a USB mouse is currently connected and enumerated.
pub fn mouse_ready() -> bool {
    #[cfg(feature = "bsp_rpi4")]
    { unsafe { ffi::circle_mouse_ready() == 1 } }
    #[cfg(not(feature = "bsp_rpi4"))]
    { false }
}

/// Returns (buttons, x, y) — current mouse state.
pub fn mouse_get_state() -> (u32, i32, i32) {
    #[cfg(feature = "bsp_rpi4")]
    {
        let mut s = MouseState { buttons: 0, x: 0, y: 0, dx: 0, dy: 0 };
        unsafe { ffi::circle_mouse_get_state(&mut s as *mut MouseState); }
        (s.buttons, s.x, s.y)
    }
    #[cfg(not(feature = "bsp_rpi4"))]
    { (0, 0, 0) }
}

/// Returns (dx, dy) — last mouse displacement delta.
pub fn mouse_get_delta() -> (i32, i32) {
    #[cfg(feature = "bsp_rpi4")]
    {
        let mut s = MouseState { buttons: 0, x: 0, y: 0, dx: 0, dy: 0 };
        unsafe { ffi::circle_mouse_get_state(&mut s as *mut MouseState); }
        (s.dx, s.dy)
    }
    #[cfg(not(feature = "bsp_rpi4"))]
    { (0, 0) }
}
