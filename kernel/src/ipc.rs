//! Kernel IPC primitives for P2.7 and P2.8.
//!
//! This module deliberately stops at the object layer: endpoint queues and notification words
//! are ready for the syscall and scheduler glue, while all operations remain non-blocking until
//! threads and the scheduler are introduced.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use carv_caps::{CSpace, CSpaceError, Capability};

use crate::sync::SpinLock;

/// Maximum number of machine words carried inline by one IPC message (ABI v1 limit).
pub const INLINE_WORDS: usize = carv_abi::MESSAGE_WORDS;
/// Maximum number of capabilities carried by one IPC message (ABI v1 limit).
pub const TRANSFERRED_CAPS: usize = carv_abi::MESSAGE_CAPS;

/// A small, register-sized IPC payload and optional capability transfer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Message {
    /// Number of valid entries in [`Message::words`].
    pub len: usize,
    /// Inline payload words.
    pub words: [u64; INLINE_WORDS],
    /// Capabilities copied into the receiver's CSpace by the IPC glue.
    pub caps: [Option<Capability>; TRANSFERRED_CAPS],
}

impl Message {
    /// Creates an empty message.
    pub const fn empty() -> Self {
        Self {
            len: 0,
            words: [0; INLINE_WORDS],
            caps: [None; TRANSFERRED_CAPS],
        }
    }

    /// Creates a message from an inline payload.
    pub fn from_words(words: &[u64]) -> Option<Self> {
        if words.len() > INLINE_WORDS {
            return None;
        }
        let mut message = Self::empty();
        message.len = words.len();
        message.words[..words.len()].copy_from_slice(words);
        Some(message)
    }

    /// Adds a capability to the next free transfer slot.
    pub fn push_cap(&mut self, cap: Capability) -> bool {
        if let Some(slot) = self.caps.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some(cap);
            true
        } else {
            false
        }
    }

    /// Installs all transferred capabilities into empty receiver slots.
    pub fn install_caps(&self, cspace: &mut CSpace, slots: &[usize]) -> Result<(), CSpaceError> {
        for (slot_index, cap) in self.caps.iter().flatten().enumerate() {
            let Some(&slot) = slots.get(slot_index) else {
                return Err(CSpaceError::SlotOutOfRange);
            };
            cspace.insert(slot, cap.object(), cap.rights(), cap.badge())?;
        }
        Ok(())
    }
}

/// A reply token created by [`Endpoint::call`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplyToken(u64);

/// A message received from an endpoint, including the sender badge and optional reply token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Received {
    /// Badge attached to the sender's endpoint capability.
    pub badge: u64,
    /// Message payload and transferred capabilities.
    pub message: Message,
    /// Token to use with [`Endpoint::reply`], present for calls.
    pub reply: Option<ReplyToken>,
}

struct Request {
    received: Received,
}

struct EndpointState {
    next_reply: u64,
    queue: Vec<Request>,
    replies: BTreeMap<u64, Message>,
}

/// Synchronous IPC endpoint queue.
pub struct Endpoint {
    state: SpinLock<EndpointState>,
}

impl Endpoint {
    /// Creates an empty endpoint.
    pub const fn new() -> Self {
        Self {
            state: SpinLock::new(EndpointState {
                next_reply: 1,
                queue: Vec::new(),
                replies: BTreeMap::new(),
            }),
        }
    }

    /// Enqueues a one-way message with the sender badge.
    pub fn send(&self, badge: u64, message: Message) {
        self.state.lock().queue.push(Request {
            received: Received {
                badge,
                message,
                reply: None,
            },
        });
    }

    /// Dequeues the oldest message, if one is available.
    pub fn recv(&self) -> Option<Received> {
        let mut state = self.state.lock();
        if state.queue.is_empty() {
            None
        } else {
            Some(state.queue.remove(0).received)
        }
    }

    /// Enqueues a call and returns its one-shot reply token.
    pub fn call(&self, badge: u64, message: Message) -> ReplyToken {
        let mut state = self.state.lock();
        let token = ReplyToken(state.next_reply);
        state.next_reply = state.next_reply.wrapping_add(1).max(1);
        state.queue.push(Request {
            received: Received {
                badge,
                message,
                reply: Some(token),
            },
        });
        token
    }

    /// Completes a call. A token can be replied to at most once.
    pub fn reply(&self, token: ReplyToken, message: Message) -> bool {
        let mut state = self.state.lock();
        if state.replies.contains_key(&token.0) {
            return false;
        }
        state.replies.insert(token.0, message).is_none()
    }

    /// Completes an optional reply and receives the next request (server fast path).
    pub fn reply_recv(&self, reply: Option<ReplyToken>, message: Message) -> Option<Received> {
        if let Some(token) = reply {
            assert!(self.reply(token, message), "reply token was already used");
        }
        self.recv()
    }

