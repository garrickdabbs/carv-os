//! Pure CPU, memory, hierarchical-limit, and token-bucket accounting.
//!
//! A [`Budget`] can carve child budgets out of its currently uncommitted CPU allowance and
//! memory limit. Children inherit the parent's CPU period and phase, keeping the hierarchy's
//! replenishment synchronized. Time is supplied by the caller in nanoseconds so the logic is
//! deterministic and host-testable.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(clippy::undocumented_unsafe_blocks)]

const NANOS_PER_SECOND: u128 = 1_000_000_000;

/// Errors returned while creating or using a budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BudgetError {
    /// A CPU period must be nonzero.
    ZeroPeriod,
    /// A CPU allowance cannot exceed its period.
    CpuBudgetExceedsPeriod,
    /// The requested child CPU allowance exceeds the parent's uncommitted allowance.
    CpuLimitExceeded,
    /// The requested child memory limit exceeds the parent's uncommitted memory.
    MemoryLimitExceeded,
    /// The requested memory charge exceeds the remaining limit.
    MemoryLimitReached,
    /// More memory was released than was previously charged.
    MemoryUnderflow,
}

/// CPU and memory limits assigned to one budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    cpu_budget_ns: u64,
    cpu_period_ns: u64,
    memory_bytes: u64,
}

impl Limits {
    /// CPU allowance available in each period, in nanoseconds.
    pub const fn cpu_budget_ns(&self) -> u64 {
        self.cpu_budget_ns
    }

    /// Period over which the CPU allowance is replenished, in nanoseconds.
    pub const fn cpu_period_ns(&self) -> u64 {
        self.cpu_period_ns
    }

    /// Maximum memory charge, in bytes.
    pub const fn memory_bytes(&self) -> u64 {
        self.memory_bytes
    }
}

/// A CPU and memory account with synchronized, hierarchical child carve-outs.
#[derive(Debug)]
pub struct Budget {
    limits: Limits,
    period_start_ns: u64,
    cpu_remaining_ns: u64,
    child_cpu_reserved_ns: u64,
    memory_used_bytes: u64,
    child_memory_reserved_bytes: u64,
}

impl Budget {
    /// Creates a budget with the full CPU allowance available at `now_ns`.
    ///
    /// # Errors
    /// Returns [`BudgetError::ZeroPeriod`] for a zero period or
    /// [`BudgetError::CpuBudgetExceedsPeriod`] when the allowance exceeds the period.
    pub fn new(
        cpu_budget_ns: u64,
        cpu_period_ns: u64,
        memory_bytes: u64,
        now_ns: u64,
    ) -> Result<Self, BudgetError> {
        if cpu_period_ns == 0 {
            return Err(BudgetError::ZeroPeriod);
        }
        if cpu_budget_ns > cpu_period_ns {
            return Err(BudgetError::CpuBudgetExceedsPeriod);
        }

        Ok(Self {
            limits: Limits {
                cpu_budget_ns,
                cpu_period_ns,
                memory_bytes,
            },
            period_start_ns: now_ns,
            cpu_remaining_ns: cpu_budget_ns,
            child_cpu_reserved_ns: 0,
            memory_used_bytes: 0,
            child_memory_reserved_bytes: 0,
        })
    }

    /// Limits assigned to this budget.
    pub const fn limits(&self) -> Limits {
        self.limits
    }

    /// CPU allowance currently available to this budget, excluding child reservations.
    pub const fn cpu_remaining_ns(&self) -> u64 {
        self.cpu_remaining_ns
    }

    /// Memory charged directly to this budget.
    pub const fn memory_used_bytes(&self) -> u64 {
        self.memory_used_bytes
    }

    /// Advances to the current CPU period, replenishing the allowance when due.
    ///
    /// Complete periods are accounted for while preserving the original period phase. A clock
    /// value earlier than the last refill is ignored. Returns whether a period was replenished.
    pub fn refill(&mut self, now_ns: u64) -> bool {
        if now_ns < self.period_start_ns {
            return false;
        }

        let elapsed = now_ns - self.period_start_ns;
        if elapsed < self.limits.cpu_period_ns {
            return false;
        }

        self.period_start_ns = now_ns - elapsed % self.limits.cpu_period_ns;
        self.cpu_remaining_ns = self.limits.cpu_budget_ns - self.child_cpu_reserved_ns;
        true
    }

    /// Charges CPU time if this budget has enough allowance after refilling at `now_ns`.
    pub fn charge_cpu(&mut self, amount_ns: u64, now_ns: u64) -> bool {
        self.refill(now_ns);
        if amount_ns > self.cpu_remaining_ns {
            return false;
        }
        self.cpu_remaining_ns -= amount_ns;
        true
    }

