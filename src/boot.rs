// SPDX-License-Identifier: MIT
//
// boot.rs — AArch64 bare-metal boot code for ai-os v0.5.0
//
// LAYOUT (kernel_old=1, loaded at 0x0):
//
//   0x0000  _start: branch to .L_startup at 0x100
//   0x0004  (padding — 0xD4 bytes of zeros)
//   0x00D8  spin_cpu0: .quad 0  (core 0 entry — unused, stays 0)
//   0x00E0  spin_cpu1: .quad 0  (core 1 entry — written by launch_linux_on_core1)
//   0x00E8  spin_cpu2: .quad 0  (core 2 entry — unused)
//   0x00F0  spin_cpu3: .quad 0  (core 3 entry — unused)
//   0x00F8  (8 bytes padding)
//   0x0100  .L_startup: real startup code begins here
//
// Boot sequence:
//   All 4 cores enter _start at 0x0 simultaneously.
//   Each reads MPIDR to get core_id.
//   Cores 1-3 jump to .L_park which spins on their spin_cpu slot.
//   Core 0 proceeds through EL3→EL2 setup and calls rust_init().
//
//   When kernel_main() wants to launch Linux on core 1:
//     1. Write trampoline address to physical 0xE0 (spin_cpu1)
//     2. dc civac flush + dsb sy + sev
//     3. Core 1 wakes from .L_park, reads 0xE0, branches to trampoline
//     4. Trampoline sets HCR/SPSR/ELR and ERETSs to Linux at EL1
//
// config.txt must have:
//   kernel_old=1
//   disable_commandline_tags=1
//   arm_64bit=1

use core::arch::global_asm;

