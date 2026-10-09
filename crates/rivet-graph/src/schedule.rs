//! Parallel task scheduler (D14).
//!
//! A small FIFO work queue over `std::thread::scope`: workers pull [`Task`]s,
//! each of which runs its command with output captured (parallel tasks must
//! never share inherited stdio — output would interleave). A poison flag
//! implements fail-fast: after the first failure, workers stop picking up
//! new tasks (running tasks finish), and the caller replays failures
//! deterministically (sorted by name, raw bytes preserved).
//!
//! Compile tasks of one package are independent; archive/link steps run after
//! the queue drains. Packages build serially in topological order.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// A unit of parallel work.
pub struct Task {
    /// Stable name for deterministic replay (e.g. a relative source path).
    pub name: String,
    /// The work. Must capture stdout/stderr rather than inheriting stdio.
    pub run: Box<dyn FnOnce() -> TaskOutcome + Send>,
}

/// The captured result of one [`Task`].
pub struct TaskOutcome {
    /// Task name (echoed for replay).
    pub name: String,
    /// Human-readable command for `-v` (`Running …`).
    pub display: String,
    /// `Ok` on success.
    pub result: Result<TaskSuccess, String>,
    /// Raw captured stdout (replayed verbatim on failure).
    pub stdout: Vec<u8>,
    /// Raw captured stderr (replayed verbatim on failure).
    pub stderr: Vec<u8>,
}

/// Success payload of a [`Task`].
#[derive(Debug, Default)]
pub struct TaskSuccess {
    /// Note lines for `-v` (e.g. how many headers were tracked).
    pub notes: Vec<String>,
}

/// Resolve the worker count: explicit `jobs` wins, `0` means auto
/// (`available_parallelism`, fallback 4), always at least 1.
pub fn resolve_jobs(jobs: usize) -> usize {
    if jobs > 0 {
        return jobs;
    }
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .max(1)
}

/// Shared FIFO task queue with fail-fast poisoning.
pub struct Queue {
    inner: Mutex<VecDeque<Task>>,
    poisoned: AtomicBool,
    fail_fast: bool,
}

impl Queue {
    /// New queue holding `tasks`.
    pub fn new(tasks: Vec<Task>, fail_fast: bool) -> Self {
        Self {
            inner: Mutex::new(tasks.into()),
            poisoned: AtomicBool::new(false),
            fail_fast,
        }
    }

    /// Poison the queue: workers stop taking new tasks.
    pub fn poison(&self) {
        self.poisoned.store(true, Ordering::SeqCst);
    }

    /// Take the next task, or `None` when empty (or poisoned + fail-fast).
    pub fn next(&self) -> Option<Task> {
        if self.fail_fast && self.poisoned.load(Ordering::SeqCst) {
            return None;
        }
        self.inner.lock().unwrap().pop_front()
    }
}

/// Run `tasks` on up to `jobs` workers, returning one outcome per task
/// sorted by name. With `fail_fast`, tasks still queued when the first
/// failure lands are skipped; already-running tasks finish.
pub fn run(tasks: Vec<Task>, jobs: usize, fail_fast: bool) -> Vec<TaskOutcome> {
    if tasks.is_empty() {
        return Vec::new();
    }
    let workers = resolve_jobs(jobs).min(tasks.len()).max(1);
    let queue = Queue::new(tasks, fail_fast);
    let out = Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                while let Some(task) = queue.next() {
                    let outcome = (task.run)();
                    if outcome.result.is_err() {
                        queue.poison();
                    }
                    out.lock().unwrap().push(outcome);
                }
            });
        }
    });
    let mut outcomes = out.into_inner().unwrap();
    outcomes.sort_by(|a, b| a.name.cmp(&b.name));
    outcomes
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn ok_task(name: &str) -> Task {
        let name = name.to_string();
        Task {
            name: name.clone(),
            run: Box::new(|| TaskOutcome {
                name,
                display: "echo".to_string(),
                result: Ok(TaskSuccess::default()),
                stdout: Vec::new(),
                stderr: Vec::new(),
            }),
        }
    }

    #[test]
    fn all_tasks_run_in_parallel() {
        use std::sync::atomic::AtomicUsize;
        let counter = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for i in 0..16 {
            let c = counter.clone();
            tasks.push(Task {
                name: format!("t{i:02}"),
                run: Box::new(move || {
                    c.fetch_add(1, Ordering::SeqCst);
                    TaskOutcome {
                        name: format!("t{i:02}"),
                        display: "x".to_string(),
                        result: Ok(TaskSuccess::default()),
                        stdout: Vec::new(),
                        stderr: Vec::new(),
                    }
                }),
            });
        }
        let outcomes = run(tasks, 4, true);
        assert_eq!(outcomes.len(), 16);
        assert_eq!(counter.load(Ordering::SeqCst), 16);
        // Sorted replay order.
        assert_eq!(outcomes[0].name, "t00");
        assert_eq!(outcomes[15].name, "t15");
    }

    #[test]
    fn failures_propagate() {
        let mut tasks = vec![ok_task("a")];
        tasks.push(Task {
            name: "b".to_string(),
            run: Box::new(|| TaskOutcome {
                name: "b".to_string(),
                display: "fail-cmd".to_string(),
                result: Err("boom".to_string()),
                stdout: b"out".to_vec(),
                stderr: b"err".to_vec(),
            }),
        });
        let outcomes = run(tasks, 2, true);
        assert_eq!(outcomes.len(), 2);
        assert!(outcomes[1].result.is_err());
        assert_eq!(outcomes[1].stdout, b"out");
    }

    #[test]
    fn fail_fast_skips_queued_tasks() {
        // One worker, first task fails: the rest must be skipped.
        let mut tasks = vec![Task {
            name: "a".to_string(),
            run: Box::new(|| TaskOutcome {
                name: "a".to_string(),
                display: "x".to_string(),
                result: Err("boom".to_string()),
                stdout: Vec::new(),
                stderr: Vec::new(),
            }),
        }];
        for n in ["b", "c", "d"] {
            tasks.push(ok_task(n));
        }
        let outcomes = run(tasks, 1, true);
        assert_eq!(outcomes.len(), 1, "got: {}", outcomes.len());
        assert_eq!(outcomes[0].name, "a");
    }

    #[test]
    fn keep_going_runs_everything() {
        let mut tasks = vec![Task {
            name: "a".to_string(),
            run: Box::new(|| TaskOutcome {
                name: "a".to_string(),
                display: "x".to_string(),
                result: Err("boom".to_string()),
                stdout: Vec::new(),
                stderr: Vec::new(),
            }),
        }];
        for n in ["b", "c", "d"] {
            tasks.push(ok_task(n));
        }
        let outcomes = run(tasks, 1, false);
        assert_eq!(outcomes.len(), 4);
    }

    #[test]
    fn resolve_jobs_defaults() {
        assert_eq!(resolve_jobs(3), 3);
        assert!(resolve_jobs(0) >= 1);
    }
}
