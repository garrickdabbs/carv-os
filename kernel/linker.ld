/* CarvOS "chisel" kernel linker script.
 * Links the kernel into the top 2 GiB of the address space (higher half),
 * matching -C code-model=kernel. Limine loads each PT_LOAD segment with
 * exactly the permissions given here, so R, RX and RW are kept separate (W^X). */
OUTPUT_FORMAT(elf64-x86-64)
OUTPUT_ARCH(i386:x86-64)
ENTRY(kmain)

PHDRS
{
    text   PT_LOAD FLAGS((1 << 0) | (1 << 2)); /* RX */
    rodata PT_LOAD FLAGS((1 << 2));            /* R  */
    data   PT_LOAD FLAGS((1 << 1) | (1 << 2)); /* RW */
}

SECTIONS
{
    . = 0xffffffff80000000;

    .text : {
        *(.text .text.*)
    } :text

    . = ALIGN(CONSTANT(MAXPAGESIZE));
    .rodata : {
        *(.rodata .rodata.*)
    } :rodata

    . = ALIGN(CONSTANT(MAXPAGESIZE));
    .data : {
        /* Limine boot-protocol requests. The bootloader only honours requests
         * between the start and end markers, and --gc-sections must not drop them. */
        KEEP(*(.requests_start))
        KEEP(*(.requests))
        KEEP(*(.requests_end))
        *(.data .data.*)
    } :data

    /* Zero-initialised memory last so it adds no file padding. */
    .bss : {
        *(.bss .bss.*)
        *(COMMON)
    } :data

    /DISCARD/ : {
        *(.eh_frame*)
        *(.note .note.*)
    }
}
