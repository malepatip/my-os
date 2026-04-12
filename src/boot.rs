// SPDX-License-Identifier: MIT
//
// boot.rs — AArch64 bare-metal boot code for ai-os v0.4.0
//
// Boot sequence (core 0):
//   _start (EL3 or EL2)
//     → EL3: set SCR_EL3, CNTFRQ_EL0=54MHz, ERET to EL2
//     → EL2: configure CNTHCTL_EL2, CPTR_EL2, set up EL2 stack,
//             set VBAR_EL2, call rust_init() — STAY AT EL2
//
// Cores 1-3:
//   Park in WFE. Core 1 will be released by kernel_main() via
//   the Pi 4 spin table to run the Linux driver VM at EL1.
//
// WHY STAY AT EL2?
//   ai-os is a Type-1 hypervisor. It owns EL2 and controls the
//   Linux driver VM at EL1. All hardware access (UART, framebuffer,
//   GPIO, SD card) is done directly from EL2 — no Circle needed.
//
// VBAR_EL2:
//   We set VBAR_EL2 to a minimal exception handler table that
//   prints a panic message and halts. This catches any unexpected
//   exceptions at EL2 (e.g. stage-2 page table faults during setup).

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
    // If we're at EL1 somehow, park (shouldn't happen)
    "b .L_park",

    // ── EL3 → EL2 transition ────────────────────────────────────────────────
    ".L_from_el3:",
    // SCR_EL3: NS=1, HCE=1, RW=1 (EL2 is AArch64)
    "mov x8, xzr",
    "orr x8, x8, #(1 << 0)",     // NS
    "orr x8, x8, #(1 << 8)",     // HCE
    "orr x8, x8, #(1 << 10)",    // RW
    "msr scr_el3, x8",
    // SPSR_EL3: EL2h (0b1001), DAIF all masked. 0x3C9 = 0b11_1100_1001
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

    // ── EL2 setup — STAY AT EL2 ─────────────────────────────────────────────
    // We arrive here either from EL3 (via ERET above) or directly from
    // the Pi 4 bootloader (which boots at EL2 by default).
    ".L_from_el2:",

    // HCR_EL2: RW=1 (EL1 is AArch64). VM=0 for now (stage-2 off until
    // kernel_main() calls setup_stage2_tables()).
    // TGE=0 (EL1 is a guest, not host). We don't set VM=1 here because
    // we haven't set up page tables yet.
    "mov x8, #(1 << 31)",        // RW=1
    "msr hcr_el2, x8",
    "isb",

    // CNTHCTL_EL2: allow EL1 to access physical timer counter and regs.
    // EL1PCTEN (bit 0) = 1, EL1PCEN (bit 1) = 1.
    "mrs x8, cnthctl_el2",
    "orr x8, x8, #0x3",
    "msr cnthctl_el2, x8",
    // Zero virtual timer offset so EL1 virtual time == physical time.
    "msr cntvoff_el2, xzr",

    // NOTE: CNTFRQ_EL0 is READ-ONLY at EL2. Do NOT write it here.
    // The Pi 4 bootloader (start4.elf) sets it to 54 MHz before handing
    // off at EL2. If we came from EL3, we already set it above.

    // Disable coprocessor traps to EL2 (allow EL1 to use FP/SIMD/SVE).
    "mov x8, #0x33ff",
    "msr cptr_el2, x8",
    "msr hstr_el2, xzr",

    // Set up EL2 stack pointer (SP_EL2 = 0x2A0000, grows downward).
    // We use the same address as the old EL1 kernel stack — it's safe
    // because Linux will get its own stack from its own image.
    "movz x8, #0x0000",
    "movk x8, #0x2a, lsl #16",   // x8 = 0x002A0000
    "mov sp, x8",

    // Set VBAR_EL2 to our minimal EL2 exception vector table.
    "ldr x8, =_el2_vectors",
    "msr vbar_el2, x8",
    "isb",

    // Jump to Rust init (we are now at EL2 with a valid stack).
    "b {rust_init}",

    // ── Park loop ────────────────────────────────────────────────────────────
    ".L_park:",
    "wfe",
    "b .L_park",

    rust_init = sym rust_init,
);

// ── Minimal EL2 exception vector table ──────────────────────────────────────
//
// AArch64 exception vector table must be 2KB-aligned.
// Each entry is 128 bytes (32 instructions max).
// We only need to handle unexpected exceptions — just panic and halt.
global_asm!(
    ".balign 2048",
    "_el2_vectors:",

    // Current EL with SP0 — Synchronous
    ".balign 128",
    "b _el2_panic",
    // Current EL with SP0 — IRQ
    ".balign 128",
    "b _el2_panic",
    // Current EL with SP0 — FIQ
    ".balign 128",
    "b _el2_panic",
    // Current EL with SP0 — SError
    ".balign 128",
    "b _el2_panic",

    // Current EL with SPx — Synchronous
    ".balign 128",
    "b _el2_panic",
    // Current EL with SPx — IRQ
    ".balign 128",
    "b _el2_panic",
    // Current EL with SPx — FIQ
    ".balign 128",
    "b _el2_panic",
    // Current EL with SPx — SError
    ".balign 128",
    "b _el2_panic",

    // Lower EL AArch64 — Synchronous (from Linux EL1)
    ".balign 128",
    "b _el2_lower_sync",
    // Lower EL AArch64 — IRQ
    ".balign 128",
    "b _el2_lower_irq",
    // Lower EL AArch64 — FIQ
    ".balign 128",
    "b _el2_panic",
    // Lower EL AArch64 — SError
    ".balign 128",
    "b _el2_panic",

    // Lower EL AArch32 — (not used)
    ".balign 128",
    "b _el2_panic",
    ".balign 128",
    "b _el2_panic",
    ".balign 128",
    "b _el2_panic",
    ".balign 128",
    "b _el2_panic",

    // ── EL2 panic handler ────────────────────────────────────────────────────
    "_el2_panic:",
    "wfe",
    "b _el2_panic",

    // ── Lower EL sync handler (HVC from Linux, or stage-2 fault) ────────────
    "_el2_lower_sync:",
    // For now, just ERET back to Linux (ignore HVC calls)
    "eret",

    // ── Lower EL IRQ handler (Linux IRQ routed to EL2 via HCR_EL2.IMO) ──────
    "_el2_lower_irq:",
    // For now, just ERET back to Linux (pass IRQ handling back)
    "eret",
);

/// Zero the BSS section and call kernel_main.
///
/// # Safety
/// Called from assembly once the EL2 stack pointer is valid.
/// Must not be inlined or the compiler may emit a prologue before SP is ready.
#[no_mangle]
unsafe extern "C" fn rust_init() -> ! {
    // Zero the BSS section (static mut variables, zero-init globals).
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
