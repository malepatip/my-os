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

    // ── Park loop — Pi 4 spin table protocol ─────────────────────────────────
    // Cores 1-3 park here. They watch their spin table entry at:
    //   Core 1: 0xE0, Core 2: 0xE8, Core 3: 0xF0
    // When core 0 writes a non-zero address and sends SEV, the core
    // wakes, reads the address, and branches to it.
    // The spin table base is 0xD8. Core N entry = 0xD8 + N*8.
    ".L_park:",
    // x8 = core_id (already computed above as MPIDR.Aff0)
    "mov x9, #0xD8",           // spin table base
    "add x9, x9, x8, lsl #3", // x9 = 0xD8 + core_id * 8
    ".L_park_loop:",
    "wfe",
    "ldr x10, [x9]",           // load spin table entry
    "cbz x10, .L_park_loop",   // if zero, keep waiting
    // Non-zero: set up minimal EL2 environment for core 1
    // Set HCR_EL2: RW=1 (EL1 is AArch64)
    "mov x11, #(1 << 31)",
    "msr hcr_el2, x11",
    "isb",
    // Allow EL1 timer access
    "mrs x11, cnthctl_el2",
    "orr x11, x11, #0x3",
    "msr cnthctl_el2, x11",
    "msr cntvoff_el2, xzr",
    // Disable coprocessor traps
    "mov x11, #0x33ff",
    "msr cptr_el2, x11",
    "msr hstr_el2, xzr",
    // Set up a stack for core 1 (0x00280000, below core 0's stack at 0x002A0000)
    "movz x11, #0x0000",
    "movk x11, #0x28, lsl #16",
    "mov sp, x11",
    // Write alive marker to 0x00201000 so core 0 can detect core 1 woke up
    // This is BEFORE the trampoline, so if we see 0xC0RE0001 but Linux never
    // signals ready, we know the trampoline/ERET is the problem.
    "movz x11, #0x0001",
    "movk x11, #0xC001, lsl #16",  // x11 = 0xC0010001 (core 1 alive marker)
    "movz x12, #0x1000",
    "movk x12, #0x0020, lsl #16",  // x12 = 0x00201000
    "str x11, [x12]",
    "dsb sy",
    // Branch to the trampoline address
    "br x10",

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
    // Saves ESR_EL2 and ELR_EL2 to fixed addresses for post-mortem debugging,
    // then halts. Read 0x00201010 (ESR) and 0x00201018 (ELR) after crash.
    "_el2_panic:",
    "mrs x0, esr_el2",
    "mrs x1, elr_el2",
    "movz x2, #0x1010",
    "movk x2, #0x0020, lsl #16",   // x2 = 0x00201010
    "str x0, [x2]",                // save ESR_EL2
    "str x1, [x2, #8]",            // save ELR_EL2
    "dsb sy",
    "wfe",
    "b _el2_panic",

    // ── Lower EL sync handler (HVC from Linux, or stage-2 fault) ────────────
    //
    // Linux makes HVC calls for PSCI (CPU_ON, AFFINITY_INFO, VERSION) and
    // SMCCC (SMCCC_VERSION, ARCH_FEATURES). We must handle these correctly
    // or Linux will panic on boot.
    //
    // ESR_EL2.EC (bits [31:26]):
    //   0x16 = HVC instruction (from EL1 AArch64)
    //   0x17 = SMC instruction (from EL1 AArch64)
    //   0x24 = Data abort from lower EL (stage-2 fault)
    //   0x20 = Instruction abort from lower EL
    //
    // For HVC calls, x0 contains the function identifier:
    //   PSCI_VERSION      = 0x84000000
    //   PSCI_CPU_SUSPEND  = 0xC4000001
    //   PSCI_CPU_OFF      = 0x84000002
    //   PSCI_CPU_ON       = 0xC4000003
    //   PSCI_AFFINITY_INFO= 0xC4000004
    //   SMCCC_VERSION     = 0x80000000
    //   SMCCC_ARCH_FEAT   = 0x80000001
    //
    // Strategy: save all registers, check ESR_EL2.EC.
    //   - HVC with PSCI_VERSION → return 0x00020000 (PSCI v2.0)
    //   - HVC with SMCCC_VERSION → return 0x00010001 (SMCCC v1.1)
    //   - HVC with PSCI_CPU_ON → forward to EL3 via SMC (TF-A handles it)
    //     (if no TF-A, return NOT_SUPPORTED = -1)
    //   - HVC with PSCI_AFFINITY_INFO → return 0 (ON)
    //   - HVC with PSCI_CPU_SUSPEND → return 0 (success, no-op)
    //   - Stage-2 fault → save fault info and panic
    //   - All others → return NOT_SUPPORTED (-1) and ERET
    "_el2_lower_sync:",
    // Save x0-x4 and lr to EL2 stack
    "sub sp, sp, #48",
    "stp x0, x1, [sp, #0]",
    "stp x2, x3, [sp, #16]",
    "stp x4, x30, [sp, #32]",
    // Read ESR_EL2 to determine exception class
    "mrs x4, esr_el2",
    "lsr x4, x4, #26",             // EC = ESR_EL2[31:26]
    // Check for HVC (EC=0x16) or SMC (EC=0x17) from lower EL
    "cmp x4, #0x16",
    "b.eq .L_hvc_handler",
    "cmp x4, #0x17",
    "b.eq .L_hvc_handler",
    // Not HVC/SMC — save fault info and panic
    "mrs x0, esr_el2",
    "mrs x1, elr_el2",
    "mrs x2, far_el2",
    "movz x3, #0x1020",
    "movk x3, #0x0020, lsl #16",   // x3 = 0x00201020
    "str x0, [x3]",                // save ESR_EL2
    "str x1, [x3, #8]",            // save ELR_EL2
    "str x2, [x3, #16]",           // save FAR_EL2
    "dsb sy",
    "b _el2_panic",
    // ── HVC/SMC handler ──────────────────────────────────────────────────────
    ".L_hvc_handler:",
    // x0 (original) is the PSCI/SMCCC function ID — reload from stack
    "ldr x0, [sp, #0]",
    // Check PSCI_VERSION (0x84000000)
    "movz x4, #0x0000",
    "movk x4, #0x8400, lsl #16",
    "cmp x0, x4",
    "b.eq .L_psci_version",
    // Check SMCCC_VERSION (0x80000000)
    "movz x4, #0x0000",
    "movk x4, #0x8000, lsl #16",
    "cmp x0, x4",
    "b.eq .L_smccc_version",
    // Check PSCI_AFFINITY_INFO (0xC4000004)
    "movz x4, #0x0004",
    "movk x4, #0xC400, lsl #16",
    "cmp x0, x4",
    "b.eq .L_psci_affinity_on",
    // Check PSCI_CPU_SUSPEND (0xC4000001)
    "movz x4, #0x0001",
    "movk x4, #0xC400, lsl #16",
    "cmp x0, x4",
    "b.eq .L_psci_success",
    // Check PSCI_CPU_ON (0xC4000003) — forward to EL3 via SMC
    "movz x4, #0x0003",
    "movk x4, #0xC400, lsl #16",
    "cmp x0, x4",
    "b.eq .L_psci_cpu_on",
    // Check PSCI_CPU_OFF (0x84000002)
    "movz x4, #0x0002",
    "movk x4, #0x8400, lsl #16",
    "cmp x0, x4",
    "b.eq .L_psci_cpu_off",
    // Unknown HVC — return NOT_SUPPORTED (-1)
    "mov x0, #-1",
    "b .L_hvc_return",
    // PSCI_VERSION → 0x00020000 (PSCI v2.0)
    ".L_psci_version:",
    "movz x0, #0x0000",
    "movk x0, #0x0002, lsl #16",
    "b .L_hvc_return",
    // SMCCC_VERSION → 0x00010001 (SMCCC v1.1)
    ".L_smccc_version:",
    "movz x0, #0x0001",
    "movk x0, #0x0001, lsl #16",
    "b .L_hvc_return",
    // PSCI_AFFINITY_INFO → 0 (CPU is ON)
    ".L_psci_affinity_on:",
    "mov x0, xzr",
    "b .L_hvc_return",
    // PSCI_CPU_SUSPEND → 0 (success, no-op)
    ".L_psci_success:",
    "mov x0, xzr",
    "b .L_hvc_return",
    // PSCI_CPU_ON → forward to TF-A at EL3 via SMC
    // Reload x1/x2/x3 (target_cpu, entry_point, context_id) from stack
    ".L_psci_cpu_on:",
    "ldp x1, x2, [sp, #8]",        // x1=target_cpu, x2=entry_point
    "ldr x3, [sp, #24]",           // x3=context_id
    "smc #0",                      // forward to TF-A
    "b .L_hvc_return",
    // PSCI_CPU_OFF → halt this core
    ".L_psci_cpu_off:",
    "wfe",
    "b .L_psci_cpu_off",
    // Return from HVC: advance ELR_EL2 past the HVC instruction (+4)
    ".L_hvc_return:",
    "mrs x4, elr_el2",
    "add x4, x4, #4",
    "msr elr_el2, x4",
    // Restore registers and ERET
    "ldp x2, x3, [sp, #16]",
    "ldp x4, x30, [sp, #32]",
    "ldr x1, [sp, #8]",
    "add sp, sp, #48",
    "eret",

    // ── Lower EL IRQ handler ─────────────────────────────────────────────────
    // IMO is NOT set in HCR_EL2, so Linux IRQs go directly to EL1 via GIC.
    // This handler should never be reached in normal operation.
    // If it is reached, just ERET (do not panic — could be a spurious IRQ).
    "_el2_lower_irq:",
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
