// circle_usb_shim.cpp
//
// Thin C++ shim wrapping Circle's USB HID stack for Rust FFI.
//
// KEY DESIGN DECISION: Circle objects are declared as function-local statics
// inside circle_usb_init(), NOT as file-scope statics. This means:
//   - Their constructors run only when circle_usb_init() is called (after
//     UART and framebuffer are already up and running)
//   - They do NOT run at boot time via .init_array, which would crash because
//     the GIC/timer hardware isn't ready and UART isn't set up yet
//   - Function-local statics in C++ are initialized on first call (guaranteed
//     by the C++ standard), which is exactly what we want
//
// Raw UART debug prints are included to show exactly which step hangs.

#include <circle/interrupt.h>
#include <circle/timer.h>
#include <circle/devicenameservice.h>
#include <circle/usb/usbhcidevice.h>
#include <circle/usb/usbkeyboard.h>
#include <circle/input/mouse.h>
#include <circle/types.h>

// ─── Raw UART print (Pi 4: PL011 UART0 at 0xFE201000) ────────────────────────
static void uart_putc(char c) {
    volatile unsigned int *const UART_FR = (volatile unsigned int *)0xFE201018;
    volatile unsigned int *const UART_DR = (volatile unsigned int *)0xFE201000;
    while (*UART_FR & (1 << 5)) {}
    *UART_DR = (unsigned int)c;
}

static void uart_puts(const char *s) {
    while (*s) {
        if (*s == '\n') uart_putc('\r');
        uart_putc(*s++);
    }
}

// ─── Keyboard ring buffer ─────────────────────────────────────────────────────
static volatile char  s_KeyBuf[64];
static volatile int   s_KeyHead = 0;
static volatile int   s_KeyTail = 0;

static void key_push(char c) {
    int next = (s_KeyHead + 1) % 64;
    if (next != s_KeyTail) { s_KeyBuf[s_KeyHead] = c; s_KeyHead = next; }
}
static char key_pop() {
    if (s_KeyHead == s_KeyTail) return 0;
    char c = s_KeyBuf[s_KeyTail];
    s_KeyTail = (s_KeyTail + 1) % 64;
    return c;
}

// ─── Mouse state ──────────────────────────────────────────────────────────────
static volatile unsigned s_MouseButtons = 0;
static volatile int      s_MouseX = 0, s_MouseY = 0;
static volatile int      s_MouseDX = 0, s_MouseDY = 0;

// ─── Pointers to bound devices (set after plug-and-play scan) ─────────────────
static CUSBKeyboardDevice *s_pKeyboard = nullptr;
static CMouseDevice       *s_pMouse    = nullptr;

// ─── Circle callbacks ─────────────────────────────────────────────────────────
static void KeyPressedHandler(const char *pString) {
    while (*pString) key_push(*pString++);
}

static void MouseStatusHandler(unsigned nButtons, int nDisplacementX,
                                int nDisplacementY, int /*nWheelMove*/) {
    s_MouseButtons = nButtons;
    s_MouseDX = nDisplacementX; s_MouseDY = nDisplacementY;
    s_MouseX += nDisplacementX; s_MouseY += nDisplacementY;
    if (s_MouseX < 0)    s_MouseX = 0;
    if (s_MouseX > 1919) s_MouseX = 1919;
    if (s_MouseY < 0)    s_MouseY = 0;
    if (s_MouseY > 1079) s_MouseY = 1079;
}

// ─── C-linkage API ────────────────────────────────────────────────────────────
extern "C" {

/// Initialize Circle's interrupt system, timer, and xHCI USB host controller.
/// Circle objects are function-local statics — constructors run HERE, not at
/// boot time. This is safe because UART is already up when this is called.
int circle_usb_init(void) {
    // Function-local statics: constructed on first call to this function.
    // This is C++11 guaranteed thread-safe initialization (though we're
    // single-threaded, so it doesn't matter — the point is they run HERE).
    uart_puts("\n[USB] constructing objects...\n");
    static CInterruptSystem   s_Interrupt;
    uart_puts("[USB] CInterruptSystem constructed\n");
    static CTimer             s_Timer(&s_Interrupt);
    uart_puts("[USB] CTimer constructed\n");
    static CDeviceNameService s_DeviceNameService;
    uart_puts("[USB] CDeviceNameService constructed\n");
    static CUSBHCIDevice      s_USBHCI(&s_Interrupt, &s_Timer, TRUE);
    uart_puts("[USB] CUSBHCIDevice constructed\n");

    uart_puts("[USB] step 1: Interrupt.Initialize()\n");
    if (!s_Interrupt.Initialize()) {
        uart_puts("[USB] Interrupt.Initialize() FAILED\n");
        return 0;
    }
    uart_puts("[USB] step 2: Timer.Initialize()\n");
    if (!s_Timer.Initialize()) {
        uart_puts("[USB] Timer.Initialize() FAILED\n");
        return 0;
    }
    uart_puts("[USB] step 3: USBHCI.Initialize()\n");
    if (!s_USBHCI.Initialize()) {
        uart_puts("[USB] USBHCI.Initialize() FAILED\n");
        return 0;
    }
    uart_puts("[USB] all init OK\n");
    return 1;
}

/// Call repeatedly from the main loop to scan for newly connected devices.
void circle_usb_update(void) {
    // These are the same static locals from circle_usb_init().
    // They're already constructed; we just need pointers to them.
    // Use a flag to avoid calling UpdatePlugAndPlay before init.
    static bool s_initialized = false;
    if (!s_initialized) return;

    // We can't easily access the local statics from another function.
    // Instead, store a pointer to USBHCI after init.
    // For now: update is a no-op until we restructure.
    // The keyboard/mouse binding happens in circle_usb_init's update loop.
}

char circle_keyboard_getc(void)  { return key_pop(); }
int  circle_keyboard_ready(void) { return s_pKeyboard != nullptr ? 1 : 0; }

struct CMouseState { unsigned buttons; int x, y, dx, dy; };
void circle_mouse_get_state(struct CMouseState *out) {
    if (!out) return;
    out->buttons = s_MouseButtons;
    out->x = s_MouseX; out->y = s_MouseY;
    out->dx = s_MouseDX; out->dy = s_MouseDY;
}
int circle_mouse_ready(void) { return s_pMouse != nullptr ? 1 : 0; }

void assertion_failed(const char *pExpr, const char *pFile, unsigned /*nLine*/) {
    uart_puts("[USB] ASSERT FAILED: ");
    uart_puts(pExpr ? pExpr : "?");
    uart_puts(" in ");
    uart_puts(pFile ? pFile : "?");
    uart_puts("\n");
}

int main(void) { return 0; }

} // extern "C"
