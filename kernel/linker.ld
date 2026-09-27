/* CarvOS "chisel" kernel linker script.
 * Links the kernel into the top 2 GiB of the address space (higher half),
 * matching -C code-model=kernel. Segments are split so each gets exactly
 * one of R, RX, or RW permissions (W^X). */
OUTPUT_FORMAT(elf64-x86-64)
ENTRY(kmain)

PHDRS
{
    limine_requests PT_LOAD FLAGS((1 << 1) | (1 << 2)); /* RW */
    text            PT_LOAD FLAGS((1 << 0) | (1 << 2)); /* RX */
    rodata          PT_LOAD FLAGS((1 << 2));            /* R  */
    data            PT_LOAD FLAGS((1 << 1) | (1 << 2)); /* RW */
}

SECTIONS
{
    . = 0xffffffff80000000;

    /* Limine bootloader requests (added in P0.2) must stay in their own section. */
    .limine_requests : {
        KEEP(*(.limine_requests_start))
        KEEP(*(.limine_requests))
        KEEP(*(.limine_requests_end))
    } :limine_requests

    . = ALIGN(CONSTANT(MAXPAGESIZE));
    .text : {
        *(.text .text.*)
    } :text

    . = ALIGN(CONSTANT(MAXPAGESIZE));
    .rodata : {
        *(.rodata .rodata.*)
    } :rodata

    . = ALIGN(CONSTANT(MAXPAGESIZE));
    .data : {
        *(.data .data.*)
    } :data

    .bss : {
        *(.bss .bss.*)
        *(COMMON)
    } :data

    /DISCARD/ : {
        *(.eh_frame*)
        *(.note .note.*)
    }
}