    /// Charges memory, returning an error if the local limit (after child reservations) is reached.
    pub fn charge_memory(&mut self, amount_bytes: u64) -> Result<(), BudgetError> {
        let available = self
            .limits
            .memory_bytes
            .saturating_sub(self.child_memory_reserved_bytes)
            .saturating_sub(self.memory_used_bytes);
        if amount_bytes > available {
            return Err(BudgetError::MemoryLimitReached);
        }
        self.memory_used_bytes += amount_bytes;
        Ok(())
    }

    /// Credits previously charged memory back to this budget.
    ///
    /// # Errors
    /// Returns [`BudgetError::MemoryUnderflow`] if `amount_bytes` exceeds the current charge.
    pub fn release_memory(&mut self, amount_bytes: u64) -> Result<(), BudgetError> {
        if amount_bytes > self.memory_used_bytes {
            return Err(BudgetError::MemoryUnderflow);
        }
        self.memory_used_bytes -= amount_bytes;
        Ok(())
    }

    /// Carves a child out of this budget's currently uncommitted limits.
    ///
    /// The child inherits the parent's CPU period and phase. Its initial CPU allowance is
    /// reserved from the parent's remaining allowance for the current period, and its memory cap
    /// is reserved from the parent's uncharged, unreserved memory.
    ///
    /// # Errors
    /// Returns [`BudgetError::CpuLimitExceeded`] or [`BudgetError::MemoryLimitExceeded`] if either
    /// requested child limit cannot be reserved. Neither budget changes on error.
    pub fn carve_out(
        &mut self,
        cpu_budget_ns: u64,
        memory_bytes: u64,
        now_ns: u64,
    ) -> Result<Self, BudgetError> {
        self.refill(now_ns);

        if cpu_budget_ns > self.cpu_remaining_ns {
            return Err(BudgetError::CpuLimitExceeded);
        }
        let available_memory = self
            .limits
            .memory_bytes
            .saturating_sub(self.child_memory_reserved_bytes)
            .saturating_sub(self.memory_used_bytes);
        if memory_bytes > available_memory {
            return Err(BudgetError::MemoryLimitExceeded);
        }

        self.cpu_remaining_ns -= cpu_budget_ns;
        self.child_cpu_reserved_ns += cpu_budget_ns;
        self.child_memory_reserved_bytes += memory_bytes;

        Ok(Self {
            limits: Limits {
                cpu_budget_ns,
                cpu_period_ns: self.limits.cpu_period_ns,
                memory_bytes,
            },
            period_start_ns: self.period_start_ns,
            cpu_remaining_ns: cpu_budget_ns,
            child_cpu_reserved_ns: 0,
            memory_used_bytes: 0,
            child_memory_reserved_bytes: 0,
        })
    }
}

/// A byte/token bucket replenished at a fixed rate with a bounded burst capacity.
pub struct TokenBucket {
    capacity: u64,
    tokens: u64,
    rate_per_second: u64,
    last_refill_ns: u64,
    fractional_numerator: u64,
}

impl TokenBucket {
    /// Creates a full bucket with `capacity` tokens and the given refill rate.
    pub const fn new(capacity: u64, rate_per_second: u64, now_ns: u64) -> Self {
        Self {
            capacity,
            tokens: capacity,
            rate_per_second,
            last_refill_ns: now_ns,
            fractional_numerator: 0,
        }
    }

    /// Current number of available tokens.
    pub const fn tokens(&self) -> u64 {
        self.tokens
    }

    /// Replenishes tokens according to elapsed simulated nanoseconds, capped at capacity.
    ///
    /// Fractional tokens are retained across calls for exact long-term refill rates. Earlier
    /// timestamps are ignored.
    pub fn refill(&mut self, now_ns: u64) {
        if now_ns <= self.last_refill_ns {
            return;
        }

        let elapsed = now_ns - self.last_refill_ns;
        self.last_refill_ns = now_ns;

        let numerator = u128::from(elapsed) * u128::from(self.rate_per_second)
            + u128::from(self.fractional_numerator);
        let added = numerator / NANOS_PER_SECOND;
        let fraction = (numerator % NANOS_PER_SECOND) as u64;
        let available = self.capacity - self.tokens;

        if added >= u128::from(available) {
            self.tokens = self.capacity;
            self.fractional_numerator = 0;
        } else {
            self.tokens += added as u64;
            self.fractional_numerator = fraction;
        }
    }

