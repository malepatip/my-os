/* Linker script for AArch64 bare-metal kernel on Raspberry Pi 4 */
/* ai-os v0.5.1 — kernel at 0x80000 (default, no kernel_old=1) */

ENTRY(_start)

SECTIONS
{
    . = 0x80000;

    /* Code — _start must be first */
    .text : {
        KEEP(*(.text._start))
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
    }

    /* Per-core stacks (512 bytes each, core 1 first) */
    .cpu_stacks : ALIGN(16) {
        __stack_start = .;
        . = . + 512;   /* core 1 stack */
        . = . + 512;   /* core 2 stack */
        . = . + 512;   /* core 3 stack */
    }

    _end = .;

    /DISCARD/ : {
        *(.comment)
        *(.note*)
        *(.eh_frame*)
        *(.init_array*)
        *(.ARM.exidx*)
    }
}
