// SPDX-License-Identifier: MIT
//
// boot.rs — AArch64 bare-metal boot code for Raspberry Pi 4
//
// Boot sequence:
//   _start (EL3 or EL2)
//     → EL3: set SCR_EL3, CNTFRQ_EL0=54MHz, ERET to EL2
//     → EL2: configure HCR_EL2, CNTHCTL_EL2, SCTLR_EL1,
//             set SP_EL1 (exception stack), set VBAR_EL1,
//             ERET to EL1t
//     → EL1t: set kernel stack (SP_EL0), call rust_init()
//
// WHY EL1?
//   Circle's interrupt/timer/USB subsystem is designed to run at EL1.
//   It sets VBAR_EL1 and uses EL1 system registers (ELR_EL1, SPSR_EL1,
//   SP_EL1, etc.). Running at EL2 without a proper VBAR_EL2 means any
//   IRQ (e.g. the physical timer IRQ that CTimer arms) jumps to address
//   0x0 and the CPU dies silently.
//
// WHY EL1t (not EL1h)?
//   Circle's startup64.S drops to EL1t (SPSR_EL2 = 0x3c4). In EL1t:
//     - Normal code uses SP_EL0 (kernel stack at 0x2A0000)
//     - Exception/IRQ handlers automatically switch to SP_EL1 (0x308000)
//   This gives separate stacks for kernel and IRQ handlers, which is
//   what Circle's exceptionstub64.S expects.
//
// .init_array (C++ static constructors):
//   Circle uses C++ classes (CInterruptSystem, CTimer, CXHCIDevice, etc.)
//   as file-scope statics. Their constructors are registered in .init_array.
//   Without calling these constructors, all Circle objects are zero-
//   initialized: vtable pointers are NULL, member variables are wrong,
//   and the first method call crashes or hangs.
//   rust_init() iterates over .init_array AFTER zeroing BSS and BEFORE
//   calling kernel_main().
//
// CNTFRQ_EL0 note:
//   CNTFRQ_EL0 is READ-ONLY at EL2 and below. It can only be written
//   from EL3. On Pi 4 with an updated bootloader, start4.elf's ARM stub
//   sets CNTFRQ_EL0 = 54 MHz before handing off at EL2.

use core::arch::global_asm;