global_asm!(
    ".section .text._start",

    // ── Entry point at 0x0 ──────────────────────────────────────────────────
    // First instruction: branch over the spin table to real startup code.
    // This MUST be the very first instruction at 0x0.
    "_start:",
    "b .L_startup",          // branch to 0x100 (past spin table)

    // ── Spin table (0xD8 - 0xFF) ────────────────────────────────────────────
    // These 8-byte slots are written by the kernel to wake secondary cores.
    // They MUST be at these exact physical addresses.
    ".org 0xD8",
    ".globl spin_cpu0",
    "spin_cpu0: .quad 0",    // 0xD8: core 0 (unused — core 0 never parks)
    ".globl spin_cpu1",
    "spin_cpu1: .quad 0",    // 0xE0: core 1 entry point (written to wake core 1)
    ".globl spin_cpu2",
    "spin_cpu2: .quad 0",    // 0xE8: core 2 (unused)
    ".globl spin_cpu3",
    "spin_cpu3: .quad 0",    // 0xF0: core 3 (unused)
    ".quad 0",               // 0xF8: padding

    // ── Real startup code at 0x100 ──────────────────────────────────────────
    ".org 0x100",
    ".L_startup:",

    // Read core ID from MPIDR_EL1 bits [1:0]
    "mrs x8, mpidr_el1",
    "and x8, x8, #0x3",
    // Cores 1-3 go to park loop; core 0 continues
    "cbnz x8, .L_park",

    // ── Exception Level detection (core 0 only) ─────────────────────────────
    "mrs x9, CurrentEL",
    "lsr x9, x9, #2",
    "cmp x9, #3",
    "b.eq .L_from_el3",
    "cmp x9, #2",
    "b.eq .L_from_el2",
    "b .L_park",             // EL1 or lower — shouldn't happen, park

    // ── EL3 → EL2 transition ────────────────────────────────────────────────
    ".L_from_el3:",
    // SCR_EL3: NS=1, HCE=1, RW=1 (EL2 is AArch64)
    "mov x8, xzr",
    "orr x8, x8, #(1 << 0)",   // NS
    "orr x8, x8, #(1 << 8)",   // HCE
    "orr x8, x8, #(1 << 10)",  // RW
    "msr scr_el3, x8",
    // SPSR_EL3: EL2h (0b1001), DAIF all masked = 0x3C9
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

    // ── EL2 setup (core 0) ──────────────────────────────────────────────────
    ".L_from_el2:",
    // HCR_EL2: RW=1 (EL1 is AArch64)
    "mov x8, #(1 << 31)",
    "msr hcr_el2, x8",
    "isb",
    // CNTHCTL_EL2: allow EL1 to access physical timer
    "mrs x8, cnthctl_el2",
    "orr x8, x8, #0x3",
    "msr cnthctl_el2, x8",
    "msr cntvoff_el2, xzr",
    // Disable coprocessor traps
    "mov x8, #0x33ff",
    "msr cptr_el2, x8",
    "msr hstr_el2, xzr",
    // Core 0 stack at 0x002A0000 (grows downward, well above our kernel)
    "movz x8, #0x0000",
    "movk x8, #0x2a, lsl #16",
    "mov sp, x8",
    // Set VBAR_EL2
    "ldr x8, =_el2_vectors",
    "msr vbar_el2, x8",
    "isb",
    // Jump to Rust
    "b {rust_init}",

    // ── Park loop (cores 1-3) ────────────────────────────────────────────────
    // Each core spins on its spin_cpu slot (at 0xD8 + core_id*8).
    // When core 0 writes a non-zero address to the slot and sends SEV,
    // the core wakes, sets up minimal EL2 state, and branches there.
    ".L_park:",
    // x8 = core_id
    // x9 = address of this core's spin slot = 0xD8 + core_id*8
    "mov x9, #0xD8",
    "add x9, x9, x8, lsl #3",
    ".L_park_loop:",
    "wfe",
    "ldr x10, [x9]",          // load spin slot
    "cbz x10, .L_park_loop",  // if zero, keep waiting
    // Core woke — set up minimal EL2 for the trampoline
    "mov x11, #(1 << 31)",
    "msr hcr_el2, x11",
    "isb",
    "mrs x11, cnthctl_el2",
    "orr x11, x11, #0x3",
    "msr cnthctl_el2, x11",
    "msr cntvoff_el2, xzr",
    "mov x11, #0x33ff",
    "msr cptr_el2, x11",
    "msr hstr_el2, xzr",
    // Core 1 stack at 0x00280000
    "movz x11, #0x0000",
    "movk x11, #0x28, lsl #16",
    "mov sp, x11",
    // Write alive marker to 0x00201000
    "movz x11, #0x0001",
    "movk x11, #0xC001, lsl #16",
    "movz x12, #0x1000",
    "movk x12, #0x0020, lsl #16",
    "str x11, [x12]",
    "dsb sy",
    // Branch to trampoline
    "br x10",

    rust_init = sym rust_init,
);

// ── Minimal EL2 exception vector table ──────────────────────────────────────
global_asm!(
    ".balign 2048",
    "_el2_vectors:",
    ".balign 128", "b _el2_panic",
    ".balign 128", "b _el2_panic",
    ".balign 128", "b _el2_panic",
    ".balign 128", "b _el2_panic",
    ".balign 128", "b _el2_panic",
    ".balign 128", "b _el2_panic",
    ".balign 128", "b _el2_panic",
    ".balign 128", "b _el2_panic",
    ".balign 128", "b _el2_lower_sync",
    ".balign 128", "b _el2_lower_irq",
    ".balign 128", "b _el2_panic",
    ".balign 128", "b _el2_panic",
    ".balign 128", "b _el2_panic",
    ".balign 128", "b _el2_panic",
    ".balign 128", "b _el2_panic",
    ".balign 128", "b _el2_panic",
    "_el2_panic:",
    "wfe",
    "b _el2_panic",
    "_el2_lower_sync:",
    "eret",
    "_el2_lower_irq:",
    "eret",
);

/// Zero BSS and call kernel_main.
#[no_mangle]
unsafe extern "C" fn rust_init() -> ! {
    extern "C" {
        static __bss_start: u8;
        static __bss_end: u8;
    }
    let bss_start = &__bss_start as *const u8 as *mut u8;
    let bss_end   = &__bss_end   as *const u8;
    let bss_len   = bss_end as usize - bss_start as usize;
    core::ptr::write_bytes(bss_start, 0, bss_len);
    crate::kernel_main();
}
