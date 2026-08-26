use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use crate::activity::FrozenExportDecision;
use crate::capture_arena::{
    CaptureArena, CaptureClearAccounting, CaptureClearRecovery, FrozenCaptureEpoch,
};
use crate::error::{LambError, Result};
use crate::export_policy::{ExportCommand, ResolvedExportPolicy};
use crate::export_wav::{publish_prepared, PreparedPublication};
use crate::persistence_workspace::{
    IndeterminatePublication, PersistenceWorkspace, PrepareRequest, PreparedPersistence,
    PublicationRecovery,
};
use crate::sample_ring::{SampleRing, Snapshot};

static NEXT_DUMP_COORDINATOR_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameRange {
    pub start: u64,
    pub end: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedOutput {
    pub output_directory: PathBuf,
    pub files: Vec<PathBuf>,
}

pub use crate::persistence_workspace::CompletedOutput;

/// The committed prepared-persistence result delivered before workspace reuse.
pub enum CommittedPersistenceRef<'a> {
    Written {
        range: FrameRange,
        export_range: FrameRange,
        frames: u64,
        losses: LossBreakdown,
        output: CompletedOutput<'a>,
    },
    SkippedSilent {
        range: FrameRange,
        frames: u64,
        losses: LossBreakdown,
    },
    SkippedByPolicy {
        range: FrameRange,
        frames: u64,
        losses: LossBreakdown,
    },
    NoNewAudio {
        losses: LossBreakdown,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionPreparation {
    Continue,
    SkippedSilent,
}

enum CoordinatorPrepareRequest<'a> {
    Direct(PrepareRequest<'a>),
}

pub struct PolicyPersistenceRequest<'a> {
    pub command: ExportCommand,
    pub policy: &'a ResolvedExportPolicy,
    pub profile: &'a str,
    pub staging_root: &'a Path,
    pub timestamp: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DumpOutcome {
    Written {
        range: FrameRange,
        frames: u64,
        export_start_frame: u64,
        export_frames: u64,
        losses: LossBreakdown,
        output_directory: PathBuf,
        files: Vec<PathBuf>,
    },
    SkippedSilent {
        range: FrameRange,
        frames: u64,
        losses: LossBreakdown,
    },
    SkippedByPolicy {
        range: FrameRange,
        frames: u64,
        losses: LossBreakdown,
    },
    NoNewAudio {
        losses: LossBreakdown,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LossBreakdown {
    pub retention_lost_frames: u64,
    pub cleared_frames: u64,
    pub capture_dropped_frames: u64,
}

impl LossBreakdown {
    pub fn lost_frames(self) -> u64 {
        self.retention_lost_frames
            .saturating_add(self.cleared_frames)
            .saturating_add(self.capture_dropped_frames)
    }
}

impl DumpOutcome {
    pub fn losses(&self) -> LossBreakdown {
        match self {
            Self::Written { losses, .. }
            | Self::SkippedSilent { losses, .. }
            | Self::SkippedByPolicy { losses, .. }
            | Self::NoNewAudio { losses } => *losses,
        }
    }

    pub fn range(&self) -> Option<FrameRange> {
        match self {
            Self::Written { range, .. }
            | Self::SkippedSilent { range, .. }
            | Self::SkippedByPolicy { range, .. } => Some(*range),
            Self::NoNewAudio { .. } => None,
        }
    }
}

struct FrozenTransaction {
    frozen: FrozenCaptureEpoch,
    retention_lost_frames: u64,
    decision: FrozenExportDecision,
}

#[derive(Clone, Copy)]
struct PrevalidatedLossCommit {
    acknowledged_dropped_frames: u64,
    retention_lost_frames: u64,
    cleared_frames: u64,
}

#[derive(Clone, Copy)]
struct PendingClear {
    arena_runtime_id: u64,
    coordinator_id: u64,
    clear_id: u64,
    requested_start: u64,
    accounting: CaptureClearAccounting,
    frozen_end: Option<u64>,
    acknowledged_dropped_frames: u64,
}

enum PendingCompletionAuthority {
    /// The sole completion slot is reserved for the guard that currently owns
    /// the transaction while invoking the caller without the state lock.
    Delivering,
    /// Delivery ended and every remaining authority is represented in state.
    Finalized {
        reset_decision: FrozenExportDecision,
        failed_release: Option<FrozenCaptureEpoch>,
    },
}

#[derive(Default)]
struct DumpState {
    bound_runtime_id: Option<u64>,
    committed_until: Option<u64>,
    frozen: Option<FrozenTransaction>,
    reusable_decision: Option<FrozenExportDecision>,
    pending_clear: Option<PendingClear>,
    pending_release: Option<FrozenCaptureEpoch>,
    completion_in_progress: bool,
    pending_completion: Option<PendingCompletionAuthority>,
    indeterminate_publication: Option<IndeterminatePublication>,
    acknowledged_dropped_frames: u64,
    pending_retention_lost_frames: u64,
    pending_cleared_frames: u64,
    next_clear_id: u64,
}

pub struct DumpCoordinator {
    id: u64,
    state: Mutex<DumpState>,
}

enum CommittedPersistenceKind {
    Written {
        range: FrameRange,
        export_range: FrameRange,
        frames: u64,
        losses: LossBreakdown,
    },
    SkippedSilent {
        range: FrameRange,
        frames: u64,
        losses: LossBreakdown,
    },
    SkippedByPolicy {
        range: FrameRange,
        frames: u64,
        losses: LossBreakdown,
    },
}

/// Owns the post-commit workspace and frozen-epoch obligations while delivery
/// borrows the canonical workspace slots.  It is deliberately idempotent so a
/// delivery error and an unwind both release exactly once.
pub struct CompletionGuard<'a> {
    coordinator: &'a DumpCoordinator,
    arena: &'a CaptureArena,
    workspace: &'a mut PersistenceWorkspace,
    transaction: Option<FrozenTransaction>,
    release_timeout: Duration,
    kind: CommittedPersistenceKind,
    finalized: bool,
}

impl CompletionGuard<'_> {
    fn deliver<F>(&mut self, deliver: F) -> Result<()>
    where
        F: for<'view> FnOnce(CommittedPersistenceRef<'view>) -> Result<()>,
    {
        let outcome = match self.kind {
            CommittedPersistenceKind::Written {
                range,
                export_range,
                frames,
                losses,
            } => {
                let output_directory = self.workspace.completed_output_directory();
                let files = self.workspace.completed_file_plan();
                CommittedPersistenceRef::Written {
                    range,
                    export_range,
                    frames,
                    losses,
                    output: CompletedOutput {
                        output_directory,
                        files,
                    },
                }
            }
            CommittedPersistenceKind::SkippedSilent {
                range,
                frames,
                losses,
            } => CommittedPersistenceRef::SkippedSilent {
                range,
                frames,
                losses,
            },
            CommittedPersistenceKind::SkippedByPolicy {
                range,
                frames,
                losses,
            } => CommittedPersistenceRef::SkippedByPolicy {
                range,
                frames,
                losses,
            },
        };
        deliver(outcome)
    }

    fn finalize(&mut self) {
        if self.finalized {
            return;
        }
        self.finalized = true;
        self.workspace.finish_completed_publication();
        let (mut state, poisoned) = match self.coordinator.state.lock() {
            Ok(state) => (state, false),
            Err(error) => (error.into_inner(), true),
        };
        let reconciled = DumpCoordinator::finalize_completed_transaction(
            &mut state,
            self.arena,
            self.release_timeout,
            &mut self.transaction,
        )
        .is_ok();
        let coherent =
            reconciled && !state.completion_in_progress && state.pending_completion.is_none();
        drop(state);
        if poisoned && coherent {
            self.coordinator.state.clear_poison();
        }
    }
}

impl Drop for CompletionGuard<'_> {
    fn drop(&mut self) {
        self.finalize();
    }
}

