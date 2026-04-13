/* Linker script for AArch64 bare-metal kernel on Raspberry Pi 4 */
/* ai-os v0.5.0 — kernel_old=1, loads at 0x0 */
/* config.txt: kernel_old=1  disable_commandline_tags=1  arm_64bit=1 */

ENTRY(_start)

SECTIONS
{
    . = 0x0;

    /* Code — _start at 0x0, spin table at 0xD8, real code at 0x100 */
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