    /// Takes a completed reply, if the caller's token has been answered.
    pub fn take_reply(&self, token: ReplyToken) -> Option<Message> {
        self.state.lock().replies.remove(&token.0)
    }
}

impl Default for Endpoint {
    fn default() -> Self {
        Self::new()
    }
}

/// An asynchronous signal word used for events and IRQ delivery.
pub struct Notification {
    pending: AtomicU64,
}

impl Notification {
    /// Creates a cleared notification.
    pub const fn new() -> Self {
        Self {
            pending: AtomicU64::new(0),
        }
    }

    /// Adds one signal to the notification, saturating on overflow.
    pub fn signal(&self) {
        let _ = self
            .pending
            .try_update(Ordering::AcqRel, Ordering::Acquire, |v| v.checked_add(1));
    }

    /// Consumes one pending signal, or returns `false` when none is pending.
    pub fn wait(&self) -> bool {
        self.pending
            .try_update(Ordering::AcqRel, Ordering::Acquire, |v| v.checked_sub(1))
            .is_ok()
    }

    /// Returns the number of pending signals.
    pub fn pending(&self) -> u64 {
        self.pending.load(Ordering::Acquire)
    }
}

impl Default for Notification {
    fn default() -> Self {
        Self::new()
    }
}

/// Capability-like handle for routing one hardware interrupt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Irq {
    /// Global system interrupt number.
    pub gsi: u32,
    /// Badge delivered with the notification.
    pub badge: u64,
}

struct Route {
    irq: Irq,
    notification: Arc<Notification>,
}

/// Routes hardware interrupt numbers to notification objects.
pub struct IrqRouter {
    routes: SpinLock<Vec<Route>>,
}

impl IrqRouter {
    /// Creates an empty IRQ router.
    pub const fn new() -> Self {
        Self {
            routes: SpinLock::new(Vec::new()),
        }
    }

    /// Binds an IRQ to a notification, rejecting duplicate GSI routes.
    pub fn bind(&self, irq: Irq, notification: Arc<Notification>) -> bool {
        let mut routes = self.routes.lock();
        if routes.iter().any(|route| route.irq.gsi == irq.gsi) {
            return false;
        }
        routes.push(Route { irq, notification });
        true
    }

    /// Delivers one interrupt and returns the routed badge, if any.
    pub fn dispatch(&self, gsi: u32) -> Option<u64> {
        let routes = self.routes.lock();
        let route = routes.iter().find(|route| route.irq.gsi == gsi)?;
        route.notification.signal();
        Some(route.irq.badge)
    }
}

impl Default for IrqRouter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::sync::Arc;
    use carv_caps::{CSpace, Rights};

    #[test_case]
    fn endpoint_badges_caps_and_reply_round_trip() {
        let endpoint = Endpoint::new();
        let mut sender = CSpace::new(2);
        sender
            .insert(0, 0xCAFE, Rights::READ | Rights::DERIVE, 7)
            .unwrap();
        let mut message = Message::from_words(&[0x1234]).unwrap();
        assert!(message.push_cap(sender.get(0).unwrap()));

        let token = endpoint.call(42, message);
        let request = endpoint.recv().expect("call must be queued");
        assert_eq!(request.badge, 42);
        let mut receiver = CSpace::new(2);
        request.message.install_caps(&mut receiver, &[1]).unwrap();
        assert_eq!(receiver.get(1).unwrap().object(), 0xCAFE);
        assert_eq!(receiver.get(1).unwrap().badge(), 7);
        endpoint.reply(token, Message::from_words(&[0xBEEF]).unwrap());
        assert_eq!(endpoint.take_reply(token).unwrap().words[0], 0xBEEF);
        assert!(endpoint.take_reply(token).is_none());
    }

    #[test_case]
    fn message_limits_match_abi_v1() {
        let full = [0u64; carv_abi::MESSAGE_WORDS];
        assert!(Message::from_words(&full).is_some());
        let over = [0u64; carv_abi::MESSAGE_WORDS + 1];
        assert!(Message::from_words(&over).is_none());
        assert_eq!(Message::empty().caps.len(), carv_abi::MESSAGE_CAPS);
    }

    #[test_case]
    fn notification_and_irq_routing() {
        let notification = Arc::new(Notification::new());
        let router = IrqRouter::new();
        assert!(router.bind(
            Irq {
                gsi: 1,
                badge: 0x55
            },
            notification.clone()
        ));
        assert!(!router.bind(
            Irq {
                gsi: 1,
                badge: 0x99
            },
            notification.clone()
        ));
        assert_eq!(router.dispatch(99), None);
        assert_eq!(router.dispatch(1), Some(0x55));
        assert!(notification.wait());
        assert!(!notification.wait());
    }
}
