//! Budget-aware, timer-driven round-robin scheduling.

use alloc::vec::Vec;

use crate::sync::SpinLock;

static SCHEDULER: SpinLock<Scheduler> = SpinLock::new(Scheduler::new());

/// A schedulable thread and its CPU budget.
#[derive(Debug)]
pub struct Thread {
    /// Stable identifier supplied by the object layer.
    pub id: u64,
    budget_ns: u64,
    period_ns: u64,
    remaining_ns: u64,
    period_start_ns: u64,
    runnable: bool,
}

impl Thread {
    /// Creates a runnable thread with a full budget.
    #[allow(dead_code)]
    pub fn new(id: u64, budget_ns: u64, period_ns: u64, now_ns: u64) -> Self {
        assert!(period_ns != 0 && budget_ns <= period_ns);
        Self {
            id,
            budget_ns,
            period_ns,
            remaining_ns: budget_ns,
            period_start_ns: now_ns,
            runnable: true,
        }
    }

    fn refill(&mut self, now_ns: u64) {
        if now_ns >= self.period_start_ns && now_ns - self.period_start_ns >= self.period_ns {
            self.period_start_ns = now_ns - (now_ns - self.period_start_ns) % self.period_ns;
            self.remaining_ns = self.budget_ns;
        }
    }

    fn charge(&mut self, elapsed_ns: u64, now_ns: u64) -> bool {
        self.refill(now_ns);
        if elapsed_ns > self.remaining_ns {
            self.remaining_ns = 0;
            false
        } else {
            self.remaining_ns -= elapsed_ns;
            true
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
            t.refill(self.now_ns);
            if t.runnable && t.remaining_ns != 0 {
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
