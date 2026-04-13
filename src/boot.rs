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

    // _el2_panic: current-EL exception (EL2 bug) — display fault screen.
    // No stack frame to unwind; SP_EL2 is valid (set at boot to 0x400000).
    "_el2_panic:",
    "mrs x0, esr_el2",
    "mrs x1, elr_el2",
    "mrs x2, far_el2",
    "mrs x3, hpfar_el2",
    "b hv_fault_screen",   // noreturn — halts inside Rust

    // _el2_lower_sync: handle HVC/SMC calls from Linux (PSCI, SMCCC)
    // and catch stage-2 faults. A bare eret causes Linux to panic because
    // PSCI_VERSION/SMCCC_VERSION return garbage in x0.
    "_el2_lower_sync:",
    "sub sp, sp, #48",
    "stp x0, x1, [sp, #0]",
    "stp x2, x3, [sp, #16]",
    "stp x4, x30, [sp, #32]",
    "mrs x4, esr_el2",
    "lsr x4, x4, #26",
    "cmp x4, #0x16",
    "b.eq .L_hvc_handler",
    "cmp x4, #0x17",
    "b.eq .L_hvc_handler",
    // Not HVC/SMC — check if it is a permission fault (EC=0x24, DFSC=0x0F)
    // caused by Linux writing to our read-only framebuffer region.
    // If so, skip the faulting instruction and return to Linux silently.
    // This keeps our crash-screen pixels intact even while Linux boots.
    "mrs x0, esr_el2",
    "lsr x1, x0, #26",          // x1 = EC
    "and x1, x1, #0x3F",
    "cmp x1, #0x24",             // EC == Data abort lower EL?
    "b.ne .L_real_fault",
    "and x1, x0, #0x3F",        // x1 = DFSC
    "cmp x1, #0x0F",             // DFSC == Permission fault L3?
    "b.ne .L_real_fault",
    "tst x0, #(1 << 6)",         // WnR bit — was it a write?
    "b.eq .L_real_fault",        // reads are unexpected; show crash
    // It is a write-permission fault → skip instruction, return to Linux
    "mrs x1, elr_el2",
    "add x1, x1, #4",
    "msr elr_el2, x1",
    "ldp x0, x1, [sp, #0]",
    "ldp x2, x3, [sp, #16]",
    "ldp x4, x30, [sp, #32]",
    "add sp, sp, #48",
    "eret",
    // Real fault — restore stack, display crash screen, halt
    ".L_real_fault:",
    "add sp, sp, #48",
    "mrs x0, esr_el2",
    "mrs x1, elr_el2",
    "mrs x2, far_el2",
    "mrs x3, hpfar_el2",
    "b hv_fault_screen",
    // HVC/SMC handler: dispatch on function ID in x0
    ".L_hvc_handler:",
    "ldr x0, [sp, #0]",
    // PSCI_VERSION (0x84000000) -> 0x00020000 (v2.0)
    "movz x4, #0x0000",
    "movk x4, #0x8400, lsl #16",
    "cmp x0, x4",
    "b.eq .L_psci_version",
    // SMCCC_VERSION (0x80000000) -> 0x00010001 (v1.1)
    "movz x4, #0x0000",
    "movk x4, #0x8000, lsl #16",
    "cmp x0, x4",
    "b.eq .L_smccc_version",
    // PSCI_AFFINITY_INFO (0xC4000004) -> 0 (ON)
    "movz x4, #0x0004",
    "movk x4, #0xC400, lsl #16",
    "cmp x0, x4",
    "b.eq .L_psci_affinity_on",
    // PSCI_CPU_SUSPEND (0xC4000001) -> 0 (success, no-op)
    "movz x4, #0x0001",
    "movk x4, #0xC400, lsl #16",
    "cmp x0, x4",
    "b.eq .L_psci_success",
    // PSCI_CPU_ON (0xC4000003) -> forward to TF-A via SMC
    "movz x4, #0x0003",
    "movk x4, #0xC400, lsl #16",
    "cmp x0, x4",
    "b.eq .L_psci_cpu_on",
    // PSCI_CPU_OFF (0x84000002) -> halt this core
    "movz x4, #0x0002",
    "movk x4, #0x8400, lsl #16",
    "cmp x0, x4",
    "b.eq .L_psci_cpu_off",
    // Unknown -> NOT_SUPPORTED (-1)
    "mov x0, #-1",
    "b .L_hvc_return",
    ".L_psci_version:",
    "movz x0, #0x0000",
    "movk x0, #0x0002, lsl #16",
    "b .L_hvc_return",
    ".L_smccc_version:",
    "movz x0, #0x0001",
    "movk x0, #0x0001, lsl #16",
    "b .L_hvc_return",
    ".L_psci_affinity_on:",
    "mov x0, xzr",
    "b .L_hvc_return",
    ".L_psci_success:",
    "mov x0, xzr",
    "b .L_hvc_return",
    ".L_psci_cpu_on:",
    "ldp x1, x2, [sp, #8]",
    "ldr x3, [sp, #24]",
    "smc #0",
    "b .L_hvc_return",
    ".L_psci_cpu_off:",
    "wfe",
    "b .L_psci_cpu_off",
    ".L_hvc_return:",
    "mrs x4, elr_el2",
    "add x4, x4, #4",
    "msr elr_el2, x4",
    "ldp x2, x3, [sp, #16]",
    "ldp x4, x30, [sp, #32]",
    "ldr x1, [sp, #8]",
    "add sp, sp, #48",
    "eret",

    // _el2_lower_irq: IMO is NOT set so Linux IRQs go to EL1 directly.
    // This handler should never fire in normal operation.
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
