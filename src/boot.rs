// SPDX-License-Identifier: MIT
//
// boot.rs — AArch64 bare-metal boot code for Raspberry Pi 3/4
//
// This is the very first code that runs when the kernel is loaded.
// The RPi firmware loads the kernel at 0x80000 and starts all 4 cores.
//
// Boot sequence
// ─────────────
// 1. Park cores 1-3 immediately (only core 0 continues).
// 2. Detect the current Exception Level (EL2 or EL3).
//    Older Pi firmware drops directly to EL2; newer Pi 4 EEPROM
//    firmware may stay at EL3 until the kernel handles the transition.
// 3. If at EL3: set CNTFRQ_EL0 = 54 MHz (only writable from EL3),
//    configure SCR_EL3, and use ERET to drop to EL2.
//    Running at EL3 is incorrect for our kernel because:
//      a) EL3 "secure world" memory permissions differ from EL2.
//      b) UART and GPU mailbox MMIO at 0xFE000000 are in the
//         non-secure physical address space, which is inaccessible
//         when NS=0 (secure) in EL3. The kernel would fault on the
//         very first peripheral write.
// 4. Once at EL2: configure HCR_EL2, disable the D-cache via
//    SCTLR_EL2 (the GPU firmware enables it before handing off;
//    mailbox buffer writes must bypass cache so the GPU sees them),
//    set up the stack, and jump to Rust.
//
// CNTFRQ_EL0 note
// ────────────────
// The ARM generic timer frequency register (CNTFRQ_EL0) is READ-ONLY
// at EL2 and below. It can only be written from EL3. On Pi 4 with an
// updated bootloader, start4.elf's ARM stub sets CNTFRQ_EL0 = 54 MHz
// before handing off to the kernel at EL2. If we boot from EL3 (old
// firmware), we set it ourselves. If the bootloader forgot to set it,
// Circle's CTimer assert is handled by our assertion_failed() stub
// (which returns instead of hanging) and -DNDEBUG in the shim build.
//
// We use global_asm! because the Rust compiler generates a function
// prologue (stp x29, x30, [sp, #-N]!) before any Rust code runs,
// which would dereference SP before we have set it up.
//
// Hardware invariants relied upon
// ────────────────────────────────
// • Kernel is loaded at physical address 0x80000 (set by linker.ld
//   and confirmed by the Pi firmware for kernel8.img + arm_64bit=1).
// • SP grows downward from 0x80000 into the memory below the kernel.
//   The kernel image is ~22 KB; on a Pi with ≥256 MB RAM the region
//   0x0–0x7FFFF is freely available for the stack.
// • No MMU is active; all physical addresses are directly accessible.
// • Interrupts are masked (DAIF=1111) from reset and we leave them
//   masked until the kernel sets up exception vectors.

use core::arch::global_asm;

