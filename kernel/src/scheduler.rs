//! Budget-aware, timer-driven round-robin scheduling.
//!
//! CPU refill and charging come from [`carv_budget::Budget`]; this module only picks threads.

use alloc::vec::Vec;

use carv_budget::{Budget, BudgetError};

use crate::sync::SpinLock;

static SCHEDULER: SpinLock<Scheduler> = SpinLock::new(Scheduler::new());

/// A schedulable thread and its CPU budget.
#[derive(Debug)]
pub struct Thread {
    /// Stable identifier supplied by the object layer.
    pub id: u64,
    budget: Budget,
    runnable: bool,
}

impl Thread {
    /// Creates a runnable thread with a full CPU budget of `budget_ns` every `period_ns`.
    ///
    /// # Errors
    /// Returns the [`BudgetError`] from [`Budget::new`] for a zero period or a budget that
    /// exceeds its period.
    #[allow(dead_code)]
    pub fn new(id: u64, budget_ns: u64, period_ns: u64, now_ns: u64) -> Result<Self, BudgetError> {
        Ok(Self::with_budget(
            id,
            Budget::new(budget_ns, period_ns, 0, now_ns)?,
        ))
    }

    /// Creates a runnable thread that runs on `budget`, e.g. one carved out of a parent budget.
    #[allow(dead_code)]
    pub fn with_budget(id: u64, budget: Budget) -> Self {
        Self {
            id,
            budget,
            runnable: true,
        }
    }

    /// Charges `elapsed_ns` of CPU time; an overrun drains the rest of the period's allowance.
    fn charge(&mut self, elapsed_ns: u64, now_ns: u64) {
        if !self.budget.charge_cpu(elapsed_ns, now_ns) {
            let remaining = self.budget.cpu_remaining_ns();
            self.budget.charge_cpu(remaining, now_ns);
        }
    }
}

/// A single-core scheduler using FIFO round-robin among budget-eligible threads.
#[derive(Debug, Default)]
pub struct Scheduler {
    threads: Vec<Thread>,
    current: Option<usize>,
    now_ns: u64,
}

impl Scheduler {
    /// Creates an empty scheduler.
    pub const fn new() -> Self {
        Self {
            threads: Vec::new(),
            current: None,
            now_ns: 0,
        }
    }

    /// Adds a thread and returns its position.
    #[allow(dead_code)]
    pub fn add(&mut self, thread: Thread) -> usize {
        self.threads.push(thread);
        self.threads.len() - 1
    }

    /// Charges one timer quantum and selects the next eligible thread.
    pub fn tick(&mut self, quantum_ns: u64) -> Option<u64> {
        self.now_ns = self.now_ns.saturating_add(quantum_ns);
        if let Some(i) = self.current {
            self.threads[i].charge(quantum_ns, self.now_ns);
        }

        for offset in 1..=self.threads.len() {
            let i = self
                .current
                .map_or(offset - 1, |c| (c + offset) % self.threads.len());
            let t = &mut self.threads[i];
            t.budget.refill(self.now_ns);
            if t.runnable && t.budget.cpu_remaining_ns() != 0 {
                self.current = Some(i);
                return Some(t.id);
            }
        }
        self.current = None;
        None
    }

    /// Returns the currently selected thread identifier.
    #[allow(dead_code)]
    pub fn current(&self) -> Option<u64> {
        self.current.map(|i| self.threads[i].id)
    }
}

/// Accounts one APIC quantum and advances the runnable queue.
pub fn timer_tick() {
    let _ = SCHEDULER.lock().tick(1_000_000);
}
