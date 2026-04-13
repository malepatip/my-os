/* Linker script for AArch64 bare-metal kernel on Raspberry Pi 4 */
/* ai-os v0.5.0 — EL2 Thin Hypervisor */
/* The RPi firmware loads the kernel at 0x80000 */

ENTRY(_start)

SECTIONS
{
    . = 0x80000;

    /* Code — Rust _start MUST be first at 0x80000 */
    .text : {
        KEEP(*(.text._start))   /* Rust entry point must be first */
        *(.text .text.*)
        *(.init)
        _etext = .;
    }

    /* Read-only data */
    .rodata : ALIGN(4096) {
        *(.rodata .rodata.*)
    }

    /* Initialised data */
    .data : ALIGN(4096) {
        *(.data .data.*)
    }

    /* Zero-initialised data */
    .bss : ALIGN(4096) {
        __bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        __bss_end = .;
        _end = .;
    }

    /DISCARD/ : {
        *(.comment)
        *(.note*)
        *(.eh_frame*)
        *(.init_array*)
        *(.ARM.exidx*)
    }
}
