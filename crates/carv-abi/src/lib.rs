//! The shared syscall and IPC ABI for CarvOS.
//!
//! This crate contains representation-stable values only.  The wire format is described in
//! [`docs/abi.md`](https://github.com/garrickdabbs/carv-os/blob/main/docs/abi.md).

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(clippy::undocumented_unsafe_blocks)]

use core::ops::{BitOr, BitOrAssign};

/// The syscall numbers understood by the kernel.
#[repr(u64)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Syscall {
    /// Write one debugging byte (available only in debug builds).
    DebugPutc = 0,
    /// Send a message to an endpoint.
    Send = 1,
    /// Receive a message from an endpoint.
    Recv = 2,
    /// Send a message and wait for a reply.
    Call = 3,
    /// Receive a message while sending a reply.
    ReplyRecv = 4,
    /// Signal a notification.
    Signal = 5,
    /// Wait for a notification.
    Wait = 6,
    /// Yield the current thread.
    Yield = 7,
    /// Invoke a method on a kernel object.
    Invoke = 8,
}

impl Syscall {
    /// Converts a raw syscall number, rejecting numbers not defined by this ABI.
    pub const fn from_raw(number: u64) -> Option<Self> {
        match number {
            0 => Some(Self::DebugPutc),
            1 => Some(Self::Send),
            2 => Some(Self::Recv),
            3 => Some(Self::Call),
            4 => Some(Self::ReplyRecv),
            5 => Some(Self::Signal),
            6 => Some(Self::Wait),
            7 => Some(Self::Yield),
            8 => Some(Self::Invoke),
            _ => None,
        }
    }
}

/// Errors returned by a syscall.
#[repr(u64)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The syscall number or object method is not defined.
    InvalidOperation = 1,
    /// A capability handle is invalid or absent.
    InvalidCapability = 2,
    /// The capability does not grant the requested right.
    PermissionDenied = 3,
    /// An argument or message is malformed.
    InvalidArgument = 4,
    /// The operation cannot complete without waiting.
    WouldBlock = 5,
    /// The endpoint or notification has no peer.
    Closed = 6,
    /// The caller's budget cannot pay for this operation.
    BudgetExhausted = 7,
    /// A kernel allocation could not be satisfied.
    OutOfMemory = 8,
    /// The message is larger than the ABI permits.
    MessageTooLarge = 9,
}

/// Alias retaining the descriptive name used in ABI documentation.
pub type AbiError = Error;

impl Error {
    /// Converts a raw error code, returning `None` for an unassigned code.
    pub const fn from_raw(code: u64) -> Option<Self> {
        match code {
            1 => Some(Self::InvalidOperation),
            2 => Some(Self::InvalidCapability),
            3 => Some(Self::PermissionDenied),
            4 => Some(Self::InvalidArgument),
            5 => Some(Self::WouldBlock),
            6 => Some(Self::Closed),
            7 => Some(Self::BudgetExhausted),
            8 => Some(Self::OutOfMemory),
            9 => Some(Self::MessageTooLarge),
            _ => None,
        }
    }

    /// Returns this error's wire representation.
    pub const fn raw(self) -> u64 {
        self as u64
    }
}

/// Number of general-purpose words carried by a message.
pub const MESSAGE_WORDS: usize = 6;
/// Number of capability handles carried by a message.
pub const MESSAGE_CAPS: usize = 4;

/// The fixed-size message transferred by an IPC operation.
///
/// The `info` field contains the number of valid words and capabilities; unused array entries
/// must be zero.  The `repr(C)` layout is 96 bytes: `label`, `info`, six words, then four caps.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Message {
    /// Operation-specific label.
    pub label: u64,
    /// Encoded word and capability counts (see [`MessageInfo`]).
    pub info: MessageInfo,
    /// Message payload words.
    pub words: [u64; MESSAGE_WORDS],
    /// Capability slots to transfer.
    pub caps: [u64; MESSAGE_CAPS],
}

/// Counts encoded in a [`Message`]'s info word.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MessageInfo(u64);

impl MessageInfo {
    /// Creates counts after checking the ABI limits.
    pub const fn new(words: usize, caps: usize) -> Option<Self> {
        if words <= MESSAGE_WORDS && caps <= MESSAGE_CAPS {
            Some(Self(words as u64 | ((caps as u64) << 8)))
        } else {
            None
        }
    }

    /// Number of valid payload words.
    pub const fn words(self) -> usize {
        (self.0 & 0xff) as usize
    }

    /// Number of transferred capabilities.
    pub const fn caps(self) -> usize {
        ((self.0 >> 8) & 0xff) as usize
    }
}

/// Permissions attached to a capability.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rights(u8);

impl Rights {
    /// Permission to read an object.
    pub const READ: Self = Self(1 << 0);
    /// Permission to write an object.
    pub const WRITE: Self = Self(1 << 1);
    /// Permission to grant a capability to another party.
    pub const GRANT: Self = Self(1 << 2);
    /// Permission to execute an object.
    pub const EXEC: Self = Self(1 << 3);
    /// Permission to derive a capability by copying it.
    pub const DERIVE: Self = Self(1 << 4);
    /// Permission to revoke derived capabilities.
    pub const REVOKE: Self = Self(1 << 5);
    /// All rights defined by this ABI.
    pub const ALL: Self = Self(0x3f);

    /// Constructs rights from raw bits, rejecting undefined bits.
    pub const fn from_bits(bits: u8) -> Option<Self> {
        if bits & !Self::ALL.0 == 0 {
            Some(Self(bits))
        } else {
            None
        }
    }

    /// Returns the wire representation.
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Tests whether all rights in `other` are present.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

impl BitOr for Rights {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for Rights {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{align_of, size_of};

    #[test]
    fn syscall_numbers_are_stable() {
        assert_eq!(Syscall::Invoke as u64, 8);
        assert_eq!(Syscall::from_raw(5), Some(Syscall::Signal));
        assert_eq!(Syscall::from_raw(9), None);
    }

    #[test]
    fn errors_round_trip() {
        assert_eq!(
            Error::from_raw(Error::BudgetExhausted.raw()),
            Some(Error::BudgetExhausted)
        );
        assert_eq!(Error::from_raw(0), None);
    }

    #[test]
    fn message_and_rights_layout_is_bounded() {
        assert_eq!(size_of::<Message>(), 96);
        assert_eq!(align_of::<Message>(), 8);
        assert_eq!(MessageInfo::new(6, 4).unwrap().caps(), 4);
        assert!(MessageInfo::new(7, 0).is_none());
        assert_eq!((Rights::READ | Rights::WRITE).bits(), 3);
        assert!(Rights::ALL.contains(Rights::REVOKE));
        assert!(Rights::from_bits(0x40).is_none());
    }
}
