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

/// Kernel object types, as reported by [`method::DESCRIBE`] and requested by
/// [`method::BUDGET_CREATE`] (ADR-0003).
#[repr(u64)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectType {
    /// An execution context.
    Thread = 1,
    /// A user page-table root.
    AddressSpace = 2,
    /// One physical 4 KiB frame.
    Frame = 3,
    /// A synchronous IPC endpoint.
    Endpoint = 4,
    /// An asynchronous signal word.
    Notification = 5,
    /// A CPU and memory account.
    Budget = 6,
    /// A one-shot IPC reply object.
    Reply = 7,
    /// One hardware interrupt line (global system interrupt).
    Irq = 8,
    /// The authority to create [`ObjectType::Irq`] capabilities.
    IrqControl = 9,
}

impl ObjectType {
    /// Converts a raw object type, rejecting values not defined by this ABI.
    pub const fn from_raw(raw: u64) -> Option<Self> {
        match raw {
            1 => Some(Self::Thread),
            2 => Some(Self::AddressSpace),
            3 => Some(Self::Frame),
            4 => Some(Self::Endpoint),
            5 => Some(Self::Notification),
            6 => Some(Self::Budget),
            7 => Some(Self::Reply),
            8 => Some(Self::Irq),
            9 => Some(Self::IrqControl),
            _ => None,
        }
    }
}

/// `invoke` method numbers (ADR-0003). `invoke(cap, method, a0, a1, a2, a3)` takes the capability
/// slot in `rdi`, the method in `rsi` and arguments in `rdx, r10, r8, r9`; it returns the status in
/// `rax` and results in `rdx` and `rsi`.
pub mod method {
    /// Any object: returns the [`super::ObjectType`] in `rdx`. Needs no right.
    pub const DESCRIBE: u64 = 0;
    /// Any object except the root budget: destroys it and credits its budget. Needs `WRITE`.
    /// Destroying one's own thread ends it.
    pub const DESTROY: u64 = 1;
    /// Budget: returns memory bytes used in `rdx` and the memory limit in `rsi`. Needs `READ`.
    pub const BUDGET_READ: u64 = 2;
    /// Budget: creates an object of type `a0` charged to this budget and installs a capability
    /// with all rights in the caller's empty slot `a1`. For a child budget, `a2` is its CPU
    /// allowance per period (ns) and `a3` its memory limit (bytes), both carved out of this
    /// budget. Needs `WRITE`.
    pub const BUDGET_CREATE: u64 = 3;
    /// Thread (not yet started): starts it at `rip = a0`, `rsp = a1` with `rdi = a2`, sharing the
    /// caller's CSpace, in the address space named by slot `a3` (or the caller's own when
    /// `a3 == u64::MAX`). Needs `WRITE`.
    pub const THREAD_START: u64 = 4;
    /// Address space: maps a fresh zeroed frame at page `a0` with [`map`] flags `a1`, charged to
    /// the address space's budget. Needs `WRITE`.
    pub const ADDRESS_SPACE_MAP: u64 = 5;
    /// IRQ control: creates an [`super::ObjectType::Irq`] capability for global system interrupt
    /// `a0` in the caller's empty slot `a1`. Needs `WRITE`.
    pub const IRQ_CONTROL_GET: u64 = 6;
    /// Irq: delivers the interrupt to the notification in slot `a0` (which needs `WRITE`) and
    /// unmasks the line. Needs `WRITE`.
    pub const IRQ_BIND: u64 = 7;
    /// Irq: masks the line and drops its notification. Needs `WRITE`.
    pub const IRQ_UNBIND: u64 = 8;
}

/// Flags for [`method::ADDRESS_SPACE_MAP`]. Writable and executable together are rejected (W^X).
pub mod map {
    /// The page is writable.
    pub const WRITE: u64 = 1 << 0;
    /// The page is executable.
    pub const EXEC: u64 = 1 << 1;
}

/// Layout of the root task's initial CSpace and stack (ADR-0003).
pub mod init {
    /// Number of slots in the root task's CSpace.
    pub const CSPACE_SLOTS: usize = 64;
    /// Slot holding the root budget (the system's untyped authority).
    pub const ROOT_BUDGET: u64 = 0;
    /// Slot holding the root task's own thread.
    pub const THREAD: u64 = 1;
    /// Slot holding the root task's own address space.
    pub const ADDRESS_SPACE: u64 = 2;
    /// Slot holding the IRQ control capability.
    pub const IRQ_CONTROL: u64 = 3;
    /// First empty slot.
    pub const FIRST_FREE: u64 = 4;
    /// Top of the root task's initial stack (exclusive); `STACK_PAGES` pages are mapped below it.
    pub const STACK_TOP: u64 = 0x0000_7fff_0000_0000;
    /// Pages of initial stack.
    pub const STACK_PAGES: u64 = 4;
}

/// First address past the user half of every address space; user pages lie below it.
pub const USER_TOP: u64 = 0x0000_7fff_ffff_f000;

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

    #[test]
    fn object_types_and_methods_are_stable() {
        assert_eq!(ObjectType::from_raw(1), Some(ObjectType::Thread));
        assert_eq!(ObjectType::from_raw(9), Some(ObjectType::IrqControl));
        assert_eq!(ObjectType::from_raw(0), None);
        assert_eq!(ObjectType::from_raw(10), None);
        assert_eq!(method::IRQ_UNBIND, 8);
        assert_eq!(init::FIRST_FREE, 4);
        const { assert!(init::STACK_TOP <= USER_TOP) };
    }
}
