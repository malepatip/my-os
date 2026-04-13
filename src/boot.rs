// SPDX-License-Identifier: MIT
//
// boot.rs — AArch64 bare-metal boot code for ai-os v0.6.0
//
// BOOT FLOW WITH TF-A (feature/tfa-psci):
//
//   GPU firmware loads bl31.bin (TF-A BL31) at EL3.
//   TF-A runs at EL3, initialises GIC, patches DTB to add PSCI node
//   (method = "smc"), then drops Core 0 into EL2 at 0x80000 (here).
//   Cores 1-3 are parked by TF-A in its own secondary spin loop at EL3.
//   They are NOT released to EL2 until Linux calls CPU_ON via PSCI SMC.
//   TF-A intercepts CPU_ON, wakes each secondary core, and drops it
//   directly into Linux at EL1 — so all 4 cores arrive at Linux in EL1.
//
// WHAT THIS FILE DOES:
//   Core 0 enters here at EL2 (TF-A guarantees this).
//   Core 0 sets up EL2 system registers and calls rust_init().
//   Cores 1-3 NEVER reach this file — TF-A handles them entirely.
//
// NOTE: The EL3 -> EL2 fallback path (.L_from_el3) is kept as a safety
//   net in case someone boots without bl31.bin (e.g. during development
//   with the old armstub). It will not be hit in normal TF-A operation.

use core::arch::global_asm;

global_asm!(
    ".section .text._start",
    ".global _start",
    "_start:",

    // ── Timer setup ──────────────────────────────────────────────────────────
    // LOCAL_CONTROL @ 0xFF800000: clear to use 19.2 MHz crystal clock
    "ldr x0, =0xFF800000",
    "str wzr, [x0]",
    // LOCAL_PRESCALER @ 0xFF800008: divide-by-1 (0x80000000)
    "mov w1, #0x80000000",
    "str w1, [x0, #8]",
    // CNTVOFF_EL2: zero the virtual timer offset
    "msr cntvoff_el2, xzr",

    // ── Core ID check ────────────────────────────────────────────────────────
    // With TF-A, only Core 0 reaches this point.
    // Cores 1-3 are parked by TF-A and never execute this code.
    // We still check the core ID defensively — if somehow a secondary core
    // arrives here (e.g. without TF-A), it parks in a safe WFE loop.
    "mrs x1, mpidr_el1",
    "and x1, x1, #3",
    "cbz x1, .L_core0",

    // ── Safety park for unexpected secondary cores ────────────────────────────
    // This should never execute with TF-A. If it does, something is wrong.
    ".L_secondary_park:",
    "wfe",
    "b .L_secondary_park",

    // ── Core 0 entry ─────────────────────────────────────────────────────────
    ".L_core0:",
    // Detect current exception level
    "mrs x9, CurrentEL",
    "lsr x9, x9, #2",
    "cmp x9, #3",
    "b.eq .L_from_el3",
    "cmp x9, #2",
    "b.eq .L_from_el2",
    // EL1 — should never happen; park safely
    "b .L_secondary_park",

    // ── EL3 → EL2 fallback (only without TF-A) ───────────────────────────────
    // With TF-A this path is never taken. Kept for development fallback only.
    ".L_from_el3:",
    "mov x8, xzr",
    "orr x8, x8, #(1 << 0)",     // SCR_EL3.NS  — non-secure world
    "orr x8, x8, #(1 << 8)",     // SCR_EL3.HCE — HVC instructions enabled
    "orr x8, x8, #(1 << 10)",    // SCR_EL3.RW  — EL2 is AArch64
    "msr scr_el3, x8",
    "mov x8, #0x3c9",             // SPSR_EL3: EL2h, DAIF masked
    "msr spsr_el3, x8",
    "adr x8, .L_from_el2",
    "msr elr_el3, x8",
    "eret",

    // ── EL2 setup (Core 0, normal TF-A entry point) ──────────────────────────
    ".L_from_el2:",
    // HCR_EL2: RW=1 (EL1 is AArch64). VM, trap bits set later in linux_vm.rs.
    "mov x8, #(1 << 31)",
    "msr hcr_el2, x8",
    "isb",

    // Counter access: allow EL1 to access physical counter and timer
    "mrs x8, cnthctl_el2",
    "orr x8, x8, #0x3",           // EL1PCTEN | EL1PCEN
    "msr cnthctl_el2, x8",
    "msr cntvoff_el2, xzr",

    // Coprocessor traps: disable all to avoid spurious traps from Linux
    "mov x8, #0x33ff",
    "msr cptr_el2, x8",
    "msr hstr_el2, xzr",

    // Core 0 stack at 0x00400000 (4MB mark, well above kernel)
    "mov sp, #0x400000",

    // Install EL2 exception vector table
    "ldr x8, =_el2_vectors",
    "msr vbar_el2, x8",
    "isb",

    // Jump to Rust init (zeroes BSS, calls kernel_main)
    "b {rust_init}",

    rust_init = sym rust_init,
);

// ── EL2 exception vector table ───────────────────────────────────────────────
//
// With TF-A handling PSCI, the only EL2 exceptions we expect during normal
// operation are:
//   - Lower EL sync: stage-2 faults from Linux (handled by linux_vm.rs later)
//   - Lower EL IRQ:  interrupts from Linux guests (routed via GIC)
//
// For now all entries panic — we will add real handlers incrementally.
global_asm!(
    ".balign 2048",
    "_el2_vectors:",
    // Current EL with SP0
    ".balign 128", "b _el2_panic",   // Sync
    ".balign 128", "b _el2_panic",   // IRQ
    ".balign 128", "b _el2_panic",   // FIQ
    ".balign 128", "b _el2_panic",   // SError
    // Current EL with SPx
    ".balign 128", "b _el2_panic",   // Sync
    ".balign 128", "b _el2_panic",   // IRQ
    ".balign 128", "b _el2_panic",   // FIQ
    ".balign 128", "b _el2_panic",   // SError
    // Lower EL AArch64
    ".balign 128", "b _el2_lower_sync",  // Sync  (stage-2 faults, HVC/SMC from EL1)
    ".balign 128", "b _el2_lower_irq",   // IRQ
    ".balign 128", "b _el2_panic",       // FIQ
    ".balign 128", "b _el2_panic",       // SError
    // Lower EL AArch32
    ".balign 128", "b _el2_panic",
    ".balign 128", "b _el2_panic",
    ".balign 128", "b _el2_panic",
    ".balign 128", "b _el2_panic",

    "_el2_panic:",
    "wfe",
    "b _el2_panic",

    // Lower EL sync: for now just ERET back — Linux will handle its own faults.
    // A real implementation would inspect ESR_EL2 and handle stage-2 faults.
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