global_asm!(
    ".section .text._start",
    "_start:",

    // ── Core parking ────────────────────────────────────────────────────────
    // All 4 Cortex-A72 cores enter here simultaneously. Only core 0
    // (MPIDR_EL1.Aff0 == 0) continues; the rest spin in WFE.
    "mrs x8, mpidr_el1",
    "and x8, x8, #0x3",
    "cbnz x8, .L_park",

    // ── Exception Level detection ────────────────────────────────────────────
    "mrs x9, CurrentEL",
    "lsr x9, x9, #2",
    "cmp x9, #3",
    "b.eq .L_from_el3",
    "cmp x9, #2",
    "b.eq .L_from_el2",
    "b .L_park",

    // ── EL3 → EL2 transition ────────────────────────────────────────────────
    ".L_from_el3:",
    // SCR_EL3: NS=1, HCE=1, RW=1 (EL2 is AArch64)
    "mov x8, xzr",
    "orr x8, x8, #(1 << 0)",     // NS
    "orr x8, x8, #(1 << 8)",     // HCE
    "orr x8, x8, #(1 << 10)",    // RW
    "msr scr_el3, x8",
    // SPSR_EL3: EL2h, DAIF all masked. 0x3C9 = 0b11_1100_1001
    "mov x8, #0x3c9",
    "msr spsr_el3, x8",
    // CNTFRQ_EL0 = 54 MHz (only writable at EL3). 54000000 = 0x033E_D280
    "movz x8, #0xd280",
    "movk x8, #0x033e, lsl #16",
    "msr cntfrq_el0, x8",
    "isb",
    "adr x8, .L_from_el2",
    "msr elr_el3, x8",
    "eret",

    // ── EL2 setup + drop to EL1 ─────────────────────────────────────────────
    // We arrive here either from EL3 (via ERET above) or directly from
    // the updated bootloader (which boots into EL2).
    ".L_from_el2:",

    // HCR_EL2.RW (bit 31) = 1 — EL1 execution state is AArch64.
    "mov x8, #(1 << 31)",
    "msr hcr_el2, x8",
    "isb",

    // CNTHCTL_EL2: allow EL1 to access physical timer counter and regs.
    // EL1PCTEN (bit 0) = 1, EL1PCEN (bit 1) = 1.
    "mrs x8, cnthctl_el2",
    "orr x8, x8, #0x3",
    "msr cnthctl_el2, x8",
    // Zero virtual timer offset so EL1 virtual time == physical time.
    "msr cntvoff_el2, xzr",

    // Disable coprocessor traps to EL2.
    "mov x8, #0x33ff",
    "msr cptr_el2, x8",
    "msr hstr_el2, xzr",

    // Enable FP/SIMD at EL1 (CPACR_EL1 bits [21:20] = 0b11).
    "mov x8, #(3 << 20)",
    "msr cpacr_el1, x8",

    // SCTLR_EL1: RES1 bits set, MMU off, caches off.
    // Matches Circle's startup64.S armv8_switch_to_el1_m macro.
    "movz x8, #0x0800",
    "movk x8, #0x30d0, lsl #16",
    "msr sctlr_el1, x8",

    // SP_EL1 = MEM_EXCEPTION_STACK = 0x308000
    // In EL1t mode, exception/IRQ handlers use SP_EL1 (separate from
    // the kernel stack SP_EL0). This is what Circle expects.
    "movz x8, #0x8000",
    "movk x8, #0x30, lsl #16",
    "msr sp_el1, x8",

    // VBAR_EL1 = Circle's VectorTable.
    // Without this, any IRQ jumps to address 0 and the CPU dies.
    // VectorTable is defined in Circle's exceptionstub64.S and exported
    // from libcircle_nostartup.a.
    "ldr x8, =VectorTable",
    "msr vbar_el1, x8",
    "isb",

    // SPSR_EL2: EL1t (0b00100), DAIF all masked. 0x3C4 = 0b11_1100_0100
    // EL1t means: normal code uses SP_EL0, exception handlers use SP_EL1.
    // This matches Circle's startup64.S exactly.
    "mov x8, #0x3c4",
    "msr spsr_el2, x8",

    // ELR_EL2: jump to EL1 entry after ERET.
    "adr x8, .L_el1_entry",
    "msr elr_el2, x8",
    "eret",

    // ── EL1 entry ────────────────────────────────────────────────────────────
    ".L_el1_entry:",
    // Kernel stack = MEM_KERNEL_STACK = 0x2A0000 (grows downward).
    // In EL1t mode, `mov sp, x8` sets SP_EL0 (the thread stack pointer).
    // Circle's startup64.S uses this same value.
    "movz x8, #0x0000",
    "movk x8, #0x2a, lsl #16",   // x8 = 0x002A0000
    "mov sp, x8",

    // Jump to Rust init.
    "b {rust_init}",

    // ── Park loop ────────────────────────────────────────────────────────────
    ".L_park:",
    "wfe",
    "b .L_park",

    rust_init = sym rust_init,
);

/// Zero the BSS section, call C++ static constructors, then call kernel_main.
///
/// # Safety
/// Called from assembly once the stack pointer is valid. Must not be
/// inlined or the compiler may emit a prologue before SP is ready.
#[no_mangle]
unsafe extern "C" fn rust_init() -> ! {
    // ── 1. Zero the BSS section (static mut variables, zero-init globals) ──
    extern "C" {
        static __bss_start: u8;
        static __bss_end: u8;
    }
    let bss_start = &__bss_start as *const u8 as *mut u8;
    let bss_end   = &__bss_end   as *const u8;
    let bss_len   = bss_end as usize - bss_start as usize;
    core::ptr::write_bytes(bss_start, 0, bss_len);

    // ── 2. Call C++ static constructors (.init_array) ──────────────────────
    // Circle's CInterruptSystem, CTimer, CDeviceNameService, and CXHCIDevice
    // are declared as file-scope statics in circle_usb_shim.cpp. Their
    // constructors are registered in the .init_array section by the C++
    // compiler. Without calling these, all Circle objects are zero-
    // initialized: vtable pointers are NULL, member variables are wrong,
    // and the first method call crashes or hangs.
    //
    // The linker script defines __init_start and __init_end around
    // .init_array. Each entry is a function pointer (8 bytes on AArch64).
    extern "C" {
        static __init_start: u8;
        static __init_end: u8;
    }
    let init_start = &__init_start as *const u8 as *const unsafe extern "C" fn();
    let init_end   = &__init_end   as *const u8 as *const unsafe extern "C" fn();
    let count = (init_end as usize - init_start as usize)
              / core::mem::size_of::<unsafe extern "C" fn()>();
    for i in 0..count {
        let ctor = *init_start.add(i);
        ctor();
    }

    // ── 3. Enter kernel ────────────────────────────────────────────────────
    crate::kernel_main();
}
