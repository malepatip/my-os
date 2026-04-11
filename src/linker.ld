/* Linker script for AArch64 bare-metal kernel on Raspberry Pi 3/4 */
/* The RPi firmware loads the kernel at 0x80000 */

ENTRY(_start)

SECTIONS
{
    . = 0x80000;

    /* Code — Rust _start MUST be first at 0x80000 */
    .text : {
        KEEP(*(.text._start))   /* Rust entry point must be first */
        *(.text .text.*)
        *(.init)                /* absorb Circle's .init here, after our _start */
        _etext = .;             /* Circle: end of text */
    }

    /* Read-only data */
    .rodata : ALIGN(4096) {
        *(.rodata .rodata.*)
    }

    /* C++ global constructors — Circle calls these via __init_start/__init_end */
    .init_array : ALIGN(8) {
        __init_start = .;
        KEEP(*(.init_array .init_array.*))
        __init_end = .;
    }

    /* ARM exception index — Circle references these even in AArch64 mode */
    .ARM.exidx : {
        __exidx_start = .;
        *(.ARM.exidx*)
        __exidx_end = .;
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
        _end = .;               /* Circle: end of BSS / start of heap */
    }

    /DISCARD/ : {
        *(.comment)
        *(.note*)
        *(.eh_frame*)
    }
}