impl DumpCoordinator {
    pub fn new() -> Self {
        let id = NEXT_DUMP_COORDINATOR_ID
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(1)
            })
            .expect("dump coordinator identity exhausted");
        Self {
            id,
            state: Mutex::new(DumpState::default()),
        }
    }

    /// Constructs a persistence-capable coordinator with one decision allocated at session startup.
    pub fn with_frozen_decision(decision: FrozenExportDecision) -> Self {
        let mut coordinator = Self::new();
        coordinator
            .state
            .get_mut()
            .expect("new dump coordinator state is not poisoned")
            .reusable_decision = Some(decision);
        coordinator
    }

    #[cfg(test)]
    pub(crate) fn pending_frozen_decision_for_test(
        &self,
    ) -> Result<Option<crate::activity::FrozenExportDecisionSnapshot>> {
        let state = self.lock_state()?;
        Ok(state
            .frozen
            .as_ref()
            .map(|transaction| transaction.decision.snapshot_for_test()))
    }

    pub fn dump<F>(&self, ring: &SampleRing, publisher: F) -> Result<DumpOutcome>
    where
        F: FnOnce(&SampleSnapshot) -> Result<PublishedOutput>,
    {
        let mut state = self.lock_state()?;
        if state.pending_clear.is_some() {
            return Err(LambError::ControlInvariant(
                "pending clear requires capture arena recovery",
            ));
        }
        if state.completion_in_progress || state.pending_completion.is_some() {
            return Err(LambError::ControlInvariant("completion delivery is active"));
        }
        let selected = ring.select_snapshot(state.committed_until)?;
        let lost_frames = selected
            .oldest_frame()
            .saturating_sub(selected.requested_start());
        let ring_snapshot = selected.into_snapshot();
        let range = FrameRange {
            start: ring_snapshot.start_frame(),
            end: ring_snapshot.end_frame(),
        };

        if range.start >= range.end {
            return Ok(DumpOutcome::NoNewAudio {
                losses: LossBreakdown::default(),
            });
        }

        let snapshot = SampleSnapshot::from_ring_snapshot(ring_snapshot)?;
        let frames = range.end - range.start;

        if snapshot.is_digital_silence() {
            state.committed_until = Some(range.end);
            return Ok(DumpOutcome::SkippedSilent {
                range,
                frames,
                losses: LossBreakdown {
                    retention_lost_frames: lost_frames,
                    ..LossBreakdown::default()
                },
            });
        }

        let published = publisher(&snapshot)?;
        state.committed_until = Some(range.end);
        Ok(DumpOutcome::Written {
            range,
            frames,
            export_start_frame: range.start,
            export_frames: frames,
            losses: LossBreakdown {
                retention_lost_frames: lost_frames,
                ..LossBreakdown::default()
            },
            output_directory: published.output_directory,
            files: published.files,
        })
    }

    pub fn persist(
        &self,
        arena: &CaptureArena,
        workspace: &mut PersistenceWorkspace,
        request: PrepareRequest<'_>,
        timeout: Duration,
    ) -> Result<DumpOutcome> {
        self.persist_with_publisher(
            arena,
            workspace,
            request,
            timeout,
            timeout,
            publish_prepared,
        )
    }

    /// Persists using the coordinator-owned frozen decision, so callers only
    /// provide command context and the stable session policy.
    pub fn persist_policy(
        &self,
        arena: &CaptureArena,
        workspace: &mut PersistenceWorkspace,
        request: PolicyPersistenceRequest<'_>,
        timeout: Duration,
    ) -> Result<DumpOutcome> {
        // Compatibility for the pre-Task-4 daemon path.  This callback-owned
        // conversion allocates; new prepared callers must use
        // `persist_policy_with_delivery` instead.
        let mut outcome = None;
        self.persist_policy_with_delivery(
            arena,
            workspace,
            request,
            timeout,
            timeout,
            |committed| {
                outcome = Some(match committed {
                    CommittedPersistenceRef::Written {
                        range,
                        export_range,
                        frames,
                        losses,
                        output,
                    } => DumpOutcome::Written {
                        range,
                        frames,
                        export_start_frame: export_range.start,
                        export_frames: export_range.end - export_range.start,
                        losses,
                        output_directory: output.output_directory.to_path_buf(),
                        files: output
                            .files
                            .iter()
                            .map(|file| file.final_path().to_path_buf())
                            .collect(),
                    },
                    CommittedPersistenceRef::SkippedSilent {
                        range,
                        frames,
                        losses,
                    } => DumpOutcome::SkippedSilent {
                        range,
                        frames,
                        losses,
                    },
                    CommittedPersistenceRef::SkippedByPolicy {
                        range,
                        frames,
                        losses,
                    } => DumpOutcome::SkippedByPolicy {
                        range,
                        frames,
                        losses,
                    },
                    CommittedPersistenceRef::NoNewAudio { losses } => {
                        DumpOutcome::NoNewAudio { losses }
                    }
                });
                Ok(())
            },
        )?;
        outcome.ok_or(LambError::ControlInvariant(
            "persistence delivery did not provide an outcome",
        ))
    }

    pub fn persist_policy_with_delivery<F>(
        &self,
        arena: &CaptureArena,
        workspace: &mut PersistenceWorkspace,
        request: PolicyPersistenceRequest<'_>,
        timeout: Duration,
        release_timeout: Duration,
        deliver: F,
    ) -> Result<()>
    where
        F: for<'view> FnOnce(CommittedPersistenceRef<'view>) -> Result<()>,
    {
        let mut state = self.lock_state()?;
        Self::bind_arena(&mut state, arena)?;
        Self::recover_pending_clear(&mut state, self.id, arena, timeout)?;
        Self::reconcile_completion_authority(&mut state, arena, timeout)?;
        self.clear_completion_poison_if_reconciled(&state);
        let recovery_accounting = if state.indeterminate_publication.is_some() {
            let transaction = state.frozen.as_ref().ok_or(LambError::ControlInvariant(
                "indeterminate publication has no frozen transaction",
            ))?;
            let cumulative = arena.cumulative_capture_dropped_frames();
            Some((
                cumulative,
                Self::prevalidate_loss_commit(
                    &state,
                    cumulative,
                    transaction.retention_lost_frames,
                )?,
            ))
        } else {
            None
        };
        if Self::recover_indeterminate_publication(&mut state, workspace)? {
            let transaction = state.frozen.as_ref().ok_or(LambError::ControlInvariant(
                "complete recovered publication has no frozen transaction",
            ))?;
            let range = FrameRange {
                start: transaction.frozen.absolute_range().start,
                end: transaction.frozen.absolute_range().end,
            };
            let (cumulative, loss_commit) = recovery_accounting
                .expect("complete recovery retains prevalidated loss accounting");
            let losses = Self::commit_losses(&mut state, cumulative, loss_commit);
            state.committed_until = Some(range.end);
            Self::begin_completion(&mut state)?;
            let Some(transaction) = state.frozen.take() else {
                state.completion_in_progress = false;
                return Err(LambError::ControlInvariant(
                    "recovered completion lost its frozen transaction",
                ));
            };
            let export_range = if transaction.decision.valid() {
                transaction.decision.export_range()
            } else {
                range.start..range.end
            };
            let mut completion = CompletionGuard {
                coordinator: self,
                arena,
                workspace,
                transaction: Some(transaction),
                release_timeout,
                kind: CommittedPersistenceKind::Written {
                    range,
                    export_range: FrameRange {
                        start: export_range.start,
                        end: export_range.end,
                    },
                    frames: range.end - range.start,
                    losses,
                },
                finalized: false,
            };
            drop(state);
            let result = completion.deliver(deliver);
            completion.finalize();
            return result;
        }
        if state.frozen.is_none() && state.reusable_decision.is_none() {
            return Err(LambError::ControlInvariant(
                "persistence coordinator has no reusable frozen decision",
            ));
        }
        if state.frozen.is_none() {
            let Some(frozen) = arena.freeze_since(state.committed_until, timeout)? else {
                let cumulative = arena.cumulative_capture_dropped_frames();
                let loss_commit = Self::prevalidate_loss_commit(&state, cumulative, 0)?;
                let losses = Self::commit_losses(&mut state, cumulative, loss_commit);
                drop(state);
                return deliver(CommittedPersistenceRef::NoNewAudio { losses });
            };
            let requested = state.committed_until.unwrap_or(0);
            state.frozen = Some(FrozenTransaction {
                retention_lost_frames: frozen.absolute_range().start.saturating_sub(requested),
                frozen,
                decision: state
                    .reusable_decision
                    .take()
                    .ok_or(LambError::ControlInvariant(
                        "persistence coordinator has no reusable frozen decision",
                    ))?,
            });
        }
        let (range, retention_lost_frames, export_range, publication, indeterminate) = {
            let transaction = state
                .frozen
                .as_mut()
                .expect("frozen transaction remains present");
            let range = FrameRange {
                start: transaction.frozen.absolute_range().start,
                end: transaction.frozen.absolute_range().end,
            };
            let retention_lost_frames = transaction.retention_lost_frames;
            let mut indeterminate = None;
            let prepared = workspace.prepare(
                &transaction.frozen,
                PrepareRequest::Policy {
                    command: request.command,
                    policy: request.policy,
                    profile: request.profile,
                    staging_root: request.staging_root,
                    timestamp: request.timestamp,
                    decision: &mut transaction.decision,
                },
            )?;
            let publication = match prepared {
                PreparedPersistence::Silent | PreparedPersistence::SkippedSilent => {
                    Ok((false, true))
                }
                PreparedPersistence::SkippedByPolicy => Ok((true, false)),
                prepared => match publish_prepared(prepared) {
                    PreparedPublication::Published => Ok((false, false)),
                    PreparedPublication::RetryableFailure(error) => Err(error),
                    PreparedPublication::Indeterminate { operation, cleanup } => {
                        indeterminate = Some((operation, cleanup));
                        Ok((false, false))
                    }
                },
            };
            let export_range = if transaction.decision.valid() {
                transaction.decision.export_range()
            } else {
                range.start..range.end
            };
            (
                range,
                retention_lost_frames,
                export_range,
                publication,
                indeterminate,
            )
        };
        if export_range.start < range.start
            || export_range.end != range.end
            || export_range.start > export_range.end
        {
            return Err(LambError::ExportInvariant(
                "frozen export range is outside the consumed range",
            ));
        }
        let (skipped_by_policy, skipped_silent) = publication?;
        if let Some((operation, cleanup)) = indeterminate {
            state.indeterminate_publication = Some(cleanup);
            return Err(LambError::IndeterminatePublication {
                operation: Box::new(operation),
            });
        }
        let cumulative = arena.cumulative_capture_dropped_frames();
        let loss_commit = Self::prevalidate_loss_commit(&state, cumulative, retention_lost_frames)?;
        let losses = Self::commit_losses(&mut state, cumulative, loss_commit);
        state.committed_until = Some(range.end);
        Self::begin_completion(&mut state)?;
        let Some(transaction) = state.frozen.take() else {
            state.completion_in_progress = false;
            return Err(LambError::ControlInvariant(
                "committed completion lost its frozen transaction",
            ));
        };
        let kind = if skipped_by_policy {
            CommittedPersistenceKind::SkippedByPolicy {
                range,
                frames: range.end - range.start,
                losses,
            }
        } else if skipped_silent {
            CommittedPersistenceKind::SkippedSilent {
                range,
                frames: range.end - range.start,
                losses,
            }
        } else {
            CommittedPersistenceKind::Written {
                range,
                export_range: FrameRange {
                    start: export_range.start,
                    end: export_range.end,
                },
                frames: range.end - range.start,
                losses,
            }
        };
        let mut completion = CompletionGuard {
            coordinator: self,
            arena,
            workspace,
            transaction: Some(transaction),
            release_timeout,
            kind,
            finalized: false,
        };
        drop(state);
        let result = completion.deliver(deliver);
        completion.finalize();
        result
    }

    pub fn persist_with_release_timeout(
        &self,
        arena: &CaptureArena,
        workspace: &mut PersistenceWorkspace,
        request: PrepareRequest<'_>,
        timeout: Duration,
        release_timeout: Duration,
    ) -> Result<DumpOutcome> {
        self.persist_with_publisher(
            arena,
            workspace,
            request,
            timeout,
            release_timeout,
            publish_prepared,
        )
    }

    pub fn persist_with_publisher<F>(
        &self,
        arena: &CaptureArena,
        workspace: &mut PersistenceWorkspace,
        request: PrepareRequest<'_>,
        timeout: Duration,
        release_timeout: Duration,
        publisher: F,
    ) -> Result<DumpOutcome>
    where
        F: FnOnce(PreparedPersistence<'_>) -> PreparedPublication,
    {
        self.persist_with_decision_preparation_and_publisher(
            arena,
            workspace,
            request,
            timeout,
            release_timeout,
            |_, _| Ok(DecisionPreparation::Continue),
            publisher,
        )
    }

    #[allow(clippy::too_many_arguments)] // preserves the existing publisher seam while adding preparation.
    pub fn persist_with_decision_preparation_and_publisher<P, F>(
        &self,
        arena: &CaptureArena,
        workspace: &mut PersistenceWorkspace,
        request: PrepareRequest<'_>,
        timeout: Duration,
        release_timeout: Duration,
        prepare_decision: P,
        publisher: F,
    ) -> Result<DumpOutcome>
    where
        P: FnOnce(&FrozenCaptureEpoch, &mut FrozenExportDecision) -> Result<DecisionPreparation>,
        F: FnOnce(PreparedPersistence<'_>) -> PreparedPublication,
    {
        self.persist_request_with_decision_preparation_and_publisher(
            arena,
            workspace,
            CoordinatorPrepareRequest::Direct(request),
            timeout,
            release_timeout,
            prepare_decision,
            publisher,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn persist_request_with_decision_preparation_and_publisher<P, F>(
        &self,
        arena: &CaptureArena,
        workspace: &mut PersistenceWorkspace,
        request: CoordinatorPrepareRequest<'_>,
        timeout: Duration,
        release_timeout: Duration,
        prepare_decision: P,
        publisher: F,
    ) -> Result<DumpOutcome>
    where
        P: FnOnce(&FrozenCaptureEpoch, &mut FrozenExportDecision) -> Result<DecisionPreparation>,
        F: FnOnce(PreparedPersistence<'_>) -> PreparedPublication,
    {
        let mut state = self.lock_state()?;
        Self::bind_arena(&mut state, arena)?;
        Self::recover_pending_clear(&mut state, self.id, arena, timeout)?;
        Self::reconcile_completion_authority(&mut state, arena, timeout)?;
        self.clear_completion_poison_if_reconciled(&state);
        let recovery_accounting = if state.indeterminate_publication.is_some() {
            let transaction = state.frozen.as_ref().ok_or(LambError::ControlInvariant(
                "indeterminate publication has no frozen transaction",
            ))?;
            let cumulative = arena.cumulative_capture_dropped_frames();
            let commit = Self::prevalidate_loss_commit(
                &state,
                cumulative,
                transaction.retention_lost_frames,
            )?;
            Some((cumulative, commit))
        } else {
            None
        };
        let recovered_publication = Self::recover_indeterminate_publication(&mut state, workspace)?;
        if recovered_publication {
            let (range, export_range) = {
                let transaction = state.frozen.as_ref().ok_or(LambError::ControlInvariant(
                    "complete recovered publication has no frozen transaction",
                ))?;
                let range = FrameRange {
                    start: transaction.frozen.absolute_range().start,
                    end: transaction.frozen.absolute_range().end,
                };
                let export_range = if transaction.decision.valid() {
                    transaction.decision.export_range()
                } else {
                    range.start..range.end
                };
                (range, export_range)
            };
            let (cumulative, loss_commit) = recovery_accounting
                .expect("complete publication recovery retains prevalidated loss accounting");
            let losses = Self::commit_losses(&mut state, cumulative, loss_commit);
            state.committed_until = Some(range.end);
            let files = workspace.completed_file_plan();
            let outcome = DumpOutcome::Written {
                range,
                frames: range.end - range.start,
                export_start_frame: export_range.start,
                export_frames: export_range.end - export_range.start,
                losses,
                output_directory: files.final_root().to_path_buf(),
                files: files
                    .iter()
                    .map(|file| file.final_path().to_path_buf())
                    .collect(),
            };
            Self::begin_completion(&mut state)?;
            let Some(transaction) = state.frozen.take() else {
                Self::cancel_completion(&mut state);
                return Err(LambError::ControlInvariant(
                    "recovered completion lost its frozen transaction",
                ));
            };
            let mut transaction = Some(transaction);
            Self::finalize_completed_transaction(
                &mut state,
                arena,
                release_timeout,
                &mut transaction,
            )?;
            return Ok(outcome);
        }

        if state.frozen.is_none() && state.reusable_decision.is_none() {
            return Err(LambError::ControlInvariant(
                "persistence coordinator has no reusable frozen decision",
            ));
        }
        if state.frozen.is_none() {
            let Some(frozen) = arena.freeze_since(state.committed_until, timeout)? else {
                let cumulative = arena.cumulative_capture_dropped_frames();
                let commit = Self::prevalidate_loss_commit(&state, cumulative, 0)?;
                let losses = Self::commit_losses(&mut state, cumulative, commit);
                return Ok(DumpOutcome::NoNewAudio { losses });
            };
            let requested = state.committed_until.unwrap_or(0);
            let retention_lost_frames = frozen.absolute_range().start.saturating_sub(requested);
            state.frozen = Some(FrozenTransaction {
                frozen,
                retention_lost_frames,
                decision: state
                    .reusable_decision
                    .take()
                    .ok_or(LambError::ControlInvariant(
                        "persistence coordinator has no reusable frozen decision",
                    ))?,
            });
        }

        let (
            range,
            retention_lost_frames,
            mut decision_export_range,
            mut skipped_by_policy,
            skipped_silent,
        ) = {
            let transaction = state.frozen.as_mut().ok_or(LambError::ControlInvariant(
                "frozen transaction disappeared",
            ))?;
            let range = FrameRange {
                start: transaction.frozen.absolute_range().start,
                end: transaction.frozen.absolute_range().end,
            };
            let retention_lost_frames = transaction.retention_lost_frames;
            let preparation = prepare_decision(&transaction.frozen, &mut transaction.decision)?;
            let decision_export_range = if transaction.decision.valid() {
                transaction.decision.export_range()
            } else {
                range.start..range.end
            };
            if decision_export_range.start < range.start
                || decision_export_range.end != range.end
                || decision_export_range.start > decision_export_range.end
            {
                return Err(LambError::ExportInvariant(
                    "frozen export range is outside the consumed range",
                ));
            }
            let skipped_by_policy = transaction.decision.valid()
                && transaction
                    .decision
                    .channels()
                    .iter()
                    .all(|channel| channel.mode == crate::activity::ChannelExportMode::Never);
            let skipped_silent = preparation == DecisionPreparation::SkippedSilent;
            (
                range,
                retention_lost_frames,
                decision_export_range,
                skipped_by_policy,
                skipped_silent,
            )
        };
        let cumulative_before_preparation = arena.cumulative_capture_dropped_frames();
        let loss_commit = Self::prevalidate_loss_commit(
            &state,
            cumulative_before_preparation,
            retention_lost_frames,
        )?;
        let published = if skipped_by_policy || skipped_silent {
            None
        } else {
            let transaction = state
                .frozen
                .as_mut()
                .expect("frozen transaction remains prepared");
            let CoordinatorPrepareRequest::Direct(request) = request;
            let prepared = workspace.prepare(&transaction.frozen, request)?;
            if transaction.decision.valid() {
                decision_export_range = transaction.decision.export_range();
                if decision_export_range.start < range.start
                    || decision_export_range.end != range.end
                    || decision_export_range.start > decision_export_range.end
                {
                    return Err(LambError::ExportInvariant(
                        "frozen export range is outside the consumed range",
                    ));
                }
            }
            match prepared {
                PreparedPersistence::Silent | PreparedPersistence::SkippedSilent => None,
                PreparedPersistence::SkippedByPolicy => {
                    skipped_by_policy = true;
                    None
                }
                prepared => match publisher(prepared) {
                    PreparedPublication::Published => Some(()),
                    PreparedPublication::RetryableFailure(error) => return Err(error),
                    PreparedPublication::Indeterminate { operation, cleanup } => {
                        state.indeterminate_publication = Some(cleanup);
                        return Err(LambError::IndeterminatePublication {
                            operation: Box::new(operation),
                        });
                    }
                },
            }
        };
        let cumulative = arena.cumulative_capture_dropped_frames();
        let losses = Self::commit_losses(&mut state, cumulative, loss_commit);
        state.committed_until = Some(range.end);
        let frames = range.end - range.start;
        let outcome = match published {
            Some(()) => {
                let files = workspace.completed_file_plan();
                DumpOutcome::Written {
                    range,
                    frames,
                    export_start_frame: decision_export_range.start,
                    export_frames: decision_export_range.end - decision_export_range.start,
                    losses,
                    output_directory: files.final_root().to_path_buf(),
                    files: files
                        .iter()
                        .map(|file| file.final_path().to_path_buf())
                        .collect(),
                }
            }
            None if skipped_by_policy => DumpOutcome::SkippedByPolicy {
                range,
                frames,
                losses,
            },
            None => DumpOutcome::SkippedSilent {
                range,
                frames,
                losses,
            },
        };

        workspace.finish_completed_publication();

        Self::begin_completion(&mut state)?;
        let Some(transaction) = state.frozen.take() else {
            Self::cancel_completion(&mut state);
            return Err(LambError::ControlInvariant(
                "committed completion lost its frozen transaction",
            ));
        };
        let mut transaction = Some(transaction);
        Self::finalize_completed_transaction(&mut state, arena, release_timeout, &mut transaction)?;
        Ok(outcome)
    }

    pub fn clear_in_order(&self, arena: &CaptureArena, timeout: Duration) -> Result<()> {
        self.clear_in_order_with_release_timeout(arena, timeout, timeout)
    }

    pub fn clear_in_order_with_release_timeout(
        &self,
        arena: &CaptureArena,
        timeout: Duration,
        release_timeout: Duration,
    ) -> Result<()> {
        let mut state = self.lock_state()?;
        Self::bind_arena(&mut state, arena)?;
        let recovered_clear = Self::recover_pending_clear(&mut state, self.id, arena, timeout)?;
        Self::reconcile_completion_authority(&mut state, arena, timeout)?;
        self.clear_completion_poison_if_reconciled(&state);
        if recovered_clear {
            return Ok(());
        }
        if state.indeterminate_publication.is_some() {
            return Err(LambError::IndeterminatePublication {
                operation: Box::new(LambError::ExportInvariant(
                    "publication cleanup recovery is required before clear",
                )),
            });
        }
        Self::recover_pending_release(&mut state, arena, timeout)?;

        let requested_start = state.committed_until.unwrap_or(0);
        let mut accounting = CaptureClearAccounting {
            arena_runtime_id: arena.runtime_id(),
            coordinator_id: self.id,
            clear_id: state
                .next_clear_id
                .checked_add(1)
                .ok_or(LambError::ControlInvariant(
                    "clear transaction identity exhausted",
                ))?,
            expected_active_start: requested_start,
            pending_retention_lost_frames: state.pending_retention_lost_frames,
            pending_cleared_frames: state.pending_cleared_frames,
        };
        if let Some(transaction) = state.frozen.as_ref() {
            let range = transaction.frozen.absolute_range().clone();
            accounting.pending_retention_lost_frames = checked_loss_add(
                accounting.pending_retention_lost_frames,
                transaction.retention_lost_frames,
                "pending retention loss counter exhausted",
            )?;
            accounting.pending_cleared_frames = checked_loss_add(
                accounting.pending_cleared_frames,
                range.end - range.start,
                "pending cleared frame counter exhausted",
            )?;
            accounting.expected_active_start = range.end;
        }

        let pending = PendingClear {
            arena_runtime_id: arena.runtime_id(),
            coordinator_id: self.id,
            clear_id: accounting.clear_id,
            requested_start,
            accounting,
            frozen_end: state
                .frozen
                .as_ref()
                .map(|transaction| transaction.frozen.absolute_range().end),
            acknowledged_dropped_frames: state.acknowledged_dropped_frames,
        };
        state.next_clear_id = accounting.clear_id;
        state.pending_clear = Some(pending);

        let report = match arena.clear_active_accounted(accounting, timeout) {
            Ok(report) => report,
            Err(error) => {
                if !matches!(
                    error,
                    LambError::ControlInvariant(
                        "capture command timed out" | "capture command client timed out"
                    )
                ) {
                    state.pending_clear = None;
                }
                return Err(error);
            }
        };
        Self::validate_clear_report(&state, self.id, arena, &report)?;
        Self::commit_clear_report(&mut state, report);
        if Self::recover_pending_release(&mut state, arena, release_timeout).is_err() {
            return Ok(());
        }
        Ok(())
    }

    fn lock_state(&self) -> Result<std::sync::MutexGuard<'_, DumpState>> {
        match self.state.lock() {
            Ok(state) => Ok(state),
            Err(error)
                if error.get_ref().completion_in_progress
                    || error.get_ref().pending_completion.is_some() =>
            {
                Ok(error.into_inner())
            }
            Err(_) => Err(LambError::Export("dump state lock poisoned".to_string())),
        }
    }

    fn clear_completion_poison_if_reconciled(&self, state: &DumpState) {
        if !state.completion_in_progress
            && state.pending_completion.is_none()
            && state.pending_release.is_none()
        {
            self.state.clear_poison();
        }
    }

    fn bind_arena(state: &mut DumpState, arena: &CaptureArena) -> Result<()> {
        match state.bound_runtime_id {
            Some(runtime_id) if runtime_id != arena.runtime_id() => {
                Err(LambError::ControlInvariant(
                    "dump coordinator belongs to a different capture runtime",
                ))
            }
            Some(_) => Ok(()),
            None => {
                state.bound_runtime_id = Some(arena.runtime_id());
                Ok(())
            }
        }
    }

    fn recover_indeterminate_publication(
        state: &mut DumpState,
        workspace: &mut PersistenceWorkspace,
    ) -> Result<bool> {
        let Some(mut publication) = state.indeterminate_publication.take() else {
            return Ok(false);
        };
        match workspace.recover_indeterminate_publication(&mut publication) {
            Ok(PublicationRecovery::Complete(_)) => Ok(true),
            Ok(PublicationRecovery::RolledBack) => Ok(false),
            Ok(PublicationRecovery::Pending) => {
                state.indeterminate_publication = Some(publication);
                Err(LambError::IndeterminatePublication {
                    operation: Box::new(LambError::ControlInvariant(
                        "publication recovery remains pending",
                    )),
                })
            }
            Err(error) => {
                state.indeterminate_publication = Some(publication);
                Err(LambError::IndeterminatePublication {
                    operation: Box::new(error),
                })
            }
        }
    }

    fn recover_pending_release(
        state: &mut DumpState,
        arena: &CaptureArena,
        timeout: Duration,
    ) -> Result<()> {
        if state.pending_clear.is_some() {
            return Err(LambError::ControlInvariant(
                "pending clear must be recovered before frozen release",
            ));
        }
        if let Some(mut frozen) = state.pending_release.take() {
            if let Err(error) = arena.release_frozen(&mut frozen, timeout) {
                state.pending_release = Some(frozen);
                return Err(error);
            }
        }
        if let Some(PendingCompletionAuthority::Finalized { failed_release, .. }) =
            state.pending_completion.as_mut()
        {
            if let Some(mut frozen) = failed_release.take() {
                if let Err(error) = arena.release_frozen(&mut frozen, timeout) {
                    *failed_release = Some(frozen);
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    fn reconcile_completion_authority(
        state: &mut DumpState,
        arena: &CaptureArena,
        timeout: Duration,
    ) -> Result<()> {
        match state.pending_completion.as_ref() {
            Some(PendingCompletionAuthority::Delivering) => {
                return Err(LambError::ControlInvariant("completion delivery is active"));
            }
            Some(PendingCompletionAuthority::Finalized { .. }) if !state.completion_in_progress => {
                return Err(LambError::ControlInvariant(
                    "completion authority has no active marker",
                ));
            }
            None if state.completion_in_progress => {
                return Err(LambError::ControlInvariant(
                    "completion marker has no authority slot",
                ));
            }
            _ => {}
        }
        Self::recover_pending_release(state, arena, timeout)?;
        Self::reconcile_completion_authority_without_release(state)
    }

    fn reconcile_completion_authority_without_release(state: &mut DumpState) -> Result<()> {
        let Some(bundle) = state.pending_completion.as_ref() else {
            return if state.completion_in_progress {
                Err(LambError::ControlInvariant(
                    "completion marker has no authority slot",
                ))
            } else {
                Ok(())
            };
        };
        let PendingCompletionAuthority::Finalized {
            reset_decision,
            failed_release,
        } = bundle
        else {
            return Err(LambError::ControlInvariant("completion delivery is active"));
        };
        if !state.completion_in_progress {
            return Err(LambError::ControlInvariant(
                "completion authority has no active marker",
            ));
        }
        if failed_release.is_some() {
            return Err(LambError::ControlInvariant(
                "completion frozen release remains pending",
            ));
        }
        if let Some(primary) = state.reusable_decision.as_ref() {
            if !primary.compatible_reset_with(reset_decision) {
                return Err(LambError::ControlInvariant(
                    "completion reset decisions are incompatible",
                ));
            }
            let _redundant = state.pending_completion.take();
        } else {
            let reset_decision = match state.pending_completion.take() {
                Some(PendingCompletionAuthority::Finalized {
                    reset_decision,
                    failed_release: None,
                }) => reset_decision,
                unexpected => {
                    state.pending_completion = unexpected;
                    return Err(LambError::ControlInvariant(
                        "completion authority changed during reconciliation",
                    ));
                }
            };
            state.reusable_decision = Some(reset_decision);
        }
        state.completion_in_progress = false;
        Ok(())
    }

    fn begin_completion(state: &mut DumpState) -> Result<()> {
        if state.completion_in_progress
            || state.pending_completion.is_some()
            || state.pending_release.is_some()
        {
            return Err(LambError::ControlInvariant(
                "completion authorities must reconcile before delivery",
            ));
        }
        state.completion_in_progress = true;
        state.pending_completion = Some(PendingCompletionAuthority::Delivering);
        Ok(())
    }

    fn cancel_completion(state: &mut DumpState) {
        if matches!(
            state.pending_completion,
            Some(PendingCompletionAuthority::Delivering)
        ) {
            state.pending_completion = None;
            state.completion_in_progress = false;
        }
    }

    fn finalize_completed_transaction(
        state: &mut DumpState,
        arena: &CaptureArena,
        timeout: Duration,
        transaction: &mut Option<FrozenTransaction>,
    ) -> Result<()> {
        if !state.completion_in_progress
            || !matches!(
                state.pending_completion,
                Some(PendingCompletionAuthority::Delivering)
            )
        {
            if state.frozen.is_none() {
                state.frozen = transaction.take();
            }
            return Err(LambError::ControlInvariant(
                "completion authority slot is not reserved for delivery",
            ));
        }
        let Some(transaction) = transaction.take() else {
            Self::cancel_completion(state);
            return Err(LambError::ControlInvariant(
                "completion guard has no frozen transaction",
            ));
        };
        let mut reset_decision = transaction.decision;
        reset_decision.reset();
        let mut frozen = transaction.frozen;
        let failed_release = arena
            .release_frozen(&mut frozen, timeout)
            .err()
            .map(|_| frozen);
        let release_failed = failed_release.is_some();
        state.pending_completion = Some(PendingCompletionAuthority::Finalized {
            reset_decision,
            failed_release,
        });
        if release_failed {
            return Ok(());
        }
        Self::reconcile_completion_authority_without_release(state)
    }

    fn recover_pending_clear(
        state: &mut DumpState,
        coordinator_id: u64,
        arena: &CaptureArena,
        timeout: Duration,
    ) -> Result<bool> {
        let Some(pending) = state.pending_clear else {
            return Ok(false);
        };
        if pending.arena_runtime_id != arena.runtime_id()
            || pending.coordinator_id != coordinator_id
        {
            return Err(LambError::ControlInvariant(
                "pending clear belongs to a different arena or coordinator",
            ));
        }
        let recovery = CaptureClearRecovery {
            arena_runtime_id: pending.arena_runtime_id,
            coordinator_id: pending.coordinator_id,
            clear_id: pending.clear_id,
        };
        let Some(report) = arena.recover_clear_result(recovery, timeout)? else {
            state.pending_clear = None;
            return Err(LambError::ControlInvariant(
                "pending clear completed without a recoverable report",
            ));
        };
        Self::validate_clear_report(state, coordinator_id, arena, &report)?;
        Self::commit_clear_report(state, report);
        Ok(true)
    }

    fn validate_clear_report(
        state: &DumpState,
        coordinator_id: u64,
        arena: &CaptureArena,
        report: &crate::capture_arena::CaptureClearReport,
    ) -> Result<()> {
        let pending = state.pending_clear.ok_or(LambError::ControlInvariant(
            "clear report has no pending coordinator transaction",
        ))?;
        if pending.arena_runtime_id != arena.runtime_id()
            || pending.coordinator_id != coordinator_id
            || report.arena_runtime_id != pending.arena_runtime_id
            || report.coordinator_id != pending.coordinator_id
            || report.clear_id != pending.clear_id
            || report.expected_active_start != pending.accounting.expected_active_start
            || report.cumulative_dropped_frames < pending.acknowledged_dropped_frames
        {
            return Err(LambError::ControlInvariant(
                "clear report does not match pending coordinator transaction",
            ));
        }
        if let Some(frozen_end) = pending.frozen_end {
            if state
                .frozen
                .as_ref()
                .map(|transaction| transaction.frozen.absolute_range().end)
                != Some(frozen_end)
            {
                return Err(LambError::ControlInvariant(
                    "pending clear frozen capability changed before commit",
                ));
            }
        } else if state.frozen.is_some() {
            return Err(LambError::ControlInvariant(
                "pending clear unexpectedly acquired a frozen capability",
            ));
        }
        if pending.requested_start > pending.accounting.expected_active_start
            || report.active_absolute_range.start > report.active_absolute_range.end
        {
            return Err(LambError::ControlInvariant(
                "clear report boundary invariant failed",
            ));
        }
        Ok(())
    }

    fn commit_clear_report(
        state: &mut DumpState,
        report: crate::capture_arena::CaptureClearReport,
    ) {
        let Some(pending) = state.pending_clear else {
            return;
        };
        state.pending_retention_lost_frames = report.pending_retention_lost_frames;
        state.pending_cleared_frames = report.pending_cleared_frames;
        state.committed_until = Some(report.active_absolute_range.end);
        if pending.frozen_end.is_some() {
            let transaction = state
                .frozen
                .take()
                .expect("validated pending clear retains its frozen capability");
            let mut decision = transaction.decision;
            decision.reset();
            state.reusable_decision = Some(decision);
            state.pending_release = Some(transaction.frozen);
        }
        state.pending_clear = None;
    }

    fn prevalidate_loss_commit(
        state: &DumpState,
        cumulative_dropped_frames: u64,
        retention_lost_frames: u64,
    ) -> Result<PrevalidatedLossCommit> {
        if cumulative_dropped_frames < state.acknowledged_dropped_frames {
            return Err(LambError::ControlInvariant(
                "cumulative capture dropped frame counter regressed",
            ));
        }
        let retention_lost_frames = checked_loss_add(
            state.pending_retention_lost_frames,
            retention_lost_frames,
            "retention loss counter exhausted",
        )?;
        Ok(PrevalidatedLossCommit {
            acknowledged_dropped_frames: state.acknowledged_dropped_frames,
            retention_lost_frames,
            cleared_frames: state.pending_cleared_frames,
        })
    }

    fn commit_losses(
        state: &mut DumpState,
        cumulative_dropped_frames: u64,
        commit: PrevalidatedLossCommit,
    ) -> LossBreakdown {
        debug_assert!(cumulative_dropped_frames >= commit.acknowledged_dropped_frames);
        let losses = LossBreakdown {
            retention_lost_frames: commit.retention_lost_frames,
            cleared_frames: commit.cleared_frames,
            capture_dropped_frames: cumulative_dropped_frames - commit.acknowledged_dropped_frames,
        };
        state.acknowledged_dropped_frames = cumulative_dropped_frames;
        state.pending_retention_lost_frames = 0;
        state.pending_cleared_frames = 0;
        losses
    }
}

fn checked_loss_add(left: u64, right: u64, message: &'static str) -> Result<u64> {
    left.checked_add(right)
        .ok_or(LambError::ControlInvariant(message))
}

impl Default for DumpCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

pub struct SampleSnapshot {
    range: FrameRange,
    sample_rate: u32,
    channel_samples: Vec<Vec<f32>>,
}

impl SampleSnapshot {
    pub fn from_ring_range(ring: &SampleRing, range: FrameRange) -> Result<Self> {
        let snapshot = ring.snapshot_range(range.start..range.end)?;
        Self::from_ring_snapshot(snapshot)
    }

    fn from_ring_snapshot(snapshot: Snapshot) -> Result<Self> {
        let range = FrameRange {
            start: snapshot.start_frame(),
            end: snapshot.end_frame(),
        };
        let frames = snapshot.total_frames();
        let channels = snapshot.channels();
        let sample_rate = snapshot.sample_rate();
        let mut channel_samples = Vec::with_capacity(channels as usize);

        for channel in 0..channels {
            let samples = snapshot.read_channel_samples(channel)?;
            if samples.len() as u64 != frames {
                return Err(LambError::Export(format!(
                    "channel {channel} has {} samples for a {frames}-frame snapshot",
                    samples.len()
                )));
            }
            channel_samples.push(samples);
        }

        drop(snapshot);
        Ok(Self {
            range,
            sample_rate,
            channel_samples,
        })
    }

    pub fn range(&self) -> FrameRange {
        self.range
    }

    pub fn frames(&self) -> u64 {
        self.range.end - self.range.start
    }

    pub fn channels(&self) -> u32 {
        self.channel_samples.len() as u32
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn channel_samples(&self) -> &[Vec<f32>] {
        &self.channel_samples
    }

    pub fn is_digital_silence(&self) -> bool {
        self.channel_samples
            .iter()
            .flatten()
            .all(|sample| *sample == 0.0)
    }
}

#[cfg(test)]
mod coordinator_review_tests {
    use super::*;
    use crate::capture_arena::{CaptureClearRecovery, CaptureIngress, CaptureRuntimeConfig};
    use crate::export_policy::{
        ChannelActivityPolicy, ExportCommand, ResolvedActivityPolicy, ResolvedExportPolicy,
        ResolvedLayout,
    };
    use crate::memory_plan::{SessionMemoryInputs, SessionMemoryPlan};
    use crate::persistence_workspace::{PersistenceWorkspaceConfig, PrepareRequest};
    use crate::sample_ring::{RingConfig, SampleFormat};
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::sync::{Arc, Barrier};
    use std::thread;

    const DEADLINE: Duration = Duration::from_secs(2);

    fn runtime(
        retention_frames: u64,
        queue_slots: u32,
        slot_frames: u32,
    ) -> (CaptureArena, CaptureIngress, SessionMemoryPlan) {
        let plan = SessionMemoryPlan::calculate(SessionMemoryInputs {
            retention_frames,
            channels: 1,
            sample_rate: 48_000,
            sample_format: SampleFormat::F32Le,
            chunk_frames: 2,
            max_active_snapshots: 1,
            sample_bytes: 4,
            split_when_over_bytes: 1_000_000,
            control_queue_capacity: 2,
            worker_stack_bytes: 64 * 1024,
            capture_queue_slots: queue_slots,
            capture_slot_frames: slot_frames,
            capture_worker_stack_bytes: 256 * 1024,
            io_buffer_bytes_per_channel: 4 * 1024,
            maximum_path_bytes: 512,
            maximum_calibration_seconds: 0,
            headroom: 1.0,
        })
        .unwrap();
        let (arena, ingress) = CaptureArena::new(
            &plan,
            CaptureRuntimeConfig {
                ring: RingConfig {
                    channels: 1,
                    sample_rate: 48_000,
                    format: SampleFormat::F32Le,
                    chunk_frames: 2,
                    chunk_count: retention_frames.div_ceil(2) as u32,
                    max_active_snapshots: 1,
                },
                queue_slots,
                slot_frames,
                sample_bytes: 4,
                worker_stack_bytes: 256 * 1024,
            },
        )
        .unwrap();
        (arena, ingress, plan)
    }

    fn arena() -> (CaptureArena, CaptureIngress) {
        let (arena, ingress, _) = runtime(8, 8, 4);
        (arena, ingress)
    }

    fn workspace(plan: &SessionMemoryPlan, retention_frames: u64) -> PersistenceWorkspace {
        PersistenceWorkspace::new(
            plan,
            PersistenceWorkspaceConfig {
                retention_frames,
                channels: 1,
                sample_rate: 48_000,
                sample_format: SampleFormat::F32Le,
                chunk_frames: 2,
                sample_bytes: 4,
                split_when_over_bytes: 1_000_000,
                io_buffer_bytes_per_channel: 4 * 1024,
                maximum_path_bytes: 512,
            },
        )
        .unwrap()
    }

    fn delivery_policy(root: &std::path::Path) -> ResolvedExportPolicy {
        ResolvedExportPolicy::new(
            root.join("output"),
            ResolvedLayout::FlatDetailed,
            ResolvedActivityPolicy {
                detector: crate::activity::ActivityDetectorKind::ExactZero,
                channels: vec![ChannelActivityPolicy {
                    name: "mic".to_string(),
                    mode: crate::activity::ChannelExportMode::Always,
                    threshold: None,
                }],
                whole_export_exact_zero_gate: false,
                trim_leading_silence: false,
            },
        )
        .unwrap()
    }

    fn delivery_request<'a>(
        policy: &'a ResolvedExportPolicy,
        staging: &'a std::path::Path,
    ) -> PolicyPersistenceRequest<'a> {
        PolicyPersistenceRequest {
            command: ExportCommand::Recall,
            policy,
            profile: "authority-state",
            staging_root: staging,
            timestamp: "20260826T120000",
        }
    }

    fn decision_plan(channels: u32, sample_rate: u32) -> SessionMemoryPlan {
        SessionMemoryPlan::calculate(SessionMemoryInputs {
            retention_frames: 8,
            channels,
            sample_rate,
            sample_format: SampleFormat::F32Le,
            chunk_frames: 2,
            max_active_snapshots: 1,
            sample_bytes: 4,
            split_when_over_bytes: 1_000_000,
            control_queue_capacity: 2,
            worker_stack_bytes: 64 * 1024,
            capture_queue_slots: 8,
            capture_slot_frames: 4,
            capture_worker_stack_bytes: 256 * 1024,
            io_buffer_bytes_per_channel: 4 * 1024,
            maximum_path_bytes: 512,
            maximum_calibration_seconds: 0,
            headroom: 1.0,
        })
        .unwrap()
    }

    #[test]
    fn borrowed_delivery_releases_state_lock_and_recovers_one_pending_release() {
        let (arena, ingress, plan) = runtime(8, 8, 4);
        let coordinator =
            DumpCoordinator::with_frozen_decision(FrozenExportDecision::new(&plan).unwrap());
        let mut workspace = workspace(&plan, 8);
        let root = tempfile::tempdir().unwrap();
        let policy = ResolvedExportPolicy::new(
            root.path().join("output"),
            ResolvedLayout::FlatDetailed,
            ResolvedActivityPolicy {
                detector: crate::activity::ActivityDetectorKind::ExactZero,
                channels: vec![ChannelActivityPolicy {
                    name: "mic".to_string(),
                    mode: crate::activity::ChannelExportMode::Always,
                    threshold: None,
                }],
                whole_export_exact_zero_gate: false,
                trim_leading_silence: false,
            },
        )
        .unwrap();
        let staging = root.path().join("staging");
        ingress.try_push_interleaved(&[0.25, 0.5], 1).unwrap();
        let error = coordinator.persist_policy_with_delivery(
            &arena,
            &mut workspace,
            PolicyPersistenceRequest {
                command: ExportCommand::Recall,
                policy: &policy,
                profile: "borrowed-delivery",
                staging_root: &staging,
                timestamp: "20260826T120000",
            },
            DEADLINE,
            Duration::ZERO,
            |_| {
                assert!(coordinator.state.try_lock().is_ok());
                assert!(std::panic::catch_unwind(|| {
                    let _state = coordinator.state.lock().unwrap();
                    panic!("poison coordinator state after commit")
                })
                .is_err());
                Err(LambError::Control("injected delivery error".to_string()))
            },
        );
        assert!(matches!(error, Err(LambError::Control(_))));
        {
            let state = coordinator
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            assert_eq!(state.committed_until, Some(2));
            assert!(state.frozen.is_none());
            assert!(state.completion_in_progress);
            assert!(state
                .pending_completion
                .as_ref()
                .is_some_and(|bundle| matches!(
                    bundle,
                    PendingCompletionAuthority::Finalized {
                        failed_release: Some(_),
                        ..
                    }
                )));
        }
        for expected_pending in [false, false] {
            coordinator
                .persist_policy_with_delivery(
                    &arena,
                    &mut workspace,
                    PolicyPersistenceRequest {
                        command: ExportCommand::Recall,
                        policy: &policy,
                        profile: "borrowed-delivery",
                        staging_root: &staging,
                        timestamp: "20260826T120000",
                    },
                    DEADLINE,
                    DEADLINE,
                    |outcome| {
                        assert!(matches!(
                            outcome,
                            CommittedPersistenceRef::NoNewAudio { .. }
                        ));
                        Ok(())
                    },
                )
                .unwrap();
            assert_eq!(
                coordinator
                    .state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .pending_release
                    .is_some(),
                expected_pending
            );
        }
    }

    #[test]
    fn poisoned_delivery_error_finalizes_authorities_and_allows_next_borrowed_command() {
        let (arena, ingress, plan) = runtime(8, 8, 4);
        let coordinator =
            DumpCoordinator::with_frozen_decision(FrozenExportDecision::new(&plan).unwrap());
        let mut workspace = workspace(&plan, 8);
        let root = tempfile::tempdir().unwrap();
        let policy = delivery_policy(root.path());
        let staging = root.path().join("staging");
        ingress.try_push_interleaved(&[0.25, 0.5], 1).unwrap();

        let error = coordinator.persist_policy_with_delivery(
            &arena,
            &mut workspace,
            delivery_request(&policy, &staging),
            DEADLINE,
            DEADLINE,
            |_| {
                let _ = catch_unwind(AssertUnwindSafe(|| {
                    let _state = coordinator.state.lock().unwrap();
                    panic!("poison after commit");
                }));
                Err(LambError::Control("delivery failed".to_string()))
            },
        );
        assert!(matches!(error, Err(LambError::Control(message)) if message == "delivery failed"));
        assert!(coordinator
            .persist_policy_with_delivery(
                &arena,
                &mut workspace,
                delivery_request(&policy, &staging),
                DEADLINE,
                DEADLINE,
                |outcome| {
                    assert!(matches!(
                        outcome,
                        CommittedPersistenceRef::NoNewAudio { .. }
                    ));
                    Ok(())
                },
            )
            .is_ok());
    }

    #[test]
    fn poisoned_delivery_unwind_finalizes_authorities_and_allows_next_borrowed_command() {
        let (arena, ingress, plan) = runtime(8, 8, 4);
        let coordinator =
            DumpCoordinator::with_frozen_decision(FrozenExportDecision::new(&plan).unwrap());
        let mut workspace = workspace(&plan, 8);
        let root = tempfile::tempdir().unwrap();
        let policy = delivery_policy(root.path());
        let staging = root.path().join("staging");
        ingress.try_push_interleaved(&[0.25, 0.5], 1).unwrap();

        assert!(catch_unwind(AssertUnwindSafe(|| {
            let _ = coordinator.persist_policy_with_delivery(
                &arena,
                &mut workspace,
                delivery_request(&policy, &staging),
                DEADLINE,
                DEADLINE,
                |_| {
                    let _ = catch_unwind(AssertUnwindSafe(|| {
                        let _state = coordinator.state.lock().unwrap();
                        panic!("poison during callback unwind");
                    }));
                    panic!("delivery unwind");
                },
            );
        }))
        .is_err());
        assert!(coordinator
            .persist_policy_with_delivery(
                &arena,
                &mut workspace,
                delivery_request(&policy, &staging),
                DEADLINE,
                DEADLINE,
                |outcome| {
                    assert!(matches!(
                        outcome,
                        CommittedPersistenceRef::NoNewAudio { .. }
                    ));
                    Ok(())
                },
            )
            .is_ok());
    }

    #[test]
    fn active_completion_rejects_reentrant_persist_clear_and_dump_without_mutation() {
        let (arena, ingress, plan) = runtime(8, 8, 4);
        let coordinator =
            DumpCoordinator::with_frozen_decision(FrozenExportDecision::new(&plan).unwrap());
        let mut reentrant_workspace = workspace(&plan, 8);
        let mut workspace = workspace(&plan, 8);
        let ring = SampleRing::new(RingConfig {
            channels: 1,
            sample_rate: 48_000,
            format: SampleFormat::F32Le,
            chunk_frames: 2,
            chunk_count: 4,
            max_active_snapshots: 1,
        })
        .unwrap();
        let root = tempfile::tempdir().unwrap();
        let policy = delivery_policy(root.path());
        let staging = root.path().join("staging");
        let channel_names = vec!["mic".to_string()];
        ingress.try_push_interleaved(&[0.25, 0.5], 1).unwrap();

        coordinator
            .persist_policy_with_delivery(
                &arena,
                &mut workspace,
                delivery_request(&policy, &staging),
                DEADLINE,
                DEADLINE,
                |_| {
                    let before = {
                        let state = coordinator.state.lock().unwrap();
                        (
                            state.committed_until,
                            state.frozen.is_some(),
                            state.completion_in_progress,
                            matches!(
                                state.pending_completion,
                                Some(PendingCompletionAuthority::Delivering)
                            ),
                        )
                    };
                    assert!(matches!(
                        coordinator.persist_policy_with_delivery(
                            &arena,
                            &mut reentrant_workspace,
                            delivery_request(&policy, &staging),
                            DEADLINE,
                            DEADLINE,
                            |_| panic!("reentrant persistence reached delivery"),
                        ),
                        Err(LambError::ControlInvariant("completion delivery is active"))
                    ));
                    assert!(matches!(
                        coordinator.persist(
                            &arena,
                            &mut reentrant_workspace,
                            PrepareRequest::Recall {
                                staging_root: &staging,
                                output_dir: &root.path().join("legacy-output"),
                                timestamp: "20260826T120001",
                                channel_names: &channel_names,
                            },
                            DEADLINE,
                        ),
                        Err(LambError::ControlInvariant("completion delivery is active"))
                    ));
                    assert!(matches!(
                        coordinator.clear_in_order(&arena, DEADLINE),
                        Err(LambError::ControlInvariant("completion delivery is active"))
                    ));
                    assert!(matches!(
                        coordinator.dump(&ring, |_| panic!("reentrant dump reached publisher")),
                        Err(LambError::ControlInvariant("completion delivery is active"))
                    ));
                    let state = coordinator.state.lock().unwrap();
                    assert_eq!(state.committed_until, before.0);
                    assert_eq!(state.frozen.is_some(), before.1);
                    assert_eq!(state.completion_in_progress, before.2);
                    assert_eq!(
                        matches!(
                            state.pending_completion,
                            Some(PendingCompletionAuthority::Delivering)
                        ),
                        before.3
                    );
                    Ok(())
                },
            )
            .unwrap();
    }

    #[test]
    fn poisoned_compatible_primary_conflict_reconciles_and_allows_next_command() {
        let (arena, ingress, plan) = runtime(8, 8, 4);
        let coordinator =
            DumpCoordinator::with_frozen_decision(FrozenExportDecision::new(&plan).unwrap());
        let mut workspace = workspace(&plan, 8);
        let root = tempfile::tempdir().unwrap();
        let policy = delivery_policy(root.path());
        let staging = root.path().join("staging");
        ingress.try_push_interleaved(&[0.25, 0.5], 1).unwrap();

        coordinator
            .persist_policy_with_delivery(
                &arena,
                &mut workspace,
                delivery_request(&policy, &staging),
                DEADLINE,
                DEADLINE,
                |_| {
                    assert!(catch_unwind(AssertUnwindSafe(|| {
                        let mut state = coordinator.state.lock().unwrap();
                        state.reusable_decision = Some(FrozenExportDecision::new(&plan).unwrap());
                        panic!("poison with compatible primary decision");
                    }))
                    .is_err());
                    Ok(())
                },
            )
            .unwrap();

        assert!(!coordinator.state.is_poisoned());
        {
            let state = coordinator.state.lock().unwrap();
            assert!(state.reusable_decision.is_some());
            assert!(state.pending_completion.is_none());
            assert!(!state.completion_in_progress);
        }
        coordinator
            .persist_policy_with_delivery(
                &arena,
                &mut workspace,
                delivery_request(&policy, &staging),
                DEADLINE,
                DEADLINE,
                |outcome| {
                    assert!(matches!(
                        outcome,
                        CommittedPersistenceRef::NoNewAudio { .. }
                    ));
                    Ok(())
                },
            )
            .unwrap();
    }

    #[test]
    fn poisoned_incompatible_primary_conflict_stays_represented_and_reports_invariant() {
        let (arena, ingress, plan) = runtime(8, 8, 4);
        let incompatible_plan = decision_plan(2, 48_000);
        let coordinator =
            DumpCoordinator::with_frozen_decision(FrozenExportDecision::new(&plan).unwrap());
        let mut retry_workspace = workspace(&plan, 8);
        let mut workspace = workspace(&plan, 8);
        let root = tempfile::tempdir().unwrap();
        let policy = delivery_policy(root.path());
        let staging = root.path().join("staging");
        ingress.try_push_interleaved(&[0.25, 0.5], 1).unwrap();

        coordinator
            .persist_policy_with_delivery(
                &arena,
                &mut workspace,
                delivery_request(&policy, &staging),
                DEADLINE,
                DEADLINE,
                |_| {
                    assert!(catch_unwind(AssertUnwindSafe(|| {
                        let mut state = coordinator.state.lock().unwrap();
                        state.reusable_decision =
                            Some(FrozenExportDecision::new(&incompatible_plan).unwrap());
                        panic!("poison with incompatible primary decision");
                    }))
                    .is_err());
                    Ok(())
                },
            )
            .unwrap();

        assert!(coordinator.state.is_poisoned());
        assert!(matches!(
            coordinator.persist_policy_with_delivery(
                &arena,
                &mut retry_workspace,
                delivery_request(&policy, &staging),
                DEADLINE,
                DEADLINE,
                |_| panic!("incompatible retry reached delivery"),
            ),
            Err(LambError::ControlInvariant(
                "completion reset decisions are incompatible"
            ))
        ));
        assert!(coordinator.state.is_poisoned());
        let state = match coordinator.state.lock() {
            Ok(_) => panic!("incompatible completion poison was cleared"),
            Err(error) => error.into_inner(),
        };
        assert!(state.reusable_decision.is_some());
        assert!(matches!(
            state.pending_completion,
            Some(PendingCompletionAuthority::Finalized {
                failed_release: None,
                ..
            })
        ));
        assert!(state.completion_in_progress);
    }

    #[test]
    fn pending_releases_are_retried_primary_first_without_overwrite() {
        let (primary_arena, primary_ingress, plan) = runtime(8, 8, 4);
        let (bundle_arena, bundle_ingress, _) = runtime(8, 8, 4);
        primary_ingress
            .try_push_interleaved(&[0.25, 0.5], 1)
            .unwrap();
        bundle_ingress
            .try_push_interleaved(&[0.75, 1.0], 1)
            .unwrap();
        let primary = primary_arena.freeze_since(None, DEADLINE).unwrap().unwrap();
        let bundled = bundle_arena.freeze_since(None, DEADLINE).unwrap().unwrap();
        let coordinator =
            DumpCoordinator::with_frozen_decision(FrozenExportDecision::new(&plan).unwrap());
        let mut state = coordinator.state.lock().unwrap();
        state.pending_release = Some(primary);
        state.completion_in_progress = true;
        state.pending_completion = Some(PendingCompletionAuthority::Finalized {
            reset_decision: FrozenExportDecision::new(&plan).unwrap(),
            failed_release: Some(bundled),
        });

        assert!(DumpCoordinator::reconcile_completion_authority(
            &mut state,
            &bundle_arena,
            DEADLINE,
        )
        .is_err());
        assert!(state.pending_release.is_some());
        assert!(matches!(
            state.pending_completion,
            Some(PendingCompletionAuthority::Finalized {
                failed_release: Some(_),
                ..
            })
        ));

        assert!(DumpCoordinator::reconcile_completion_authority(
            &mut state,
            &primary_arena,
            DEADLINE,
        )
        .is_err());
        assert!(state.pending_release.is_none());
        assert!(matches!(
            state.pending_completion,
            Some(PendingCompletionAuthority::Finalized {
                failed_release: Some(_),
                ..
            })
        ));

        DumpCoordinator::reconcile_completion_authority(&mut state, &bundle_arena, DEADLINE)
            .unwrap();
        assert!(state.pending_release.is_none());
        assert!(state.pending_completion.is_none());
        assert!(!state.completion_in_progress);
        assert!(state.reusable_decision.is_some());
    }

    #[test]
    fn compatible_primary_decision_reconciles_one_bundle_and_clears_marker_and_poison() {
        let (_, _, plan) = runtime(8, 8, 4);
        let coordinator =
            DumpCoordinator::with_frozen_decision(FrozenExportDecision::new(&plan).unwrap());
        let mut state = coordinator.state.lock().unwrap();
        state.completion_in_progress = true;
        state.pending_completion = Some(PendingCompletionAuthority::Finalized {
            reset_decision: FrozenExportDecision::new(&plan).unwrap(),
            failed_release: None,
        });
        DumpCoordinator::reconcile_completion_authority_without_release(&mut state).unwrap();
        assert!(state.reusable_decision.is_some());
        assert!(state.pending_completion.is_none());
        assert!(!state.completion_in_progress);
    }

    #[test]
    fn incompatible_primary_decision_preserves_bundle_and_marker() {
        let primary_plan = decision_plan(1, 48_000);
        let incompatible_plan = decision_plan(2, 48_000);
        let coordinator = DumpCoordinator::with_frozen_decision(
            FrozenExportDecision::new(&primary_plan).unwrap(),
        );
        let mut state = coordinator.state.lock().unwrap();
        state.completion_in_progress = true;
        state.pending_completion = Some(PendingCompletionAuthority::Finalized {
            reset_decision: FrozenExportDecision::new(&incompatible_plan).unwrap(),
            failed_release: None,
        });
        assert!(matches!(
            DumpCoordinator::reconcile_completion_authority_without_release(&mut state),
            Err(LambError::ControlInvariant(
                "completion reset decisions are incompatible"
            ))
        ));
        assert!(state.reusable_decision.is_some());
        assert!(state.pending_completion.is_some());
        assert!(state.completion_in_progress);
    }

    #[test]
    fn clear_overflow_before_frozen_release_preserves_frozen_capability() {
        let (arena, ingress) = arena();
        let coordinator = DumpCoordinator::new();
        ingress.try_push_interleaved(&[1.0, 2.0], 1).unwrap();
        let frozen = arena.freeze_since(None, DEADLINE).unwrap().unwrap();
        {
            let mut state = coordinator.state.lock().unwrap();
            state.bound_runtime_id = Some(arena.runtime_id());
            state.frozen = Some(FrozenTransaction {
                retention_lost_frames: 0,
                frozen,
                decision: FrozenExportDecision::new(&runtime(8, 8, 4).2).unwrap(),
            });
            state.pending_cleared_frames = u64::MAX;
        }

        assert!(coordinator.clear_in_order(&arena, DEADLINE).is_err());

        assert!(arena.status(DEADLINE).unwrap().frozen_pending);
        let state = coordinator.state.lock().unwrap();
        assert!(state.frozen.is_some());
        assert_eq!(state.committed_until, None);
    }

    #[test]
    fn clear_overflow_at_active_boundary_does_not_clear_ring_or_cursor() {
        let (arena, ingress) = arena();
        let coordinator = DumpCoordinator::new();
        ingress.try_push_interleaved(&[1.0, 2.0], 1).unwrap();
        {
            let mut state = coordinator.state.lock().unwrap();
            state.bound_runtime_id = Some(arena.runtime_id());
            state.pending_cleared_frames = u64::MAX;
        }

        assert!(coordinator.clear_in_order(&arena, DEADLINE).is_err());

        assert_eq!(arena.active_absolute_range(DEADLINE).unwrap(), 0..2);
        assert_eq!(coordinator.state.lock().unwrap().committed_until, None);
    }

    #[test]
    fn status_stashes_timed_out_clear_and_persist_commits_it_once_before_freeze() {
        let (arena, ingress, plan) = runtime(2, 1, 4);
        let arena = Arc::new(arena);
        let coordinator = Arc::new(DumpCoordinator::with_frozen_decision(
            FrozenExportDecision::new(&plan).unwrap(),
        ));
        let mut workspace = workspace(&plan, 2);
        let root = tempfile::tempdir().unwrap();
        let names = vec!["mic".to_string()];
        let pushed = ingress.try_push_interleaved(&[1.0; 7], 1).unwrap();
        assert_eq!(pushed.enqueued_frames, 4);
        assert_eq!(pushed.dropped_frames, 3);
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        arena.set_clear_reply_pause_for_test(Arc::clone(&entered), Arc::clone(&release));

        let clear_arena = Arc::clone(&arena);
        let clear_coordinator = Arc::clone(&coordinator);
        let clear = thread::spawn(move || {
            clear_coordinator.clear_in_order(&clear_arena, Duration::from_millis(10))
        });
        entered.wait();
        assert!(clear.join().unwrap().is_err());
        release.wait();

        assert_eq!(arena.status(DEADLINE).unwrap().active_absolute_range, 4..4);
        let first = coordinator
            .persist(
                &arena,
                &mut workspace,
                PrepareRequest::Recall {
                    staging_root: &root.path().join("staging"),
                    output_dir: &root.path().join("output"),
                    timestamp: "20260818T130000",
                    channel_names: &names,
                },
                DEADLINE,
            )
            .unwrap();
        assert_eq!(first.range(), None);
        assert_eq!(first.losses().retention_lost_frames, 2);
        assert_eq!(first.losses().cleared_frames, 2);
        assert_eq!(first.losses().capture_dropped_frames, 3);

        let repeated = coordinator
            .persist(
                &arena,
                &mut workspace,
                PrepareRequest::Recall {
                    staging_root: &root.path().join("staging"),
                    output_dir: &root.path().join("output"),
                    timestamp: "20260818T130001",
                    channel_names: &names,
                },
                DEADLINE,
            )
            .unwrap();
        assert_eq!(repeated.losses().lost_frames(), 0);
    }

    #[test]
    fn concurrent_clear_report_recovery_has_exactly_one_owner() {
        let (arena, ingress) = arena();
        let arena = Arc::new(arena);
        ingress.try_push_interleaved(&[1.0, 2.0], 1).unwrap();
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        arena.set_clear_reply_pause_for_test(Arc::clone(&entered), Arc::clone(&release));
        let clear_arena = Arc::clone(&arena);
        let clear = thread::spawn(move || clear_arena.clear_active(Duration::from_millis(10)));
        entered.wait();
        assert!(clear.join().unwrap().is_err());
        release.wait();

        let callers = (0..2)
            .map(|_| {
                let arena = Arc::clone(&arena);
                let recovery = CaptureClearRecovery {
                    arena_runtime_id: arena.runtime_id(),
                    coordinator_id: 0,
                    clear_id: 0,
                };
                thread::spawn(move || {
                    arena
                        .recover_clear_result(recovery, DEADLINE)
                        .unwrap()
                        .is_some()
                })
            })
            .collect::<Vec<_>>();
        assert_eq!(
            callers
                .into_iter()
                .map(|caller| caller.join().unwrap())
                .filter(|owned| *owned)
                .count(),
            1
        );
    }

    #[test]
    fn dropping_coordinator_does_not_discard_pending_clear_report() {
        let (arena, ingress) = arena();
        let arena = Arc::new(arena);
        let coordinator = Arc::new(DumpCoordinator::new());
        let coordinator_id = coordinator.id;
        ingress.try_push_interleaved(&[1.0, 2.0], 1).unwrap();
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        arena.set_clear_reply_pause_for_test(Arc::clone(&entered), Arc::clone(&release));
        let clear_arena = Arc::clone(&arena);
        let clear_coordinator = Arc::clone(&coordinator);
        let clear = thread::spawn(move || {
            clear_coordinator.clear_in_order(&clear_arena, Duration::from_millis(10))
        });
        entered.wait();
        assert!(clear.join().unwrap().is_err());
        drop(coordinator);
        release.wait();

        let _ = arena.status(DEADLINE).unwrap();
        let recovery = CaptureClearRecovery {
            arena_runtime_id: arena.runtime_id(),
            coordinator_id,
            clear_id: 1,
        };
        let mismatched = CaptureClearRecovery {
            coordinator_id: 99,
            ..recovery
        };
        assert!(arena.recover_clear_result(mismatched, DEADLINE).is_err());
        assert!(arena
            .recover_clear_result(recovery, DEADLINE)
            .unwrap()
            .is_some());
        assert!(arena
            .recover_clear_result(recovery, DEADLINE)
            .unwrap()
            .is_none());
    }
}
