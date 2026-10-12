//! `print!`/`println!` and the provisional console write protocol.
//!
//! Until the `cons` service exists (P3.4) output goes to the kernel's `debug_putc` syscall. A
//! program that is handed a *console capability* — an endpoint capability whose receiver is a
//! console server — calls [`set_console_cap`] and from then on its output is sent there as
//! [`WRITE`] messages instead, so the program needs no ambient output authority.
//!
//! The message format is defined here so that the client (this crate) and the server (P3.4)
//! share one encoding: `label` = [`WRITE`], word 0 = number of bytes `n` (at most
//! [`MAX_CHUNK`]), words 1.. = the bytes, packed little-endian eight per word. It is not part of
//! the frozen kernel ABI; it is an ordinary IPC protocol that P3.2/P3.4 may refine.

use carv_abi::{MESSAGE_WORDS, Message, MessageInfo};

/// Message label of a console write.
pub const WRITE: u64 = 1;
/// Most bytes one [`WRITE`] message carries (five payload words).
pub const MAX_CHUNK: usize = 8 * (MESSAGE_WORDS - 1);

/// Packs `bytes` (at most [`MAX_CHUNK`]) into a [`WRITE`] message.
pub fn encode(bytes: &[u8]) -> Option<Message> {
    if bytes.len() > MAX_CHUNK {
        return None;
    }
    let mut message = Message {
        label: WRITE,
        ..Message::default()
    };
    message.words[0] = bytes.len() as u64;
    for (i, chunk) in bytes.chunks(8).enumerate() {
        let mut word = [0u8; 8];
        word[..chunk.len()].copy_from_slice(chunk);
        message.words[1 + i] = u64::from_le_bytes(word);
    }
    message.info = MessageInfo::new(1 + bytes.len().div_ceil(8), 0)?;
    Some(message)
}

/// Unpacks a [`WRITE`] message into `out`, returning the number of bytes, or `None` if the label
/// is wrong or the declared length does not fit the message's words.
pub fn decode(message: &Message, out: &mut [u8; MAX_CHUNK]) -> Option<usize> {
    if message.label != WRITE || message.info.words() == 0 {
        return None;
    }
    let len = usize::try_from(message.words[0]).ok()?;
    if len > MAX_CHUNK || message.info.words() < 1 + len.div_ceil(8) {
        return None;
    }
    for (i, word) in message.words[1..].iter().enumerate() {
        out[8 * i..8 * i + 8].copy_from_slice(&word.to_le_bytes());
    }
    Some(len)
}

#[cfg(target_os = "none")]
pub use target::{Console, console_cap, set_console_cap, use_debug_console};

#[cfg(target_os = "none")]
#[doc(hidden)]
pub use target::{_print, _println};

#[cfg(target_os = "none")]
mod target {
    use core::fmt;
    use core::sync::atomic::{AtomicU64, Ordering};

    use super::{MAX_CHUNK, encode};
    use crate::sys;

    /// Marks "no console capability": output goes to `debug_putc`.
    const DEBUG: u64 = u64::MAX;
    static CONSOLE_CAP: AtomicU64 = AtomicU64::new(DEBUG);

    /// Sends all further output to the console server behind endpoint capability `slot`.
    pub fn set_console_cap(slot: u64) {
        CONSOLE_CAP.store(slot, Ordering::Relaxed);
    }

    /// Sends all further output to the kernel's `debug_putc` again.
    pub fn use_debug_console() {
        CONSOLE_CAP.store(DEBUG, Ordering::Relaxed);
    }

    /// The console capability slot, or `None` while output goes to `debug_putc`.
    pub fn console_cap() -> Option<u64> {
        match CONSOLE_CAP.load(Ordering::Relaxed) {
            DEBUG => None,
            slot => Some(slot),
        }
    }

    /// A [`fmt::Write`] sink for the current console. Bytes are batched into full messages and
    /// the remainder is sent when the value is dropped. Send errors drop the output: a console
    /// that cannot be written to cannot report that either.
    pub struct Console {
        cap: Option<u64>,
        buf: [u8; MAX_CHUNK],
        len: usize,
    }

    impl Console {
        /// A sink writing to whichever console is current now.
        pub fn new() -> Self {
            Self {
                cap: console_cap(),
                buf: [0; MAX_CHUNK],
                len: 0,
            }
        }

        fn flush(&mut self) {
            if let (Some(cap), Some(message)) = (self.cap, encode(&self.buf[..self.len])) {
                let _ = sys::send(cap, &message);
            }
            self.len = 0;
        }
    }

    impl Default for Console {
        fn default() -> Self {
            Self::new()
        }
    }

    impl fmt::Write for Console {
        fn write_str(&mut self, s: &str) -> fmt::Result {
            if self.cap.is_none() {
                s.bytes().for_each(sys::debug_putc);
                return Ok(());
            }
            for &byte in s.as_bytes() {
                if self.len == MAX_CHUNK {
                    self.flush();
                }
                self.buf[self.len] = byte;
                self.len += 1;
            }
            Ok(())
        }
    }

    impl Drop for Console {
        fn drop(&mut self) {
            if self.len > 0 {
                self.flush();
            }
        }
    }

    #[doc(hidden)]
    pub fn _print(args: fmt::Arguments<'_>) {
        let _ = fmt::Write::write_fmt(&mut Console::new(), args);
    }

    #[doc(hidden)]
    pub fn _println(args: fmt::Arguments<'_>) {
        let mut console = Console::new();
        let _ = fmt::Write::write_fmt(&mut console, args);
        let _ = fmt::Write::write_str(&mut console, "\n");
    }
}

/// Prints to the console (see [`console`](crate::console)).
#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {
        $crate::console::_print(::core::format_args!($($arg)*))
    };
}

/// Prints to the console, with a newline.
#[macro_export]
macro_rules! println {
    () => {
        $crate::console::_println(::core::format_args!(""))
    };
    ($($arg:tt)*) => {
        $crate::console::_println(::core::format_args!($($arg)*))
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(bytes: &[u8]) {
        let message = encode(bytes).expect("fits one message");
        assert_eq!(message.label, WRITE);
        assert_eq!(message.info.words(), 1 + bytes.len().div_ceil(8));
        assert_eq!(message.info.caps(), 0);
        let mut out = [0u8; MAX_CHUNK];
        let n = decode(&message, &mut out).expect("decodes");
        assert_eq!(&out[..n], bytes);
    }

    #[test]
    fn messages_round_trip_at_every_length() {
        for len in 0..=MAX_CHUNK {
            let bytes: std::vec::Vec<u8> =
                (0..len as u8).map(|b| b.wrapping_mul(7) ^ 0x5a).collect();
            round_trip(&bytes);
        }
        round_trip(b"hello, world\n");
    }

    #[test]
    fn oversized_chunks_are_refused() {
        assert!(encode(&[0u8; MAX_CHUNK + 1]).is_none());
    }

    #[test]
    fn malformed_messages_are_rejected() {
        let mut out = [0u8; MAX_CHUNK];
        let mut message = encode(b"abc").unwrap();
        message.label = 2;
        assert_eq!(decode(&message, &mut out), None);

        let mut message = encode(b"abc").unwrap();
        message.words[0] = MAX_CHUNK as u64 + 1;
        assert_eq!(decode(&message, &mut out), None);

        let mut message = encode(b"abc").unwrap();
        message.words[0] = 20; // claims three words of data, only one is declared
        assert_eq!(decode(&message, &mut out), None);

        let message = Message {
            label: WRITE,
            ..Message::default()
        };
        assert_eq!(decode(&message, &mut out), None);
    }
}
