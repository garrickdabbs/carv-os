//! IPC object state for P2.7 and P2.8: endpoints, notifications and interrupt lines.
//!
//! The objects here only hold queues and words; the blocking operations that move messages
//! between threads live in [`crate::syscalls`], which owns the rendezvous logic, and
//! [`crate::scheduler`], which blocks and wakes threads. Messages are always the ABI v1
//! [`carv_abi::Message`] copied in from and out to user memory, so the only capabilities a
//! message can carry are slots of the sender's own CSpace, transferred by the kernel with
//! `carv_caps::CapSpaces::transfer` (a message can never forge a capability).

use alloc::collections::VecDeque;

use carv_abi::{MESSAGE_CAPS, MESSAGE_WORDS, Message};

use crate::objects::ObjectId;
use crate::scheduler::ThreadId;

/// A synchronous rendezvous point: at most one of the two queues is non-empty.
#[derive(Debug, Default)]
pub struct Endpoint {
    /// Threads blocked in send or call, oldest first; their message is in their control block.
    pub senders: VecDeque<ThreadId>,
    /// Threads blocked in recv or reply_recv, oldest first.
    pub receivers: VecDeque<ThreadId>,
}

impl Endpoint {
    /// An endpoint with nobody waiting.
    pub const fn new() -> Self {
        Self {
            senders: VecDeque::new(),
            receivers: VecDeque::new(),
        }
    }

    /// Forgets `thread` (it was killed or its wait was cancelled).
    pub fn remove(&mut self, thread: ThreadId) {
        self.senders.retain(|t| *t != thread);
        self.receivers.retain(|t| *t != thread);
    }
}

/// An asynchronous signal word: signals accumulate until a waiter takes them.
#[derive(Debug, Default)]
pub struct Notification {
    /// Signals not yet consumed by a wait.
    pub pending: u64,
    /// Threads blocked in wait, oldest first.
    pub waiters: VecDeque<ThreadId>,
}

impl Notification {
    /// A notification with no pending signals.
    pub const fn new() -> Self {
        Self {
            pending: 0,
            waiters: VecDeque::new(),
        }
    }

    /// Records one signal. Returns a waiter to wake with the accumulated count, if any; the
    /// count is then consumed.
    pub fn signal(&mut self) -> Option<(ThreadId, u64)> {
        self.pending = self.pending.saturating_add(1);
        let waiter = self.waiters.pop_front()?;
        Some((waiter, core::mem::take(&mut self.pending)))
    }

    /// Takes the accumulated count if there is one.
    pub fn poll(&mut self) -> Option<u64> {
        (self.pending > 0).then(|| core::mem::take(&mut self.pending))
    }
}

/// One hardware interrupt line (a global system interrupt) and the notification it signals.
#[derive(Debug)]
pub struct IrqLine {
    /// The IOAPIC input this object stands for.
    pub gsi: u8,
    /// Notification signalled on every interrupt, once bound.
    pub notification: Option<ObjectId>,
}

/// Checks a message copied in from user memory against the ABI v1 rules: counts within the
/// limits ([`carv_abi::Error::MessageTooLarge`]) and every unused word and cap zero
/// ([`carv_abi::Error::InvalidArgument`]).
pub fn validate(message: &Message) -> Result<(), carv_abi::Error> {
    let (words, caps) = (message.info.words(), message.info.caps());
    if words > MESSAGE_WORDS || caps > MESSAGE_CAPS {
        return Err(carv_abi::Error::MessageTooLarge);
    }
    let canonical = carv_abi::MessageInfo::new(words, caps).expect("checked above");
    if canonical != message.info
        || message.words[words..].iter().any(|w| *w != 0)
        || message.caps[caps..].iter().any(|c| *c != 0)
    {
        return Err(carv_abi::Error::InvalidArgument);
    }
    Ok(())
}

/// Views a message as bytes for copying to user memory.
pub fn as_bytes(message: &Message) -> &[u8] {
    // SAFETY: `Message` is `repr(C)`, made only of `u64`s (no padding), so all of its
    // `size_of::<Message>()` bytes are initialised and any byte view of it is valid.
    unsafe {
        core::slice::from_raw_parts(
            core::ptr::from_ref(message).cast::<u8>(),
            size_of::<Message>(),
        )
    }
}

/// Builds a message from bytes copied in from user memory.
pub fn from_bytes(bytes: &[u8; size_of::<Message>()]) -> Message {
    // SAFETY: `Message` is `repr(C)` and made only of `u64`s and a transparent `u64`, so every
    // bit pattern is valid; `read_unaligned` copes with the byte array's alignment.
    unsafe { core::ptr::read_unaligned(bytes.as_ptr().cast::<Message>()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test_case]
    fn message_validation_matches_abi_v1() {
        let mut m = Message {
            label: 7,
            info: carv_abi::MessageInfo::new(2, 1).unwrap(),
            ..Message::default()
        };
        m.words[..2].copy_from_slice(&[1, 2]);
        m.caps[0] = 5;
        assert_eq!(validate(&m), Ok(()));
        let round = from_bytes(as_bytes(&m).try_into().unwrap());
        assert_eq!(round, m);
        m.words[3] = 9;
        assert_eq!(validate(&m), Err(carv_abi::Error::InvalidArgument));
        let mut bytes: [u8; 96] = as_bytes(&Message::default()).try_into().unwrap();
        bytes[8] = 7; // seven words
        assert_eq!(
            validate(&from_bytes(&bytes)),
            Err(carv_abi::Error::MessageTooLarge)
        );
        bytes[8] = 0;
        bytes[12] = 1; // garbage high bits in the info word
        assert_eq!(
            validate(&from_bytes(&bytes)),
            Err(carv_abi::Error::InvalidArgument)
        );
    }

    #[test_case]
    fn notifications_accumulate_until_taken() {
        let mut n = Notification::new();
        assert_eq!(n.poll(), None);
        assert_eq!(n.signal(), None);
        assert_eq!(n.signal(), None);
        assert_eq!(n.poll(), Some(2));
        n.waiters.push_back(9);
        assert_eq!(n.signal(), Some((9, 1)));
        assert_eq!(n.poll(), None);
    }
}