global_asm!(
    ".section .text._start",
    "_start:",

    // ── Core parking ────────────────────────────────────────────────────────
    // All 4 Cortex-A72 cores enter here simultaneously. Only core 0
    // (MPIDR_EL1.Aff0 == 0) continues; the rest spin in WFE.
    "mrs x8, mpidr_el1",
    "and x8, x8, #0x3",          // isolate core number (Aff0 field)
    "cbnz x8, .L_park",

    // ── Exception Level detection ────────────────────────────────────────────
    // CurrentEL[3:2] holds the current EL. Shift right by 2 to get 1/2/3.
    "mrs x9, CurrentEL",
    "lsr x9, x9, #2",
    "cmp x9, #3",
    "b.eq .L_from_el3",
    "cmp x9, #2",
    "b.eq .L_from_el2",
    // EL1 would be very unusual and not supported — park this core.
    "b .L_park",

    // ── EL3 → EL2 transition ────────────────────────────────────────────────
    // SCR_EL3 controls which features are available to EL2/1/0.
    // We set three bits then ERET into EL2:
    //   NS  (bit 0)  = 1 — EL2/1/0 are non-secure. This is critical:
    //                      peripheral MMIO at 0xFE000000 is in the
    //                      non-secure physical address space. Without NS=1
    //                      we would fault on the first UART register write.
    //   HCE (bit 8)  = 1 — enable HVC instruction (harmless, tidiness).
    //   RW  (bit 10) = 1 — EL2 execution state is AArch64 (not AArch32).
    ".L_from_el3:",
    "mov x8, xzr",
    "orr x8, x8, #(1 << 0)",     // NS
    "orr x8, x8, #(1 << 8)",     // HCE
    "orr x8, x8, #(1 << 10)",    // RW
    "msr scr_el3, x8",

    // SPSR_EL3: processor state to restore on ERET.
    // M[4:0] = 0b01001 = EL2h  (use SP_EL2, AArch64).
    // DAIF   = 0b1111  (all interrupts/errors remain masked).
    // 0x3C9  = 0b11_1100_1001
    "mov x8, #0x3c9",
    "msr spsr_el3, x8",

    // ── Set CNTFRQ_EL0 = 54 MHz (only possible at EL3) ─────────────────────
    // CNTFRQ_EL0 is READ-ONLY at EL2 — writing it at EL2 causes a
    // synchronous exception (instant silent death with no exception vectors).
    // We set it here at EL3 so Circle's CTimer gets the correct frequency.
    // 54000000 = 0x033E_D280
    "movz x8, #0xd280",
    "movk x8, #0x033e, lsl #16",
    "msr cntfrq_el0, x8",
    "isb",

    // ELR_EL3: the address to jump to after ERET (our EL2 setup code).
    "adr x8, .L_from_el2",
    "msr elr_el3, x8",
    "eret",

    // ── EL2 setup ───────────────────────────────────────────────────────────
    // Whether we arrived here from EL3 (via ERET) or directly from
    // the firmware, we are now at EL2 in AArch64.
    //
    // If we came from EL3, CNTFRQ_EL0 was set above.
    // If the bootloader dropped us directly at EL2, its ARM stub
    // (start4.elf) should have set CNTFRQ_EL0 = 54 MHz already.
    // If it didn't (very old firmware), Circle's assert is handled
    // by our assertion_failed() stub which returns instead of hanging.
    ".L_from_el2:",

    // HCR_EL2.RW (bit 31) = 1 — EL1 (and EL0) execution state is AArch64.
    // Required so any future EL1 code does not revert to AArch32.
    "mov x8, #(1 << 31)",
    "msr hcr_el2, x8",
    "isb",

    // SCTLR_EL2: disable MMU (M, bit 0) and D-cache (C, bit 2).
    // The Pi 4 GPU firmware enables the ARM D-cache before handing off.
    // Our mailbox buffer is allocated on the stack; with D-cache on,
    // ARM writes stay in the L1 cache and the GPU DMA engine reads
    // stale zeros from RAM, making every mailbox property call fail.
    // We will re-enable I/D-cache and the MMU properly once we have
    // set up page tables in a later milestone.
    "mrs x8, sctlr_el2",
    "bic x8, x8, #(1 << 2)",     // C: disable D-cache
    "bic x8, x8, #(1 << 0)",     // M: disable MMU (may already be off)
    "msr sctlr_el2, x8",
    "isb",

    // ── Stack setup ─────────────────────────────────────────────────────────
    // SP points to 0x80000 and grows downward. The kernel .text section
    // starts at 0x80000 and grows upward; there is no overlap.
    "mov x8, #0x80000",
    "mov sp, x8",

    // ── Jump to Rust ─────────────────────────────────────────────────────────
    "b {rust_init}",

    // ── Park loop ────────────────────────────────────────────────────────────
    ".L_park:",
    "wfe",
    "b .L_park",

    rust_init = sym rust_init,
);

/// Zero the BSS section and call kernel_main.
///
/// # Safety
/// Called from assembly once the stack pointer is valid. Must not be
/// inlined or the compiler may emit a prologue before SP is ready.
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
