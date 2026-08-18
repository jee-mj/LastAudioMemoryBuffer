use crate::error::{LambError, Result};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

/// Why an operation lane rejected an enqueue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueError {
    /// The lane has reached its preallocated capacity.
    Full,
    /// The lane has been closed and is draining.
    Closed,
}

/// A bounded, preallocated single-consumer operation lane.
///
/// Mutating control requests are transferred here and processed serially by
/// exactly one prestarted worker, so the control accept/parser path stays
/// responsive while persistence runs and no per-request threads are spawned.
/// The realtime capture callback never touches this lane.
pub struct OperationLane<T> {
    state: Arc<LaneState<T>>,
}

struct LaneState<T> {
    queue: Mutex<VecDeque<T>>,
    not_empty: Condvar,
    capacity: usize,
    closed: AtomicBool,
}

impl<T> OperationLane<T> {
    pub fn new(capacity: usize) -> Result<Self> {
        if capacity == 0 {
            return Err(LambError::Control(
                "operation lane capacity must be greater than zero".to_string(),
            ));
        }
        Ok(Self {
            state: Arc::new(LaneState {
                queue: Mutex::new(VecDeque::with_capacity(capacity)),
                not_empty: Condvar::new(),
                capacity,
                closed: AtomicBool::new(false),
            }),
        })
    }

    pub fn capacity(&self) -> usize {
        self.state.capacity
    }

    pub fn is_closed(&self) -> bool {
        self.state.closed.load(Ordering::Acquire)
    }

    /// Enqueues a job if there is spare capacity and the lane is still open.
    /// The backing `VecDeque` never reallocates past `capacity`, so saturation
    /// is a deterministic busy result rather than unbounded growth. On failure
    /// the job is returned so the caller can report the busy/closed condition.
    pub fn try_enqueue(&self, job: T) -> std::result::Result<(), (EnqueueError, T)> {
        let mut queue = self
            .state
            .queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.state.closed.load(Ordering::Acquire) {
            return Err((EnqueueError::Closed, job));
        }
        if queue.len() >= self.state.capacity {
            return Err((EnqueueError::Full, job));
        }
        queue.push_back(job);
        self.state.not_empty.notify_one();
        Ok(())
    }