    /// Refills at `now_ns` and consumes `amount` tokens if available.
    pub fn try_consume(&mut self, amount: u64, now_ns: u64) -> bool {
        self.refill(now_ns);
        if amount > self.tokens {
            return false;
        }
        self.tokens -= amount;
        true
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;

    #[test]
    fn validates_cpu_period_and_quota() {
        assert_eq!(
            Budget::new(1, 0, 0, 0).unwrap_err(),
            BudgetError::ZeroPeriod
        );
        assert_eq!(
            Budget::new(11, 10, 0, 0).unwrap_err(),
            BudgetError::CpuBudgetExceedsPeriod
        );
    }

    #[test]
    fn child_and_nested_limits_never_exceed_parent() {
        let mut parent = Budget::new(100, 1_000, 1_000, 0).unwrap();
        parent.charge_memory(100).unwrap();
        let mut child = parent.carve_out(40, 500, 0).unwrap();
        let grandchild = child.carve_out(20, 200, 0).unwrap();

        assert_eq!(child.limits().cpu_budget_ns(), 40);
        assert_eq!(child.limits().memory_bytes(), 500);
        assert_eq!(grandchild.limits().cpu_budget_ns(), 20);
        assert_eq!(grandchild.limits().memory_bytes(), 200);
        assert_eq!(
            parent.carve_out(61, 0, 0).unwrap_err(),
            BudgetError::CpuLimitExceeded
        );
        assert_eq!(
            parent.carve_out(0, 401, 0).unwrap_err(),
            BudgetError::MemoryLimitExceeded
        );
        assert_eq!(parent.cpu_remaining_ns(), 60);
        assert_eq!(
            parent.charge_memory(401),
            Err(BudgetError::MemoryLimitReached)
        );
    }

    #[test]
    fn cpu_refills_exactly_on_period_boundaries_and_preserves_phase() {
        let mut budget = Budget::new(60, 100, 0, 1_000).unwrap();
        assert!(budget.charge_cpu(60, 1_000));
        assert!(!budget.charge_cpu(1, 1_099));
        assert!(!budget.refill(1_099));
        assert_eq!(budget.cpu_remaining_ns(), 0);

        assert!(budget.refill(1_100));
        assert_eq!(budget.cpu_remaining_ns(), 60);
        assert!(budget.charge_cpu(30, 1_100));
        assert!(budget.refill(1_350)); // two periods elapsed
        assert_eq!(budget.cpu_remaining_ns(), 60);
        assert!(!budget.refill(1_349)); // a backwards timestamp changes nothing
        assert_eq!(budget.cpu_remaining_ns(), 60);
    }

    #[test]
    fn child_quota_is_removed_from_parent_and_refills_synchronously() {
        let mut parent = Budget::new(100, 100, 0, 0).unwrap();
        assert!(parent.charge_cpu(30, 0));
        let mut child = parent.carve_out(50, 0, 0).unwrap();
        assert_eq!(parent.cpu_remaining_ns(), 20);
        assert!(child.charge_cpu(50, 99));
        assert!(!child.charge_cpu(1, 99));
        assert!(parent.refill(100));
        assert!(child.refill(100));
        assert_eq!(parent.cpu_remaining_ns(), 50);
        assert_eq!(child.cpu_remaining_ns(), 50);
    }

    #[test]
    fn memory_charge_release_and_overflow_are_bounded() {
        let mut budget = Budget::new(0, 10, u64::MAX, 0).unwrap();
        budget.charge_memory(u64::MAX).unwrap();
        assert_eq!(budget.memory_used_bytes(), u64::MAX);
        assert_eq!(
            budget.charge_memory(1),
            Err(BudgetError::MemoryLimitReached)
        );
        assert_eq!(budget.release_memory(u64::MAX), Ok(()));
        assert_eq!(budget.release_memory(1), Err(BudgetError::MemoryUnderflow));
    }

    #[test]
    fn token_bucket_refill_is_exact_over_simulated_time() {
        let mut bucket = TokenBucket::new(10, 3, 0);
        assert!(bucket.try_consume(10, 0));
        bucket.refill(333_333_333);
        assert_eq!(bucket.tokens(), 0);
        bucket.refill(666_666_666);
        assert_eq!(bucket.tokens(), 1);
        bucket.refill(1_000_000_000);
        assert_eq!(bucket.tokens(), 3);
        assert!(!bucket.try_consume(4, 1_000_000_000));
        assert_eq!(bucket.tokens(), 3);
    }

    #[test]
    fn token_bucket_caps_bursts_and_ignores_backwards_time() {
        let mut bucket = TokenBucket::new(5, 10, 10);
        assert!(bucket.try_consume(4, 10));
        bucket.refill(510_000_010);
        assert_eq!(bucket.tokens(), 5);
        assert!(bucket.try_consume(5, 510_000_010));
        bucket.refill(100);
        assert_eq!(bucket.tokens(), 0);
    }
}
