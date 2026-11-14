/* Pure-SRAM Linker Script for ESP32-C3 Stage 2 Zero-Flash RAM Probe */
MEMORY
{
    IRAM (rx)  : ORIGIN = 0x40380400, LENGTH = 72K
    DRAM (rwx) : ORIGIN = 0x3FC92400, LENGTH = 224K
}

ENTRY(_start)

SECTIONS
{
    .text : ALIGN(4)
    {
        KEEP(*(.text.init))
        *(.text .text.*)
    } > IRAM

    .rodata : ALIGN(4)
    {
        *(.rodata .rodata.* .srodata .srodata.*)
    } > DRAM

    .data : ALIGN(4)
    {
        _sdata = .;
        *(.data .data.* .sdata .sdata.*)
        . = ALIGN(4);
        _edata = .;
    } > DRAM

    .bss (NOLOAD) : ALIGN(4)
    {
        _sbss = .;
        *(.bss .bss.* .sbss .sbss.* COMMON)
        . = ALIGN(4);
        _ebss = .;
    } > DRAM

    . = ALIGN(16);
    _stack_bottom = .;
    _stack_top = ORIGIN(DRAM) + LENGTH(DRAM) - 16;
    PROVIDE(__global_pointer$ = _sdata + 0x800);

    /DISCARD/ :
    {
        *(.eh_frame*)
    }
}
