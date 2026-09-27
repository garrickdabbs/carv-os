//! Legacy 8259 PIC pair: remapped out of the exception range and fully masked, so only the local
//! APIC delivers interrupts (P1.5). The remap matters even when masked: a spurious IRQ7/IRQ15
//! (the ones a masked 8259 can still raise) arrives on [`PIC_VECTOR_BASE`]`..+16`, where the IDT
//! installs no-op handlers (`idt.rs`), instead of looking like a CPU exception or hitting an
//! empty gate. The range sits well away from the APIC timer (32) and spurious (0xFF) vectors.

use super::port::Port;

const MASTER_CMD: Port<u8> = Port::new(0x20);
const MASTER_DATA: Port<u8> = Port::new(0x21);
const SLAVE_CMD: Port<u8> = Port::new(0xA0);
const SLAVE_DATA: Port<u8> = Port::new(0xA1);

/// Vector base the (masked) PICs are remapped to: `0xE0..=0xEF`.
pub const PIC_VECTOR_BASE: u8 = 0xE0;

/// Initialises both PICs (ICW1–ICW4) to vectors `PIC_VECTOR_BASE..+16`, then masks every line.
pub fn remap_and_mask() {
    // SAFETY: the 8259 initialisation sequence on the standard ports; done once at boot on the
    // only CPU with interrupts disabled, and every line ends up masked.
    unsafe {
        MASTER_CMD.write(0x11); // ICW1: init, expect ICW4
        SLAVE_CMD.write(0x11);
        MASTER_DATA.write(PIC_VECTOR_BASE); // ICW2: vector offsets
        SLAVE_DATA.write(PIC_VECTOR_BASE + 8);
        MASTER_DATA.write(0x04); // ICW3: slave on IRQ2
        SLAVE_DATA.write(0x02); //       slave identity 2
        MASTER_DATA.write(0x01); // ICW4: 8086 mode
        SLAVE_DATA.write(0x01);
        MASTER_DATA.write(0xFF); // mask everything
        SLAVE_DATA.write(0xFF);
    }
}
