// SPDX-License-Identifier: MIT
//
// boot.rs — AArch64 bare-metal boot code for ai-os v0.5.1
//
// APPROACH: Mirrors the proven rpi4-osdev part10-multicore tutorial exactly.
//
// Kernel loads at 0x80000 (default, no kernel_old=1).
// All 4 cores start executing at 0x80000 simultaneously.
// Cores 1-3 park in a WFE loop reading from spin_cpu[N] (data labels in binary).
// Core 0 sets up EL2 and calls rust_init().
//
// To wake core 1:
//   extern "C" { static spin_cpu1: u64; }
//   (spin_cpu1 as *mut u64).write_volatile(entry_addr);
//   asm!("sev");
//
// The spin table labels (spin_cpu0..3) are placed by .ltorg after the code.
// Their addresses are determined by the linker — NOT hardcoded.

use core::arch::global_asm;

global_asm!(
    ".section .text._start",
    ".global _start",
    "_start:",

    // ── Timer setup (matches rpi4-osdev tutorial) ────────────────────────────
    // LOCAL_CONTROL = 0xFF800000: clear to use crystal clock
    "ldr x0, =0xFF800000",
    "str wzr, [x0]",
    // LOCAL_PRESCALER = 0xFF800008: set to 0x80000000 for 1:1 prescale
    "mov w1, #0x80000000",
    "str w1, [x0, #8]",
    // CNTFRQ_EL0 = 54 MHz (only writable at EL3 or if we're at EL2 with access)
    // On Pi 4 with start4.elf, we're at EL2 and CNTFRQ is already set.
    // Try to set it; if it faults, the exception handler will ignore it.
    "ldr x0, =54000000",
    "msr cntfrq_el0, x0",
    "msr cntvoff_el2, xzr",

    // ── Core ID check ────────────────────────────────────────────────────────
    // Read MPIDR_EL1 bits [1:0] to get core ID (0-3)
    "mrs x1, mpidr_el1",
    "and x1, x1, #3",
    // Core 0 continues; cores 1-3 go to park loop
    "cbz x1, .L_core0",

    // ── Park loop (cores 1-3) ────────────────────────────────────────────────
    // x1 = core_id (1, 2, or 3)
    // x5 = address of spin_cpu0 (PC-relative via adr)
    "adr x5, spin_cpu0",
    ".L_park_loop:",
    "wfe",
    "ldr x4, [x5, x1, lsl #3]",  // x4 = spin_cpu[core_id]
    "cbz x4, .L_park_loop",       // if zero, keep waiting
    // Non-zero: set up minimal EL2 state for this core
    // Set up stack: __stack_start + core_id * 512
    "ldr x2, =__stack_start",
    "lsl x3, x1, #9",             // core_id * 512
    "add x3, x2, x3",
    "mov sp, x3",
    // Write alive marker to 0x00201000 (core 1 only, for diagnostics)
    "cmp x1, #1",
    "b.ne .L_no_marker",
    "movz x6, #0x0001",
    "movk x6, #0xC001, lsl #16",  // 0xC0010001
    "movz x7, #0x1000",
    "movk x7, #0x0020, lsl #16",  // 0x00201000
    "str x6, [x7]",
    "dsb sy",
    ".L_no_marker:",
    // Clear x0-x3 per Linux boot ABI (x0=DTB will be set by trampoline)
    "mov x0, #0",
    "mov x2, #0",
    "mov x3, #0",
    // Branch to entry function
    "br x4",
    "b .L_park_loop",             // safety: loop if br returns

    // ── Core 0 setup ─────────────────────────────────────────────────────────
    ".L_core0:",
    // Detect exception level
    "mrs x9, CurrentEL",
    "lsr x9, x9, #2",
    "cmp x9, #3",
    "b.eq .L_from_el3",
    "cmp x9, #2",
    "b.eq .L_from_el2",
    "b .L_park_loop",             // EL1 — shouldn't happen

    // ── EL3 → EL2 ───────────────────────────────────────────────────────────
    ".L_from_el3:",
    "mov x8, xzr",
    "orr x8, x8, #(1 << 0)",     // SCR_EL3.NS
    "orr x8, x8, #(1 << 8)",     // SCR_EL3.HCE
    "orr x8, x8, #(1 << 10)",    // SCR_EL3.RW (EL2 is AArch64)
    "msr scr_el3, x8",
    "mov x8, #0x3c9",             // SPSR_EL3: EL2h, DAIF masked
    "msr spsr_el3, x8",
    "adr x8, .L_from_el2",
    "msr elr_el3, x8",
    "eret",

    // ── EL2 setup (core 0) ──────────────────────────────────────────────────
    ".L_from_el2:",
    "mov x8, #(1 << 31)",         // HCR_EL2.RW = 1 (EL1 is AArch64)
    "msr hcr_el2, x8",
    "isb",
    "mrs x8, cnthctl_el2",
    "orr x8, x8, #0x3",           // EL1PCTEN | EL1PCEN
    "msr cnthctl_el2, x8",
    "msr cntvoff_el2, xzr",
    "mov x8, #0x33ff",
    "msr cptr_el2, x8",           // disable coprocessor traps
    "msr hstr_el2, xzr",
    // Core 0 stack at 0x00400000 (4MB, well above kernel)
    "mov sp, #0x400000",
    // VBAR_EL2
    "ldr x8, =_el2_vectors",
    "msr vbar_el2, x8",
    "isb",
    "b {rust_init}",

    // ── Spin table data (placed by .ltorg) ───────────────────────────────────
    // These labels are the spin table. Their addresses are used by
    // launch_linux_on_core1() to wake core 1.
    ".ltorg",
    ".global spin_cpu0",
    "spin_cpu0: .quad 0",         // core 0 (unused)
    ".global spin_cpu1",
    "spin_cpu1: .quad 0",         // core 1 — written to wake Linux VM
    ".global spin_cpu2",
    "spin_cpu2: .quad 0",         // core 2 (unused)
    ".global spin_cpu3",
    "spin_cpu3: .quad 0",         // core 3 (unused)

    rust_init = sym rust_init,
);

// ── EL2 exception vector table ───────────────────────────────────────────────
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
