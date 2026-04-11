// circle_usb_shim.cpp — with raw UART debug prints to find exact hang point

#include <circle/interrupt.h>
#include <circle/timer.h>
#include <circle/devicenameservice.h>
#include <circle/usb/usbhcidevice.h>
#include <circle/usb/usbkeyboard.h>
#include <circle/input/mouse.h>
#include <circle/types.h>

// ─── Raw UART print (Pi 4: PL011 UART0 at 0xFE201000) ────────────────────────
// We write directly to hardware registers — no Circle, no Rust, no stack needed.
// This works at any exception level as long as UART was already initialized
// by our Rust kernel_main() before calling circle_usb_init().

static void uart_putc(char c) {
    volatile unsigned int *const UART_FR = (volatile unsigned int *)0xFE201018;
    volatile unsigned int *const UART_DR = (volatile unsigned int *)0xFE201000;
    while (*UART_FR & (1 << 5)) {} // wait until TX FIFO not full
    *UART_DR = (unsigned int)c;
}

static void uart_puts(const char *s) {
    while (*s) {
        if (*s == '\n') uart_putc('\r');
        uart_putc(*s++);
    }
}

// ─── Shared state ─────────────────────────────────────────────────────────────

static CInterruptSystem   s_Interrupt;
static CTimer             s_Timer (&s_Interrupt);
static CDeviceNameService s_DeviceNameService;
static CUSBHCIDevice      s_USBHCI (&s_Interrupt, &s_Timer, TRUE);

static CUSBKeyboardDevice *s_pKeyboard = nullptr;
static CMouseDevice       *s_pMouse    = nullptr;

// ─── Keyboard ring buffer ─────────────────────────────────────────────────────

static volatile char  s_KeyBuf[64];
static volatile int   s_KeyHead = 0;
static volatile int   s_KeyTail = 0;

static void key_push(char c) {
    int next = (s_KeyHead + 1) % 64;
    if (next != s_KeyTail) {
        s_KeyBuf[s_KeyHead] = c;
        s_KeyHead = next;
    }
}

static char key_pop() {
    if (s_KeyHead == s_KeyTail) return 0;
    char c = s_KeyBuf[s_KeyTail];
    s_KeyTail = (s_KeyTail + 1) % 64;
    return c;
}

// ─── Mouse state ──────────────────────────────────────────────────────────────

static volatile unsigned s_MouseButtons = 0;
static volatile int      s_MouseX       = 0;
static volatile int      s_MouseY       = 0;
static volatile int      s_MouseDX      = 0;
static volatile int      s_MouseDY      = 0;

// ─── Circle callbacks ─────────────────────────────────────────────────────────

static void KeyPressedHandler(const char *pString) {
    while (*pString) key_push(*pString++);
}

static void MouseStatusHandler(unsigned nButtons, int nDisplacementX,
                                int nDisplacementY, int nWheelMove) {
    s_MouseButtons = nButtons;
    s_MouseDX = nDisplacementX;
    s_MouseDY = nDisplacementY;
    s_MouseX += nDisplacementX;
    s_MouseY += nDisplacementY;
    if (s_MouseX < 0)    s_MouseX = 0;
    if (s_MouseX > 1919) s_MouseX = 1919;
    if (s_MouseY < 0)    s_MouseY = 0;
    if (s_MouseY > 1079) s_MouseY = 1079;
}

// ─── C-linkage API ────────────────────────────────────────────────────────────

extern "C" {

int circle_usb_init(void) {
    uart_puts("\n[USB] step 1: Interrupt.Initialize()\n");
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

void circle_usb_update(void) {
    boolean bUpdated = s_USBHCI.UpdatePlugAndPlay();
    if (bUpdated && s_pKeyboard == nullptr) {
        s_pKeyboard = (CUSBKeyboardDevice *)
            s_DeviceNameService.GetDevice("ukbd1", FALSE);
        if (s_pKeyboard != nullptr)
            s_pKeyboard->RegisterKeyPressedHandler(KeyPressedHandler);
    }
    if (bUpdated && s_pMouse == nullptr) {
        s_pMouse = (CMouseDevice *)
            s_DeviceNameService.GetDevice("mouse1", FALSE);
        if (s_pMouse != nullptr)
            s_pMouse->RegisterStatusHandler(MouseStatusHandler);
    }
}

char circle_keyboard_getc(void)  { return key_pop(); }
int  circle_keyboard_ready(void) { return s_pKeyboard != nullptr ? 1 : 0; }

struct CMouseState {
    unsigned buttons;
    int x, y, dx, dy;
};

void circle_mouse_get_state(struct CMouseState *out) {
    if (!out) return;
    out->buttons = s_MouseButtons;
    out->x = s_MouseX; out->y = s_MouseY;
    out->dx = s_MouseDX; out->dy = s_MouseDY;
}

int circle_mouse_ready(void) { return s_pMouse != nullptr ? 1 : 0; }

void assertion_failed(const char *pExpr, const char *pFile, unsigned nLine) {
    uart_puts("[USB] ASSERT FAILED: ");
    uart_puts(pExpr ? pExpr : "?");
    uart_puts(" in ");
    uart_puts(pFile ? pFile : "?");
    uart_puts("\n");
    // Return instead of hanging — let caller continue
}

int main(void) { return 0; }

} // extern "C"