    /// Blocks until a job is available; returns `None` once the lane is closed
    /// and drained. A closed lane yields all queued jobs before returning
    /// `None`.
    pub fn pop(&self) -> Option<T> {
        let mut queue = self
            .state
            .queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            if let Some(job) = queue.pop_front() {
                return Some(job);
            }
            if self.state.closed.load(Ordering::Acquire) {
                return None;
            }
            queue = self
                .state
                .not_empty
                .wait(queue)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    /// Closes admission and wakes the worker so it drains and exits.
    pub fn close(&self) {
        self.state.closed.store(true, Ordering::Release);
        self.state.not_empty.notify_all();
    }
}

impl<T> Drop for OperationLane<T> {
    fn drop(&mut self) {
        self.close();
    }
}

/// Spawns the single operation worker with an explicit stack size, touching its
/// configured stack pages before entering the loop so startup latency is
/// committed rather than paid on the first operation. The operation worker is
/// not a realtime thread; it may block on filesystem persistence.
///
/// Once the lane is closed, jobs that were queued but not yet started are
/// handed to `cancel` (so callers can answer them with a shutting-down
/// response) rather than executed. The job currently running in `handler`
/// always finishes first.
pub fn spawn_operation_worker<T, F, C>(
    lane: Arc<OperationLane<T>>,
    stack_bytes: usize,
    handler: F,
    cancel: C,
) -> thread::JoinHandle<()>
where
    T: Send + 'static,
    F: Fn(T) + Send + 'static,
    C: Fn(T) + Send + 'static,
{
    thread::Builder::new()
        .name("lamb-operation-worker".to_string())
        .stack_size(stack_bytes.max(64 * 1024))
        .spawn(move || {
            touch_stack_pages();
            while let Some(job) = lane.pop() {
                if lane.is_closed() {
                    cancel(job);
                } else {
                    handler(job);
                }
            }
        })
        .expect("failed to spawn operation worker")
}

/// Volatile-touches one byte per OS page within a bounded stack-local buffer so
/// the worker's stack pages are materialized up front. The buffer is kept well
/// below the minimum configured worker stack.
#[inline(never)]
fn touch_stack_pages() {
    const TOUCH_BYTES: usize = 32 * 1024;
    let mut buffer = [0_u8; TOUCH_BYTES];
    let page = page_size();
    for chunk in buffer.chunks_mut(page) {
        let first = chunk.first_mut().expect("page chunk is non-empty");
        unsafe { std::ptr::write_volatile(first, 0) };
    }
}

fn page_size() -> usize {
    // Linux page size; fall back to 4 KiB if the syscall is unavailable.
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if size > 0 {
        size as usize
    } else {
        4096
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    #[test]
    fn lane_preserves_fifo_order_and_rejects_saturation_deterministically() {
        let lane = OperationLane::new(3).unwrap();
        assert_eq!(lane.capacity(), 3);
        lane.try_enqueue(1).unwrap();
        lane.try_enqueue(2).unwrap();
        lane.try_enqueue(3).unwrap();
        assert_eq!(lane.try_enqueue(4), Err((EnqueueError::Full, 4)));
        assert_eq!(lane.pop(), Some(1));
        assert_eq!(lane.pop(), Some(2));
        assert_eq!(lane.pop(), Some(3));
        lane.close();
        assert_eq!(lane.pop(), None);
    }

    #[test]
    fn closed_lane_drains_then_returns_none_and_rejects_new_jobs() {
        let lane = OperationLane::new(2).unwrap();
        lane.try_enqueue(10).unwrap();
        lane.try_enqueue(11).unwrap();
        lane.close();
        assert_eq!(lane.try_enqueue(12), Err((EnqueueError::Closed, 12)));
        assert_eq!(lane.pop(), Some(10));
        assert_eq!(lane.pop(), Some(11));
        assert_eq!(lane.pop(), None);
    }

    #[test]
    fn worker_processes_jobs_in_order_until_close_and_join() {
        let lane = Arc::new(OperationLane::new(4).unwrap());
        let processed = Arc::new(AtomicUsize::new(0));
        let processed_w = Arc::clone(&processed);
        let worker = spawn_operation_worker(
            Arc::clone(&lane),
            64 * 1024,
            move |job: usize| {
                processed_w.fetch_add(job, Ordering::SeqCst);
            },
            |_job: usize| {},
        );
        for job in [1, 2, 3, 4] {
            lane.try_enqueue(job).unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while processed.load(Ordering::SeqCst) != 10 && Instant::now() < deadline {
            thread::yield_now();
        }
        lane.close();
        worker.join().unwrap();
        assert_eq!(processed.load(Ordering::SeqCst), 10);
    }

    #[test]
    fn close_cancels_queued_jobs_after_the_active_one_finishes() {
        use std::sync::Barrier;
        let lane = Arc::new(OperationLane::new(4).unwrap());
        let processed = Arc::new(AtomicUsize::new(0));
        let cancelled = Arc::new(AtomicUsize::new(0));
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));

        let processed_w = Arc::clone(&processed);
        let cancelled_w = Arc::clone(&cancelled);
        let entered_w = Arc::clone(&entered);
        let release_w = Arc::clone(&release);
        let worker = spawn_operation_worker(
            Arc::clone(&lane),
            64 * 1024,
            move |job: usize| {
                if job == 1 {
                    entered_w.wait();
                    release_w.wait();
                }
                processed_w.fetch_add(job, Ordering::SeqCst);
            },
            move |job: usize| {
                cancelled_w.fetch_add(job, Ordering::SeqCst);
            },
        );

        lane.try_enqueue(1).unwrap();
        entered.wait();
        lane.try_enqueue(2).unwrap();
        lane.try_enqueue(3).unwrap();
        lane.close();
        release.wait();
        worker.join().unwrap();

        assert_eq!(processed.load(Ordering::SeqCst), 1);
        assert_eq!(cancelled.load(Ordering::SeqCst), 5);
    }

    #[test]
    fn zero_capacity_lane_is_rejected() {
        assert!(OperationLane::<()>::new(0).is_err());
    }
}
