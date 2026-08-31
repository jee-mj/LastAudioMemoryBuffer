use crate::control::{CaptureState, DaemonLifecycleStatus, DaemonState, ErrorClass, RetryPolicy};
use crate::control_server::EnqueueError;
use crate::error::{LambError, Result};
use std::io;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub(crate) const RETRY_DELAYS: [Duration; 6] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(10),
    Duration::from_secs(30),
    Duration::from_secs(60),
];

pub(crate) fn retry_delay(attempt: u32) -> Duration {
    let index = attempt.saturating_sub(1) as usize;
    RETRY_DELAYS[index.min(RETRY_DELAYS.len() - 1)]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct RetryInstant(u64);

impl RetryInstant {
    pub(crate) fn from_millis(value: u64) -> Self {
        Self(value)
    }

    pub(crate) fn as_millis(self) -> u64 {
        self.0
    }

    pub(crate) fn checked_add(self, delay: Duration) -> Option<Self> {
        let millis = u64::try_from(delay.as_millis()).ok()?;
        self.0.checked_add(millis).map(Self)
    }
}

pub(crate) trait RetryClock: Send + Sync + 'static {
    fn now(&self) -> RetryInstant;
    fn unix_seconds(&self, instant: RetryInstant) -> u64;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct CaptureAttemptId(u64);

impl CaptureAttemptId {
    pub(crate) fn checked_after(current: u64) -> Option<Self> {
        current.checked_add(1).map(Self)
    }

    #[cfg(test)]
    pub(crate) fn from_raw_for_test(value: u64) -> Self {
        Self(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)] // Runtime capture producers are connected in Task 8.
pub(crate) enum RuntimeCaptureFault {
    DeviceDisconnected(String),
    BackendFault(String),
}

impl From<String> for RuntimeCaptureFault {
    fn from(message: String) -> Self {
        Self::BackendFault(message)
    }
}

impl From<&str> for RuntimeCaptureFault {
    fn from(message: &str) -> Self {
        Self::BackendFault(message.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ScheduledOperation {
    Retry {
        generation: u64,
    },
    #[allow(dead_code)] // Runtime capture producers are connected in Task 8.
    RuntimeFault {
        generation: u64,
        attempt_id: CaptureAttemptId,
        fault: RuntimeCaptureFault,
    },
}

impl ScheduledOperation {
    fn generation(&self) -> u64 {
        match self {
            Self::Retry { generation } | Self::RuntimeFault { generation, .. } => *generation,
        }
    }
}

#[derive(Clone)]
#[allow(dead_code)] // Runtime capture producers are connected in Task 8.
pub(crate) struct RuntimeFaultSink {
    generation: u64,
    attempt_id: CaptureAttemptId,
    notify: Arc<dyn Fn(u64, CaptureAttemptId, RuntimeCaptureFault) + Send + Sync>,
}

impl Default for RuntimeFaultSink {
    fn default() -> Self {
        Self::new(|_| {})
    }
}

#[allow(dead_code)] // Runtime capture producers are connected in Task 8.
impl RuntimeFaultSink {
    pub(crate) fn new<F>(notify: F) -> Self
    where
        F: Fn(RuntimeCaptureFault) + Send + Sync + 'static,
    {
        Self {
            generation: 0,
            attempt_id: CaptureAttemptId(0),
            notify: Arc::new(move |_, _, fault| notify(fault)),
        }
    }

    fn bound<F>(generation: u64, attempt_id: CaptureAttemptId, notify: F) -> Self
    where
        F: Fn(u64, CaptureAttemptId, RuntimeCaptureFault) + Send + Sync + 'static,
    {
        Self {
            generation,
            attempt_id,
            notify: Arc::new(notify),
        }
    }

    pub(crate) fn attempt_id(&self) -> CaptureAttemptId {
        self.attempt_id
    }

    pub(crate) fn notify(&self, fault: RuntimeCaptureFault) {
        (self.notify)(self.generation, self.attempt_id, fault);
    }

    pub(crate) fn same_binding(&self, other: &Self) -> bool {
        if self.generation != 0 || self.attempt_id.0 != 0 {
            self.generation == other.generation && self.attempt_id == other.attempt_id
        } else {
            Arc::ptr_eq(&self.notify, &other.notify)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)] // Immediate runtime faults are produced beginning in Task 8.
enum PendingOperation {
    Retry { generation: u64, due: RetryInstant },
    Immediate(ScheduledOperation),
}

impl PendingOperation {
    fn generation(&self) -> u64 {
        match self {
            Self::Retry { generation, .. } => *generation,
            Self::Immediate(operation) => operation.generation(),
        }
    }

    fn operation(&self) -> ScheduledOperation {
        match self {
            Self::Retry { generation, .. } => ScheduledOperation::Retry {
                generation: *generation,
            },
            Self::Immediate(operation) => operation.clone(),
        }
    }
}

#[derive(Debug, Default)]
struct SchedulerState {
    generation: u64,
    pending: Option<PendingOperation>,
    stopped: bool,
    wake_epoch: u64,
    thread_spawned: bool,
    #[cfg(test)]
    timed_wait_count: u64,
    #[cfg(test)]
    processed_wake_epoch: u64,
    #[cfg(test)]
    thread_start_count: u64,
}

#[derive(Clone)]
pub(crate) struct RetrySchedulerHandle {
    shared: Arc<(Mutex<SchedulerState>, Condvar)>,
}

impl RetrySchedulerHandle {
    pub(crate) fn new() -> Self {
        Self {
            shared: Arc::new((Mutex::new(SchedulerState::default()), Condvar::new())),
        }
    }

    pub(crate) fn schedule_retry(&self, generation: u64, due: RetryInstant) {
        let (state, changed) = &*self.shared;
        let mut state = state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.stopped || generation < state.generation {
            return;
        }
        state.generation = generation;
        state.pending = Some(PendingOperation::Retry { generation, due });
        state.wake_epoch = state.wake_epoch.wrapping_add(1);
        changed.notify_all();
    }

    #[allow(dead_code)] // Runtime capture producers are connected in Task 8.
    pub(crate) fn notify_fault(
        &self,
        generation: u64,
        attempt_id: CaptureAttemptId,
        fault: RuntimeCaptureFault,
    ) {
        let (state, changed) = &*self.shared;
        let mut state = state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.stopped || generation < state.generation {
            return;
        }
        state.generation = generation;
        state.pending = Some(PendingOperation::Immediate(
            ScheduledOperation::RuntimeFault {
                generation,
                attempt_id,
                fault,
            },
        ));
        state.wake_epoch = state.wake_epoch.wrapping_add(1);
        changed.notify_all();
    }

    #[allow(dead_code)] // Runtime capture producers are connected in Task 8.
    pub(crate) fn fault_sink(
        &self,
        generation: u64,
        attempt_id: CaptureAttemptId,
    ) -> RuntimeFaultSink {
        let scheduler = self.clone();
        RuntimeFaultSink::bound(
            generation,
            attempt_id,
            move |generation, attempt_id, fault| {
                scheduler.notify_fault(generation, attempt_id, fault)
            },
        )
    }

    pub(crate) fn invalidate(&self, generation: u64) {
        let (state, changed) = &*self.shared;
        let mut state = state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if generation < state.generation {
            return;
        }
        state.generation = generation;
        state.pending = None;
        state.wake_epoch = state.wake_epoch.wrapping_add(1);
        changed.notify_all();
    }

    pub(crate) fn notify_lane_available(&self) {
        let (state, changed) = &*self.shared;
        let mut state = state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.wake_epoch = state.wake_epoch.wrapping_add(1);
        changed.notify_all();
    }

    pub(crate) fn stop(&self) {
        let (state, changed) = &*self.shared;
        let mut state = state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.stopped = true;
        state.pending = None;
        state.wake_epoch = state.wake_epoch.wrapping_add(1);
        changed.notify_all();
    }

    pub(crate) fn is_stopped(&self) -> bool {
        self.shared
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .stopped
    }

    #[cfg(test)]
    pub(crate) fn wake_for_test(&self) {
        let (state, changed) = &*self.shared;
        let mut state = state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.wake_epoch = state.wake_epoch.wrapping_add(1);
        let requested = state.wake_epoch;
        changed.notify_all();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !state.stopped && state.processed_wake_epoch < requested {
            let remaining = deadline
                .checked_duration_since(std::time::Instant::now())
                .expect("timed out waiting for retry scheduler wake acknowledgment");
            let (next, timeout) = changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            assert!(
                !timeout.timed_out(),
                "timed out waiting for retry scheduler wake acknowledgment"
            );
            state = next;
        }
    }

    #[cfg(test)]
    pub(crate) fn timed_wait_count_for_test(&self) -> u64 {
        self.shared
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .timed_wait_count
    }

    #[cfg(test)]
    pub(crate) fn pending_operation_for_test(&self) -> Option<ScheduledOperation> {
        self.shared
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pending
            .as_ref()
            .map(PendingOperation::operation)
    }

    #[cfg(test)]
    pub(crate) fn thread_start_count_for_test(&self) -> u64 {
        self.shared
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .thread_start_count
    }
}

pub(crate) fn spawn_retry_scheduler<F>(
    handle: RetrySchedulerHandle,
    clock: Arc<dyn RetryClock>,
    submit: F,
) -> io::Result<JoinHandle<()>>
where
    F: Fn(ScheduledOperation) -> std::result::Result<(), EnqueueError> + Send + 'static,
{
    {
        let mut state = handle
            .shared
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.thread_spawned {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "retry scheduler already spawned",
            ));
        }
        state.thread_spawned = true;
    }
    let shared = Arc::clone(&handle.shared);
    let spawned = thread::Builder::new()
        .name("lamb-retry-scheduler".to_string())
        .spawn(move || {
            let (state, changed) = &*handle.shared;
            let mut state = state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            #[cfg(test)]
            {
                state.thread_start_count = state.thread_start_count.saturating_add(1);
                changed.notify_all();
            }
            loop {
                if state.stopped {
                    return;
                }
                let Some(pending) = state.pending.clone() else {
                    #[cfg(test)]
                    {
                        state.processed_wake_epoch = state.wake_epoch;
                        changed.notify_all();
                    }
                    state = changed
                        .wait(state)
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    continue;
                };
                if pending.generation() != state.generation {
                    state.pending = None;
                    continue;
                }
                if let PendingOperation::Retry { due, .. } = pending {
                    let now = clock.now();
                    if now < due {
                        let wait = Duration::from_millis(due.as_millis() - now.as_millis());
                        #[cfg(test)]
                        {
                            state.timed_wait_count = state.timed_wait_count.saturating_add(1);
                            state.processed_wake_epoch = state.wake_epoch;
                            changed.notify_all();
                        }
                        let (next, _) = changed
                            .wait_timeout(state, wait)
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        state = next;
                        continue;
                    }
                }
                let operation = pending.operation();
                let wake_epoch = state.wake_epoch;
                let result = submit(operation);
                match result {
                    Ok(()) | Err(EnqueueError::Closed) => state.pending = None,
                    Err(EnqueueError::Full) if state.wake_epoch == wake_epoch => {
                        #[cfg(test)]
                        {
                            state.processed_wake_epoch = state.wake_epoch;
                            changed.notify_all();
                        }
                        state = changed
                            .wait(state)
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                    }
                    Err(EnqueueError::Full) => {}
                }
            }
        });
    if spawned.is_err() {
        shared
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .thread_spawned = false;
    }
    spawned
}

pub(crate) struct SystemRetryClock {
    monotonic_origin: Instant,
    unix_origin: SystemTime,
}

impl Default for SystemRetryClock {
    fn default() -> Self {
        Self {
            monotonic_origin: Instant::now(),
            unix_origin: SystemTime::now(),
        }
    }
}

impl RetryClock for SystemRetryClock {
    fn now(&self) -> RetryInstant {
        let millis = self.monotonic_origin.elapsed().as_millis();
        RetryInstant::from_millis(u64::try_from(millis).unwrap_or(u64::MAX))
    }

    fn unix_seconds(&self, instant: RetryInstant) -> u64 {
        self.unix_origin
            .checked_add(Duration::from_millis(instant.as_millis()))
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs())
            .unwrap_or(u64::MAX)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LifecycleState {
    pub(crate) daemon_state: DaemonState,
    pub(crate) capture_state: CaptureState,
    pub(crate) error_class: Option<ErrorClass>,
    pub(crate) last_error: Option<String>,
    pub(crate) retry_policy: RetryPolicy,
    pub(crate) retry_attempt: u32,
    pub(crate) next_retry_at: Option<RetryInstant>,
    pub(crate) active_profile: Option<String>,
    pub(crate) resolved_target: Option<String>,
    pub(crate) generation: u64,
}

impl LifecycleState {
    pub(crate) fn ready_stopped(active_profile: Option<String>) -> Self {
        Self {
            daemon_state: DaemonState::Ready,
            capture_state: CaptureState::Stopped,
            error_class: None,
            last_error: None,
            retry_policy: RetryPolicy::None,
            retry_attempt: 0,
            next_retry_at: None,
            active_profile,
            resolved_target: None,
            generation: 0,
        }
    }

    pub(crate) fn mark_starting(&mut self, active_profile: Option<String>) {
        self.daemon_state = DaemonState::Ready;
        self.capture_state = CaptureState::Starting;
        self.clear_error_and_retry();
        self.active_profile = active_profile;
        self.resolved_target = None;
    }

    pub(crate) fn mark_permanent(&mut self, error: String) {
        self.daemon_state = DaemonState::Degraded;
        self.capture_state = CaptureState::Faulted;
        self.error_class = Some(ErrorClass::Permanent);
        self.last_error = Some(error);
        self.retry_policy = RetryPolicy::Manual;
        self.retry_attempt = 0;
        self.next_retry_at = None;
    }

    pub(crate) fn mark_transient(
        &mut self,
        capture_state: CaptureState,
        error: String,
        now: RetryInstant,
    ) {
        self.daemon_state = DaemonState::Degraded;
        self.capture_state = capture_state;
        self.error_class = Some(ErrorClass::Transient);
        self.last_error = Some(error);
        self.retry_policy = RetryPolicy::BoundedBackoff;
        self.retry_attempt = self.retry_attempt.saturating_add(1);
        self.next_retry_at = Some(
            now.checked_add(retry_delay(self.retry_attempt))
                .unwrap_or_else(|| RetryInstant::from_millis(u64::MAX)),
        );
    }

    pub(crate) fn mark_running(
        &mut self,
        active_profile: Option<String>,
        resolved_target: Option<String>,
    ) {
        self.daemon_state = DaemonState::Ready;
        self.capture_state = CaptureState::Running;
        self.clear_error_and_retry();
        self.active_profile = active_profile;
        self.resolved_target = resolved_target;
    }

    pub(crate) fn mark_stopped(&mut self, active_profile: Option<String>) {
        self.daemon_state = DaemonState::Ready;
        self.capture_state = CaptureState::Stopped;
        self.clear_error_and_retry();
        self.active_profile = active_profile;
        self.resolved_target = None;
    }

    pub(crate) fn mark_stopping(&mut self) {
        self.daemon_state = DaemonState::Stopping;
        self.clear_error_and_retry();
    }

    pub(crate) fn status(&self, clock: &dyn RetryClock) -> DaemonLifecycleStatus {
        DaemonLifecycleStatus {
            daemon_state: self.daemon_state,
            capture_state: self.capture_state,
            error_class: self.error_class,
            last_error: self.last_error.clone(),
            retry_policy: self.retry_policy,
            retry_attempt: self.retry_attempt,
            next_retry_at: self.next_retry_at.map(|at| clock.unix_seconds(at)),
            active_profile: self.active_profile.clone(),
            resolved_target: self.resolved_target.clone(),
        }
    }

    pub(crate) fn begin_operation(&mut self) -> Result<u64> {
        if self.daemon_state == DaemonState::Stopping {
            return Err(LambError::Control("daemon is stopping".to_string()));
        }
        let next = self
            .generation
            .checked_add(1)
            .ok_or(LambError::ControlInvariant("operation generation overflow"))?;
        self.generation = next;
        Ok(next)
    }

    fn clear_error_and_retry(&mut self) {
        self.error_class = None;
        self.last_error = None;
        self.retry_policy = RetryPolicy::None;
        self.retry_attempt = 0;
        self.next_retry_at = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_fault_job_carries_generation_and_capture_attempt_identity() {
        let scheduler = RetrySchedulerHandle::new();
        let attempt_id = CaptureAttemptId::from_raw_for_test(9);

        scheduler
            .fault_sink(4, attempt_id)
            .notify(RuntimeCaptureFault::BackendFault("failed".to_string()));

        assert_eq!(
            scheduler.pending_operation_for_test(),
            Some(ScheduledOperation::RuntimeFault {
                generation: 4,
                attempt_id,
                fault: RuntimeCaptureFault::BackendFault("failed".to_string()),
            })
        );
    }
    use crate::control::{CaptureState, DaemonState, ErrorClass, RetryPolicy};
    use crate::control_server::EnqueueError;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Condvar, Mutex};
    use std::thread::JoinHandle;
    use std::time::Duration;

    #[derive(Default)]
    struct ManualClock {
        now_millis: AtomicU64,
        unix_origin_seconds: u64,
    }

    impl RetryClock for ManualClock {
        fn now(&self) -> RetryInstant {
            RetryInstant::from_millis(self.now_millis.load(Ordering::SeqCst))
        }

        fn unix_seconds(&self, instant: RetryInstant) -> u64 {
            self.unix_origin_seconds + instant.as_millis() / 1_000
        }
    }

    struct SchedulerHarness {
        clock: Arc<ManualClock>,
        submitted: Arc<(Mutex<Vec<ScheduledOperation>>, Condvar)>,
        lane_full: Arc<AtomicBool>,
        lifecycle: Arc<Mutex<LifecycleState>>,
        handle: RetrySchedulerHandle,
        worker: Option<JoinHandle<()>>,
    }

    impl SchedulerHarness {
        fn new() -> Self {
            Self::build(false)
        }

        fn with_full_lane() -> Self {
            Self::build(true)
        }

        fn build(full: bool) -> Self {
            let clock = Arc::new(ManualClock::default());
            let submitted = Arc::new((Mutex::new(Vec::new()), Condvar::new()));
            let lane_full = Arc::new(AtomicBool::new(full));
            let lifecycle = Arc::new(Mutex::new(LifecycleState::ready_stopped(None)));
            let handle = RetrySchedulerHandle::new();
            let worker = spawn_retry_scheduler(handle.clone(), clock.clone(), {
                let submitted = Arc::clone(&submitted);
                let lane_full = Arc::clone(&lane_full);
                move |operation| {
                    if lane_full.load(Ordering::SeqCst) {
                        return Err(EnqueueError::Full);
                    }
                    let (jobs, changed) = &*submitted;
                    jobs.lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .push(operation);
                    changed.notify_all();
                    Ok(())
                }
            })
            .unwrap();
            Self {
                clock,
                submitted,
                lane_full,
                lifecycle,
                handle,
                worker: Some(worker),
            }
        }

        fn advance(&self, duration: Duration) {
            self.clock.now_millis.fetch_add(
                u64::try_from(duration.as_millis()).unwrap(),
                Ordering::SeqCst,
            );
            self.handle.wake_for_test();
        }

        fn schedule_retry(&self, generation: u64, due: RetryInstant) {
            {
                let mut lifecycle = self
                    .lifecycle
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                lifecycle.generation = generation;
                lifecycle.retry_attempt = 1;
                lifecycle.next_retry_at = Some(due);
            }
            self.handle.schedule_retry(generation, due);
        }

        fn invalidate(&self, generation: u64) {
            self.lifecycle
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .generation = generation;
            self.handle.invalidate(generation);
        }

        fn release_lane(&self) {
            self.lane_full.store(false, Ordering::SeqCst);
            self.handle.wake_for_test();
        }

        fn submitted_jobs(&self) -> Vec<ScheduledOperation> {
            self.submitted
                .0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        }

        fn take_job(&self) -> ScheduledOperation {
            let (jobs, changed) = &*self.submitted;
            let mut jobs = jobs.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            while jobs.is_empty() {
                jobs = changed
                    .wait(jobs)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
            jobs.remove(0)
        }

        fn lifecycle(&self) -> LifecycleState {
            self.lifecycle
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        }

        fn timed_wait_count(&self) -> u64 {
            self.handle.timed_wait_count_for_test()
        }
    }

    impl Drop for SchedulerHarness {
        fn drop(&mut self) {
            self.handle.stop();
            if let Some(worker) = self.worker.take() {
                worker.join().unwrap();
            }
        }
    }

    #[test]
    fn healthy_scheduler_blocks_without_timer_wakeups() {
        let harness = SchedulerHarness::new();
        harness.advance(Duration::from_secs(600));
        assert_eq!(harness.submitted_jobs(), Vec::new());
        assert_eq!(harness.timed_wait_count(), 0);
    }

    #[test]
    fn retry_is_submitted_at_each_capped_deadline() {
        let harness = SchedulerHarness::new();
        harness.schedule_retry(7, RetryInstant::from_millis(1_000));
        harness.advance(Duration::from_millis(999));
        assert_eq!(harness.submitted_jobs(), Vec::new());
        harness.advance(Duration::from_millis(1));
        assert_eq!(
            harness.submitted_jobs(),
            vec![ScheduledOperation::Retry { generation: 7 }]
        );
    }

    #[test]
    fn invalidated_generation_discards_waking_retry() {
        let harness = SchedulerHarness::new();
        harness.schedule_retry(3, RetryInstant::from_millis(1_000));
        harness.invalidate(4);
        harness.advance(Duration::from_secs(2));
        assert!(harness.submitted_jobs().is_empty());
    }

    #[test]
    fn current_generation_work_is_accepted_after_invalidation_and_lower_work_is_rejected() {
        let harness = SchedulerHarness::new();
        harness.invalidate(4);
        harness.schedule_retry(3, RetryInstant::from_millis(1_000));
        harness.schedule_retry(4, RetryInstant::from_millis(2_000));
        harness.advance(Duration::from_secs(1));
        assert!(harness.submitted_jobs().is_empty());
        harness.advance(Duration::from_secs(1));
        assert_eq!(
            harness.take_job(),
            ScheduledOperation::Retry { generation: 4 }
        );
    }

    #[test]
    fn duplicate_pending_work_is_replaced_without_duplicate_admission() {
        let harness = SchedulerHarness::new();
        harness.schedule_retry(5, RetryInstant::from_millis(1_000));
        harness.schedule_retry(5, RetryInstant::from_millis(2_000));
        harness.advance(Duration::from_secs(1));
        assert!(harness.submitted_jobs().is_empty());
        harness.advance(Duration::from_secs(1));
        assert_eq!(
            harness.submitted_jobs(),
            vec![ScheduledOperation::Retry { generation: 5 }]
        );
        harness.advance(Duration::from_secs(60));
        assert_eq!(harness.submitted_jobs().len(), 1);
    }

    #[test]
    fn poisoned_scheduler_mutex_recovers_and_processes_current_work() {
        let handle = RetrySchedulerHandle::new();
        let shared = Arc::clone(&handle.shared);
        let _ = std::panic::catch_unwind(move || {
            let _state = shared.0.lock().unwrap();
            panic!("poison scheduler state");
        });
        let submitted = Arc::new((Mutex::new(Vec::new()), Condvar::new()));
        let clock = Arc::new(ManualClock::default());
        let worker = spawn_retry_scheduler(handle.clone(), clock, {
            let submitted = Arc::clone(&submitted);
            move |operation| {
                submitted.0.lock().unwrap().push(operation);
                submitted.1.notify_all();
                Ok(())
            }
        })
        .unwrap();
        handle.schedule_retry(1, RetryInstant::from_millis(0));
        let mut jobs = submitted.0.lock().unwrap();
        while jobs.is_empty() {
            jobs = submitted.1.wait(jobs).unwrap();
        }
        assert_eq!(
            jobs.as_slice(),
            &[ScheduledOperation::Retry { generation: 1 }]
        );
        drop(jobs);
        handle.stop();
        worker.join().unwrap();
    }

    #[test]
    fn a_scheduler_handle_allows_exactly_one_worker_thread() {
        let handle = RetrySchedulerHandle::new();
        let clock = Arc::new(ManualClock::default());
        let worker = spawn_retry_scheduler(handle.clone(), clock.clone(), |_| Ok(())).unwrap();

        let duplicate = spawn_retry_scheduler(handle.clone(), clock, |_| Ok(()));

        assert_eq!(
            duplicate.unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists
        );
        handle.stop();
        worker.join().unwrap();
        assert_eq!(handle.thread_start_count_for_test(), 1);
    }

    #[test]
    fn full_lane_retains_job_without_advancing_attempt() {
        let harness = SchedulerHarness::with_full_lane();
        harness.schedule_retry(2, RetryInstant::from_millis(1_000));
        harness.advance(Duration::from_secs(1));
        assert_eq!(harness.lifecycle().retry_attempt, 1);
        harness.release_lane();
        assert_eq!(
            harness.take_job(),
            ScheduledOperation::Retry { generation: 2 }
        );
        assert_eq!(harness.lifecycle().retry_attempt, 1);
    }

    #[test]
    fn backoff_sequence_caps_at_sixty_seconds() {
        let actual = (1..=8).map(retry_delay).collect::<Vec<_>>();
        assert_eq!(
            actual,
            vec![
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(5),
                Duration::from_secs(10),
                Duration::from_secs(30),
                Duration::from_secs(60),
                Duration::from_secs(60),
                Duration::from_secs(60),
            ]
        );
    }

    #[test]
    fn transient_attempt_and_clock_overflow_saturate_with_a_consistent_deadline() {
        let mut state = LifecycleState::ready_stopped(None);
        state.retry_attempt = u32::MAX;

        state.mark_transient(
            CaptureState::WaitingForDevice,
            "still unavailable".to_string(),
            RetryInstant::from_millis(u64::MAX - 1),
        );

        assert_eq!(state.retry_attempt, u32::MAX);
        assert_eq!(state.retry_policy, RetryPolicy::BoundedBackoff);
        assert_eq!(
            state.next_retry_at,
            Some(RetryInstant::from_millis(u64::MAX))
        );
    }

    #[test]
    fn permanent_fault_has_manual_policy_without_retry() {
        let mut state = LifecycleState::ready_stopped(None);
        state.mark_permanent("channels conflicts with capturePorts".to_string());

        assert_eq!(state.daemon_state, DaemonState::Degraded);
        assert_eq!(state.capture_state, CaptureState::Faulted);
        assert_eq!(state.error_class, Some(ErrorClass::Permanent));
        assert_eq!(state.retry_policy, RetryPolicy::Manual);
        assert_eq!(state.retry_attempt, 0);
        assert_eq!(state.next_retry_at, None);
    }

    #[test]
    fn successful_capture_resets_transient_retry_state() {
        let mut state = LifecycleState::ready_stopped(None);
        state.mark_transient(
            CaptureState::WaitingForDevice,
            "target missing".to_string(),
            RetryInstant::from_millis(10_000),
        );
        state.mark_running(None, Some("scarlett".to_string()));

        assert_eq!(state.daemon_state, DaemonState::Ready);
        assert_eq!(state.capture_state, CaptureState::Running);
        assert_eq!(state.error_class, None);
        assert_eq!(state.retry_policy, RetryPolicy::None);
        assert_eq!(state.retry_attempt, 0);
        assert_eq!(state.next_retry_at, None);
    }

    #[test]
    fn transient_fault_uses_bounded_backoff_and_increments_attempts() {
        let mut state = LifecycleState::ready_stopped(Some("studio".to_string()));
        state.mark_transient(
            CaptureState::WaitingForDevice,
            "target missing".to_string(),
            RetryInstant::from_millis(10_000),
        );

        assert_eq!(state.daemon_state, DaemonState::Degraded);
        assert_eq!(state.capture_state, CaptureState::WaitingForDevice);
        assert_eq!(state.error_class, Some(ErrorClass::Transient));
        assert_eq!(state.retry_policy, RetryPolicy::BoundedBackoff);
        assert_eq!(state.retry_attempt, 1);
        assert_eq!(state.next_retry_at, Some(RetryInstant::from_millis(11_000)));
    }

    #[test]
    fn starting_stopped_and_stopping_transitions_clear_retry_state() {
        let mut state = LifecycleState::ready_stopped(None);
        state.mark_transient(
            CaptureState::WaitingForDevice,
            "target missing".to_string(),
            RetryInstant::from_millis(10_000),
        );
        state.mark_starting(Some("studio".to_string()));
        assert_eq!(state.capture_state, CaptureState::Starting);
        assert_eq!(state.active_profile.as_deref(), Some("studio"));
        assert_eq!(state.error_class, None);
        assert_eq!(state.next_retry_at, None);

        state.mark_stopped(Some("field".to_string()));
        assert_eq!(state.daemon_state, DaemonState::Ready);
        assert_eq!(state.capture_state, CaptureState::Stopped);
        assert_eq!(state.active_profile.as_deref(), Some("field"));

        state.mark_stopping();
        assert_eq!(state.daemon_state, DaemonState::Stopping);
        assert_eq!(state.retry_policy, RetryPolicy::None);
        assert_eq!(state.next_retry_at, None);
    }

    struct FixedClock;

    impl RetryClock for FixedClock {
        fn now(&self) -> RetryInstant {
            RetryInstant::from_millis(0)
        }

        fn unix_seconds(&self, instant: RetryInstant) -> u64 {
            1_700_000_000 + instant.as_millis() / 1_000
        }
    }

    #[test]
    fn status_converts_retry_instant_through_the_clock() {
        let mut state = LifecycleState::ready_stopped(Some("studio".to_string()));
        state.mark_transient(
            CaptureState::WaitingForDevice,
            "target missing".to_string(),
            RetryInstant::from_millis(10_000),
        );

        let status = state.status(&FixedClock);
        assert_eq!(status.next_retry_at, Some(1_700_000_011));
        assert_eq!(status.last_error.as_deref(), Some("target missing"));
        assert_eq!(status.active_profile.as_deref(), Some("studio"));
    }

    #[test]
    fn operation_generation_overflow_is_a_control_invariant() {
        let mut state = LifecycleState::ready_stopped(None);
        assert_eq!(state.begin_operation().unwrap(), 1);
        state.generation = u64::MAX;

        let error = state.begin_operation().unwrap_err();
        assert!(matches!(
            error,
            LambError::ControlInvariant("operation generation overflow")
        ));
    }

    #[test]
    fn stopping_rejects_new_published_operation_generation() {
        let mut state = LifecycleState::ready_stopped(None);
        state.mark_stopping();

        assert!(state.begin_operation().is_err());
        assert_eq!(state.generation, 0);
    }
}
