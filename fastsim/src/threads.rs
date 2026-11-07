//! Helper threads created ahead of their work.
//!
//! Creating a thread costs its creator 150-250 us inside gVisor (natively about 20 us), and a
//! scenario's helpers used to be created one after another on the thread that then runs the
//! simulation. A run that knows how many helpers it will use calls [`prestart`] once: the caller
//! creates a single launcher, which creates the others while the caller goes on with its setup.
//! [`spawn`] then hands each piece of work to a prestarted thread, or creates a thread when none
//! is left.

use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::JoinHandle;

type Job = Box<dyn FnOnce() + Send>;

#[derive(Default)]
struct Pool {
    state: Mutex<PoolState>,
    /// Signals a queued job to the prestarted threads.
    ready: Condvar,
}

#[derive(Default)]
struct PoolState {
    jobs: VecDeque<Job>,
    /// Prestarted threads (created or still being created) not yet promised a job.
    unclaimed: usize,
}

static POOL: OnceLock<Pool> = OnceLock::new();

/// Start `n` threads that wait for work from [`spawn`]. The caller creates one of them; that one
/// creates the rest. Each takes one job and ends with it.
pub fn prestart(n: usize) {
    if n == 0 {
        return;
    }
    let pool = POOL.get_or_init(Pool::default);
    pool.state.lock().expect("thread pool").unclaimed += n;
    std::thread::spawn(move || {
        for _ in 1..n {
            std::thread::spawn(run_one);
        }
        run_one();
    });
}

/// Helper threads that poll for work sleep for tens of microseconds at a time: give them a
/// timer slack of a microsecond instead of the default 50 (Linux; harmless elsewhere).
pub fn precise_timers() {
    #[cfg(target_os = "linux")]
    // SAFETY: a plain prctl setting this thread's timer slack.
    unsafe {
        libc::prctl(libc::PR_SET_TIMERSLACK, 1_000usize);
    }
}

/// A prestarted thread: wait for a job and run it.
fn run_one() {
    precise_timers();
    let pool = POOL.get().expect("thread pool");
    let job = {
        let mut st = pool.state.lock().expect("thread pool");
        loop {
            if let Some(job) = st.jobs.pop_front() {
                break job;
            }
            st = pool.ready.wait(st).expect("thread pool");
        }
    };
    job();
}

/// Run `f` on a thread of its own: a prestarted one if any is unclaimed, otherwise a new one.
pub fn spawn<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Handle<T> {
    if let Some(pool) = POOL.get() {
        let mut st = pool.state.lock().expect("thread pool");
        if st.unclaimed > 0 {
            st.unclaimed -= 1;
            let slot = Arc::new(Slot::default());
            let result = Arc::clone(&slot);
            st.jobs.push_back(Box::new(move || result.put(catch_unwind(AssertUnwindSafe(f)))));
            drop(st);
            pool.ready.notify_one();
            return Handle(Inner::Prestarted(slot));
        }
    }
    Handle(Inner::Thread(std::thread::spawn(move || {
        precise_timers();
        f()
    })))
}

/// The result of a [`spawn`]ed function.
pub struct Handle<T>(Inner<T>);

enum Inner<T> {
    Thread(JoinHandle<T>),
    Prestarted(Arc<Slot<T>>),
}

impl<T> Handle<T> {
    /// Wait for the function to return, as [`JoinHandle::join`] does; a panic comes back as the
    /// error. A prestarted thread may still be finishing its exit when this returns.
    pub fn join(self) -> std::thread::Result<T> {
        match self.0 {
            Inner::Thread(handle) => handle.join(),
            Inner::Prestarted(slot) => slot.take(),
        }
    }
}

struct Slot<T> {
    value: Mutex<Option<std::thread::Result<T>>>,
    done: Condvar,
}

impl<T> Default for Slot<T> {
    fn default() -> Self {
        Slot { value: Mutex::new(None), done: Condvar::new() }
    }
}

impl<T> Slot<T> {
    fn put(&self, value: std::thread::Result<T>) {
        *self.value.lock().expect("thread result") = Some(value);
        self.done.notify_all();
    }

    fn take(&self) -> std::thread::Result<T> {
        let mut value = self.value.lock().expect("thread result");
        loop {
            if let Some(v) = value.take() {
                return v;
            }
            value = self.done.wait(value).expect("thread result");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prestarted_and_new_threads_return_results_in_any_order() {
        // More jobs than prestarted threads: the rest get threads of their own.
        prestart(2);
        let handles: Vec<Handle<usize>> = (0..5).map(|i| spawn(move || i * 10)).collect();
        let results: Vec<usize> = handles.into_iter().rev().map(|h| h.join().unwrap()).collect();
        assert_eq!(results, vec![40, 30, 20, 10, 0]);
    }

    #[test]
    fn jobs_can_wait_for_each_other() {
        // A prestarted job blocks on a channel fed by a later job; neither may starve.
        prestart(2);
        let (tx, rx) = std::sync::mpsc::channel::<u32>();
        let consumer = spawn(move || rx.iter().sum::<u32>());
        let producer = spawn(move || {
            for i in 1..=100 {
                tx.send(i).unwrap();
            }
        });
        producer.join().unwrap();
        assert_eq!(consumer.join().unwrap(), 5050);
    }
}
