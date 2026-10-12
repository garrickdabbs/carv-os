//! Syscall status decoding.

use carv_abi::Error;

/// A nonzero syscall status.
///
/// The ABI reserves 0 for success and tells callers to treat unknown codes as an unassigned error,
/// so the raw code is kept and [`SysError::kind`] maps it to an [`Error`] when it is assigned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SysError(u64);

impl SysError {
    /// The raw nonzero status returned in `rax`.
    pub const fn code(self) -> u64 {
        self.0
    }

    /// The assigned ABI error, or `None` for an unassigned code.
    pub const fn kind(self) -> Option<Error> {
        Error::from_raw(self.0)
    }
}

/// Result of a syscall wrapper.
pub type Result<T> = core::result::Result<T, SysError>;

/// Splits a raw `rax` status into success or [`SysError`].
pub const fn check(status: u64) -> Result<()> {
    if status == 0 {
        Ok(())
    } else {
        Err(SysError(status))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_is_success_and_nonzero_keeps_its_code() {
        assert_eq!(check(0), Ok(()));
        let e = check(Error::BudgetExhausted.raw()).unwrap_err();
        assert_eq!(e.code(), 7);
        assert_eq!(e.kind(), Some(Error::BudgetExhausted));
    }

    #[test]
    fn unassigned_codes_are_preserved() {
        let e = check(99).unwrap_err();
        assert_eq!(e.code(), 99);
        assert_eq!(e.kind(), None);
    }
}
