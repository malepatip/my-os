// circle_usb_shim.cpp
//
// Thin C++ shim wrapping Circle's USB HID stack for Rust FFI.
//
// Circle requires CMemorySystem to be initialized before ANY heap allocation
// (new/malloc). Without it, CMemorySystem::s_pThis is NULL and HeapAllocate()
// dereferences a null pointer, returning garbage addresses.
//
// Initialization order inside circle_usb_init():
//   1. CMemorySystem  — sets up the heap (9 MB to end of RAM on Pi 4)
//   2. CInterruptSystem — sets up GIC interrupt controller
//   3. CTimer         — sets up system timer
//   4. CDeviceNameService — device registry
//   5. CUSBHCIDevice  — xHCI host controller (uses heap via new)
//
// All objects are function-local statics so constructors run only when
// circle_usb_init() is called (after UART/framebuffer are already up).

#include <circle/memory.h>
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

// ─── Device pointers (set during update loop) ─────────────────────────────────
static CUSBKeyboardDevice *s_pKeyboard = nullptr;
static CMouseDevice       *s_pMouse    = nullptr;
static CUSBHCIDevice      *s_pUSBHCI   = nullptr;
static CDeviceNameService *s_pDevNS    = nullptr;

// ─── Circle callbacks ─────────────────────────────────────────────────────────
static void KeyPressedHandler(const char *pString) {
    while (*pString) key_push(*pString++);
}
static void MouseStatusHandler(unsigned nButtons, int nDX, int nDY, int) {
    s_MouseButtons = nButtons;
    s_MouseDX = nDX; s_MouseDY = nDY;
    s_MouseX += nDX; s_MouseY += nDY;
    if (s_MouseX < 0)    s_MouseX = 0;
    if (s_MouseX > 1919) s_MouseX = 1919;
    if (s_MouseY < 0)    s_MouseY = 0;
    if (s_MouseY > 1079) s_MouseY = 1079;
}

// ─── C-linkage API ────────────────────────────────────────────────────────────
extern "C" {

int circle_usb_init(void) {
    uart_puts("\n[USB] init start\n");

    // 1. Memory system — MUST be first; all Circle heap allocs depend on it.
    //    FALSE = no MMU (we run without virtual memory).
    uart_puts("[USB] 1/5: CMemorySystem\n");
    static CMemorySystem s_Memory(FALSE);
    uart_puts("[USB] 2/5: CInterruptSystem\n");
    static CInterruptSystem s_Interrupt;
    uart_puts("[USB] 3/5: CTimer\n");
    static CTimer s_Timer(&s_Interrupt);
    uart_puts("[USB] 4/5: CDeviceNameService\n");
    static CDeviceNameService s_DeviceNameService;
    uart_puts("[USB] 5/5: CUSBHCIDevice\n");
    static CUSBHCIDevice s_USBHCI(&s_Interrupt, &s_Timer, TRUE);

    // Save pointers for update loop
    s_pUSBHCI = &s_USBHCI;
    s_pDevNS  = &s_DeviceNameService;

    uart_puts("[USB] Interrupt.Initialize()\n");
    if (!s_Interrupt.Initialize()) {
        uart_puts("[USB] Interrupt.Initialize() FAILED\n");
        return 0;
    }
    uart_puts("[USB] Timer.Initialize()\n");
    if (!s_Timer.Initialize()) {
        uart_puts("[USB] Timer.Initialize() FAILED\n");
        return 0;
    }
    uart_puts("[USB] USBHCI.Initialize()\n");
    if (!s_USBHCI.Initialize()) {
        uart_puts("[USB] USBHCI.Initialize() FAILED\n");
        return 0;
    }
    uart_puts("[USB] all init OK\n");
    return 1;
}

void circle_usb_update(void) {
    if (!s_pUSBHCI || !s_pDevNS) return;

    boolean bUpdated = s_pUSBHCI->UpdatePlugAndPlay();
    if (!bUpdated) return;

    if (s_pKeyboard == nullptr) {
        s_pKeyboard = (CUSBKeyboardDevice *)
            s_pDevNS->GetDevice("ukbd1", FALSE);
        if (s_pKeyboard)
            s_pKeyboard->RegisterKeyPressedHandler(KeyPressedHandler);
    }
    if (s_pMouse == nullptr) {
        s_pMouse = (CMouseDevice *)
            s_pDevNS->GetDevice("mouse1", FALSE);
        if (s_pMouse)
            s_pMouse->RegisterStatusHandler(MouseStatusHandler);
    }
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

void assertion_failed(const char *pExpr, const char *pFile, unsigned) {
    uart_puts("[USB] ASSERT FAILED: ");
    uart_puts(pExpr ? pExpr : "?");
    uart_puts(" in ");
    uart_puts(pFile ? pFile : "?");
    uart_puts("\n");
}

int main(void) { return 0; }

} // extern "C"
