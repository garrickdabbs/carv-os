//! 8254 PIT channel 2 as a one-shot stopwatch, used to calibrate the local APIC timer (P1.5) and
//! by the timer test. Channel 2's gate and output are exposed on port 0x61, so no interrupt is
//! needed: program a count, open the gate, poll the output bit.

use super::port::Port;

/// PIT input clock in Hz.
pub const PIT_HZ: u64 = 1_193_182;

const CONTROL: Port<u8> = Port::new(0x43);
const CHANNEL2: Port<u8> = Port::new(0x42);
/// Keyboard-controller port B: bit 0 gates channel 2, bit 1 drives the speaker, bit 5 is OUT2.
const PORT_B: Port<u8> = Port::new(0x61);

/// Longest single wait the 16-bit counter can express (~54.9 ms); longer waits are chained.
const MAX_CHUNK_MS: u64 = 50;

/// Busy-waits `ms` milliseconds against the PIT. Interrupts may be on or off.
pub fn busy_wait_ms(ms: u64) {
    let mut left = ms;
    while left > 0 {
        let chunk = left.min(MAX_CHUNK_MS);
        one_shot(((PIT_HZ * chunk) / 1000) as u16);
        left -= chunk;
    }
}

/// Runs channel 2 in mode 0 (interrupt on terminal count) once with `count` and returns when
/// OUT2 goes high.
fn one_shot(count: u16) {
    // SAFETY: standard PIT/port-B programming; channel 2 drives only the speaker, which we keep
    // silent (bit 1 clear), so nothing else observes it.
    unsafe {
        let ctl = PORT_B.read();
        PORT_B.write((ctl & !0x02) | 0x01); // gate on, speaker off
        CONTROL.write(0xB0); // channel 2, lo/hi byte, mode 0
        CHANNEL2.write((count & 0xFF) as u8);
        CHANNEL2.write((count >> 8) as u8);
        // Mode 0 starts counting on the next gate rising edge: pulse the gate.
        let ctl = PORT_B.read();
        PORT_B.write(ctl & !0x01);
        PORT_B.write(ctl | 0x01);
        while PORT_B.read() & 0x20 == 0 {
            core::hint::spin_loop();
        }
    }
}
