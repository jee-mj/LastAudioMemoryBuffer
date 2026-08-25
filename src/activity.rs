use crate::capture_arena::FrozenCaptureEpoch;
use crate::error::{LambError, Result};
use crate::export_policy::ResolvedActivityPolicy;
use crate::memory_plan::{
    ExactArray, MaterializedBuffer, SessionMemoryPlan, ACTIVITY_DETECTOR_CHANNEL_WORKSPACE_BYTES,
    FROZEN_EXPORT_DECISION_SLOT_BYTES,
};
use serde::{Deserialize, Serialize};
use std::mem::size_of;
use std::ops::Range;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChannelExportMode {
    Always,
    Auto,
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ActivityDetectorKind {
    ExactZero,
    WindowedRmsPeak,
    FixedLevel,
    CalibratedNoiseFloor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityResult {
    Active,
    Inactive,
    Ambiguous,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelDisposition {
    Retain,
    Omit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrozenChannelDecision {
    pub mode: ChannelExportMode,
    pub result: ActivityResult,
    pub disposition: ChannelDisposition,
    pub first_evidence_frame: Option<u64>,
}

const _: [(); FROZEN_EXPORT_DECISION_SLOT_BYTES as usize] =
    [(); size_of::<FrozenChannelDecision>()];

impl FrozenChannelDecision {
    pub fn retained(
        mode: ChannelExportMode,
        result: ActivityResult,
        first_evidence_frame: Option<u64>,
    ) -> Self {
        Self {
            mode,
            result,
            disposition: ChannelDisposition::Retain,
            first_evidence_frame,
        }
    }

    const fn empty() -> Self {
        Self {
            mode: ChannelExportMode::Never,
            result: ActivityResult::Inactive,
            disposition: ChannelDisposition::Omit,
            first_evidence_frame: None,
        }
    }
}

pub struct FrozenExportDecision {
    channels: ExactArray<FrozenChannelDecision>,
    export_range: Range<u64>,
    sample_rate: u32,
    valid: bool,
}

impl FrozenExportDecision {
    pub fn new(plan: &SessionMemoryPlan) -> Result<Self> {
        Ok(Self {
            channels: ExactArray::try_from_fn(plan.channels() as usize, |_| {
                Ok(FrozenChannelDecision::empty())
            })?,
            export_range: 0..0,
            sample_rate: plan.sample_rate(),
            valid: false,
        })
    }

    pub fn valid(&self) -> bool {
        self.valid
    }
    pub fn export_range(&self) -> Range<u64> {
        self.export_range.clone()
    }
    pub fn channels(&self) -> &[FrozenChannelDecision] {
        self.channels.as_slice()
    }

    pub fn finalize(
        &mut self,
        frozen_range: Range<u64>,
        channels: &[FrozenChannelDecision],
        trim_leading_silence: bool,
        whole_export_exact_zero_gate: bool,
    ) -> Result<()> {
        if self.valid {
            return Err(LambError::ExportInvariant(
                "frozen export decision is already final",
            ));
        }
        if frozen_range.start > frozen_range.end || channels.len() != self.channels.len() {
            return Err(LambError::ExportInvariant(
                "invalid frozen export decision geometry",
            ));
        }
        if channels.iter().any(|channel| {
            channel
                .first_evidence_frame
                .is_some_and(|frame| frame < frozen_range.start || frame >= frozen_range.end)
        }) {
            return Err(LambError::ExportInvariant(
                "activity evidence is outside the frozen range",
            ));
        }
        let all_never = channels
            .iter()
            .all(|channel| channel.mode == ChannelExportMode::Never);
        let forced_omit = all_never || whole_export_exact_zero_gate;
        let first = channels
            .iter()
            .filter(|channel| !forced_omit && channel.disposition == ChannelDisposition::Retain)
            .filter_map(|channel| channel.first_evidence_frame)
            .min();
        let start = if trim_leading_silence {
            first
                .map(|frame| frame.saturating_sub(2 * u64::from(self.sample_rate)))
                .unwrap_or(frozen_range.start)
                .max(frozen_range.start)
        } else {
            frozen_range.start
        };
        if start > frozen_range.end {
            return Err(LambError::ExportInvariant(
                "activity crop is outside the frozen range",
            ));
        }
        for (target, source) in self.channels.as_mut_slice().iter_mut().zip(channels) {
            let mut value = *source;
            if forced_omit {
                value.disposition = ChannelDisposition::Omit;
            }
            *target = value;
        }
        self.export_range = start..frozen_range.end;
        self.valid = true;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
struct WindowAccumulator {
    start: u64,
    sum_squares: f64,
    peak: f64,
    count: u32,
}

impl WindowAccumulator {
    const fn new(start: u64) -> Self {
        Self {
            start,
            sum_squares: 0.0,
            peak: 0.0,
            count: 0,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct DetectorState {
    accumulators: [WindowAccumulator; 2],
    gate_open_since: Option<u64>,
    result: ActivityResult,
    non_finite: bool,
    exact_nonzero: bool,
    evidence: Option<u64>,
}

const _: [(); ACTIVITY_DETECTOR_CHANNEL_WORKSPACE_BYTES as usize] =
    [(); size_of::<DetectorState>() + size_of::<FrozenChannelDecision>()];

impl DetectorState {
    const fn empty() -> Self {
        Self {
            accumulators: [WindowAccumulator::new(0), WindowAccumulator::new(0)],
            gate_open_since: None,
            result: ActivityResult::Inactive,
            non_finite: false,
            exact_nonzero: false,
            evidence: None,
        }
    }

    fn prepare_windowed(&mut self, hop: u32) {
        self.accumulators = [
            WindowAccumulator::new(0),
            WindowAccumulator::new(u64::from(hop)),
        ];
    }
}

pub struct DetectorWorkspace {
    states: ExactArray<DetectorState>,
    scratch: MaterializedBuffer<f32>,
    decisions: ExactArray<FrozenChannelDecision>,
}

impl DetectorWorkspace {
    pub fn new(plan: &SessionMemoryPlan) -> Result<Self> {
        let channels = plan.channels() as usize;
        let scratch = channels
            .checked_mul(plan.chunk_frames() as usize)
            .ok_or_else(|| LambError::Validation("activity scratch size overflow".to_string()))?;
        Ok(Self {
            states: ExactArray::try_from_fn(channels, |_| Ok(DetectorState::empty()))?,
            scratch: MaterializedBuffer::new_zeroed(scratch)?,
            decisions: ExactArray::try_from_fn(channels, |_| Ok(FrozenChannelDecision::empty()))?,
        })
    }

    pub fn reset(&mut self) {
        for state in self.states.as_mut_slice() {
            *state = DetectorState::empty();
        }
        for decision in self.decisions.as_mut_slice() {
            *decision = FrozenChannelDecision::empty();
        }
    }
}

pub trait ActivityDetector {
    fn classify(
        &self,
        samples: &[f32],
        channels: u32,
        start_frame: u64,
        sample_rate: u32,
        threshold_dbfs: f64,
        workspace: &mut DetectorWorkspace,
    ) -> Result<FrozenChannelDecision>;
}

pub struct WindowedRmsPeakDetector;

impl ActivityDetector for WindowedRmsPeakDetector {
    fn classify(
        &self,
        samples: &[f32],
        channels: u32,
        start_frame: u64,
        sample_rate: u32,
        threshold_dbfs: f64,
        workspace: &mut DetectorWorkspace,
    ) -> Result<FrozenChannelDecision> {
        if channels != 1 || sample_rate == 0 || !samples.len().is_multiple_of(channels as usize) {
            return Err(LambError::Validation(
                "invalid windowed detector geometry".to_string(),
            ));
        }
        if workspace.states.len() != 1 {
            return Err(LambError::Validation(
                "windowed detector workspace must be mono".to_string(),
            ));
        }
        workspace.reset();
        let geometry = WindowGeometry::new(sample_rate)?;
        let threshold = threshold_dbfs.is_finite().then_some(threshold_dbfs);
        prepare_state(
            &mut workspace.states.as_mut_slice()[0],
            ActivityDetectorKind::WindowedRmsPeak,
            geometry,
            threshold,
            start_frame,
        );
        for (frame, &sample) in samples.iter().enumerate() {
            feed_sample(
                &mut workspace.states.as_mut_slice()[0],
                ActivityDetectorKind::WindowedRmsPeak,
                sample,
                frame as u64,
                start_frame,
                geometry,
                threshold,
            )?;
        }
        finish_state(
            &mut workspace.states.as_mut_slice()[0],
            ActivityDetectorKind::WindowedRmsPeak,
            start_frame,
            geometry,
            threshold,
        )?;
        Ok(channel_decision(
            ChannelExportMode::Auto,
            workspace.states.as_slice()[0],
        ))
    }
}

#[derive(Debug, Clone, Copy)]
struct WindowGeometry {
    window: u32,
    hop: u32,
    sustained: u64,
}

impl WindowGeometry {
    fn new(sample_rate: u32) -> Result<Self> {
        if sample_rate == 0 {
            return Err(LambError::Validation(
                "windowed detector sample rate is zero".to_string(),
            ));
        }
        let rate = u64::from(sample_rate);
        let window = u32::try_from((rate * 20).div_ceil(1_000))
            .map_err(|_| LambError::Validation("window size overflow".to_string()))?;
        let hop = u32::try_from((rate * 10).div_ceil(1_000))
            .map_err(|_| LambError::Validation("window hop overflow".to_string()))?;
        let sustained = (rate * 100).div_ceil(1_000);
        Ok(Self {
            window: window.max(1),
            hop: hop.max(1),
            sustained: sustained.max(1),
        })
    }
}

fn prepare_state(
    state: &mut DetectorState,
    detector: ActivityDetectorKind,
    geometry: WindowGeometry,
    threshold: Option<f64>,
    transaction_start: u64,
) {
    if detector == ActivityDetectorKind::WindowedRmsPeak {
        state.prepare_windowed(geometry.hop);
        if threshold.is_none() {
            state.result = ActivityResult::Ambiguous;
            state.evidence = Some(transaction_start);
        }
    }
}

fn feed_sample(
    state: &mut DetectorState,
    detector: ActivityDetectorKind,
    sample: f32,
    relative_frame: u64,
    transaction_start: u64,
    geometry: WindowGeometry,
    threshold: Option<f64>,
) -> Result<()> {
    let absolute_frame = transaction_start
        .checked_add(relative_frame)
        .ok_or(LambError::ExportInvariant("activity frame overflow"))?;
    if !sample.is_finite() {
        state.non_finite = true;
    } else if sample != 0.0 {
        state.exact_nonzero = true;
        if detector == ActivityDetectorKind::ExactZero {
            state.result = ActivityResult::Active;
            state.evidence.get_or_insert(absolute_frame);
        }
    }
    if detector != ActivityDetectorKind::WindowedRmsPeak || threshold.is_none() {
        return Ok(());
    }
    for index in 0..state.accumulators.len() {
        let accumulator = &mut state.accumulators[index];
        if relative_frame < accumulator.start
            || relative_frame >= accumulator.start + u64::from(geometry.window)
        {
            continue;
        }
        accumulator.count += 1;
        if sample.is_finite() {
            let value = f64::from(sample);
            accumulator.sum_squares += value * value;
            accumulator.peak = accumulator.peak.max(value.abs());
        }
        if accumulator.count == geometry.window {
            let completed = *accumulator;
            accumulator.start = accumulator
                .start
                .checked_add(2 * u64::from(geometry.hop))
                .ok_or(LambError::ExportInvariant("activity window overflow"))?;
            accumulator.sum_squares = 0.0;
            accumulator.peak = 0.0;
            accumulator.count = 0;
            evaluate_window(
                state,
                completed,
                transaction_start,
                geometry.sustained,
                threshold.expect("threshold checked above"),
            )?;
        }
    }
    Ok(())
}

fn evaluate_window(
    state: &mut DetectorState,
    accumulator: WindowAccumulator,
    transaction_start: u64,
    sustained_frames: u64,
    open_threshold_dbfs: f64,
) -> Result<()> {
    if accumulator.count == 0 {
        return Ok(());
    }
    let window_start = transaction_start
        .checked_add(accumulator.start)
        .ok_or(LambError::ExportInvariant("activity evidence overflow"))?;
    let window_end = window_start
        .checked_add(u64::from(accumulator.count))
        .ok_or(LambError::ExportInvariant("activity window end overflow"))?;
    let rms_dbfs = dbfs((accumulator.sum_squares / f64::from(accumulator.count)).sqrt());
    let peak_dbfs = dbfs(accumulator.peak);
    let close_threshold_dbfs = open_threshold_dbfs - 6.0;
    let transient = peak_dbfs >= open_threshold_dbfs + 12.0;

    if rms_dbfs >= close_threshold_dbfs || transient {
        state.evidence = Some(
            state
                .evidence
                .map_or(window_start, |current| current.min(window_start)),
        );
        if state.result == ActivityResult::Inactive {
            state.result = ActivityResult::Ambiguous;
        }
    }
    if transient {
        state.result = ActivityResult::Active;
        return Ok(());
    }
    if let Some(open_since) = state.gate_open_since {
        if rms_dbfs < close_threshold_dbfs {
            state.gate_open_since = None;
        } else if window_end.saturating_sub(open_since) >= sustained_frames {
            state.result = ActivityResult::Active;
        }
    } else if rms_dbfs >= open_threshold_dbfs {
        state.gate_open_since = Some(window_start);
        if window_end.saturating_sub(window_start) >= sustained_frames {
            state.result = ActivityResult::Active;
        }
    }
    Ok(())
}

fn finish_state(
    state: &mut DetectorState,
    detector: ActivityDetectorKind,
    transaction_start: u64,
    geometry: WindowGeometry,
    threshold: Option<f64>,
) -> Result<()> {
    if detector == ActivityDetectorKind::WindowedRmsPeak {
        if let Some(threshold) = threshold {
            let mut partial = state.accumulators;
            partial.sort_by_key(|accumulator| accumulator.start);
            for accumulator in partial {
                if accumulator.count > 0 {
                    evaluate_window(
                        state,
                        accumulator,
                        transaction_start,
                        geometry.sustained,
                        threshold,
                    )?;
                }
            }
        }
    }
    if state.non_finite {
        state.result = ActivityResult::Ambiguous;
    }
    Ok(())
}

fn channel_decision(mode: ChannelExportMode, state: DetectorState) -> FrozenChannelDecision {
    FrozenChannelDecision {
        mode,
        result: state.result,
        disposition: match mode {
            ChannelExportMode::Never => ChannelDisposition::Omit,
            ChannelExportMode::Always => ChannelDisposition::Retain,
            ChannelExportMode::Auto if state.result == ActivityResult::Inactive => {
                ChannelDisposition::Omit
            }
            ChannelExportMode::Auto => ChannelDisposition::Retain,
        },
        first_evidence_frame: state.evidence,
    }
}

fn dbfs(value: f64) -> f64 {
    if value == 0.0 {
        -120.0
    } else {
        20.0 * value.log10()
    }
}

pub fn classify_samples<'a>(
    samples: &[f32],
    channels: u32,
    start_frame: u64,
    sample_rate: u32,
    policy: &ResolvedActivityPolicy,
    workspace: &'a mut DetectorWorkspace,
) -> Result<&'a [FrozenChannelDecision]> {
    let channel_count = channels as usize;
    if channel_count == 0
        || !samples.len().is_multiple_of(channel_count)
        || policy.channels.len() != channel_count
        || workspace.decisions.len() != channel_count
    {
        return Err(LambError::Validation(
            "invalid activity classification geometry".to_string(),
        ));
    }
    workspace.reset();
    let geometry = WindowGeometry::new(sample_rate)?;
    for (channel, config) in policy.channels.iter().enumerate() {
        let threshold = config
            .threshold
            .as_ref()
            .map(|value| value.threshold_dbfs)
            .filter(|value| value.is_finite());
        prepare_state(
            &mut workspace.states.as_mut_slice()[channel],
            policy.detector,
            geometry,
            threshold,
            start_frame,
        );
    }
    for (frame, values) in samples.chunks_exact(channel_count).enumerate() {
        for (channel, &sample) in values.iter().enumerate() {
            let threshold = policy.channels[channel]
                .threshold
                .as_ref()
                .map(|value| value.threshold_dbfs)
                .filter(|value| value.is_finite());
            feed_sample(
                &mut workspace.states.as_mut_slice()[channel],
                policy.detector,
                sample,
                frame as u64,
                start_frame,
                geometry,
                threshold,
            )?;
        }
    }
    for (channel, config) in policy.channels.iter().enumerate() {
        let threshold = config
            .threshold
            .as_ref()
            .map(|value| value.threshold_dbfs)
            .filter(|value| value.is_finite());
        finish_state(
            &mut workspace.states.as_mut_slice()[channel],
            policy.detector,
            start_frame,
            geometry,
            threshold,
        )?;
        workspace.decisions.as_mut_slice()[channel] =
            channel_decision(config.mode, workspace.states.as_slice()[channel]);
    }
    Ok(workspace.decisions.as_slice())
}

pub struct DecisionOutcome {
    pub valid: bool,
    pub export_range: Range<u64>,
}

pub fn classify_frozen_epoch(
    frozen: &FrozenCaptureEpoch,
    policy: &ResolvedActivityPolicy,
    workspace: &mut DetectorWorkspace,
    decision: &mut FrozenExportDecision,
) -> Result<DecisionOutcome> {
    if decision.valid() {
        return Err(LambError::ExportInvariant(
            "frozen export decision is already final",
        ));
    }
    let range = frozen.absolute_range();
    let channels = frozen.channels() as usize;
    if workspace.scratch.len() < channels {
        return Err(LambError::ExportInvariant(
            "activity workspace has invalid scratch geometry",
        ));
    }
    if policy.channels.len() != channels || decision.channels.len() != channels {
        return Err(LambError::ExportInvariant(
            "activity policy does not match frozen geometry",
        ));
    }
    if frozen.sample_rate() != decision.sample_rate || frozen.sample_rate() == 0 {
        return Err(LambError::ExportInvariant(
            "activity sample rate does not match frozen decision",
        ));
    }
    workspace.reset();
    let geometry = WindowGeometry::new(frozen.sample_rate())?;
    for (channel, config) in policy.channels.iter().enumerate() {
        let threshold = config
            .threshold
            .as_ref()
            .map(|value| value.threshold_dbfs)
            .filter(|value| value.is_finite());
        prepare_state(
            &mut workspace.states.as_mut_slice()[channel],
            policy.detector,
            geometry,
            threshold,
            range.start,
        );
    }
    let mut cursor = range.start;
    while cursor < range.end {
        let copied = frozen
            .copy_interleaved_range_into(cursor..range.end, workspace.scratch.as_mut_slice())?
            as usize;
        if copied == 0 {
            return Err(LambError::ExportInvariant(
                "frozen epoch copy made no progress",
            ));
        }
        for frame in 0..copied {
            for channel in 0..channels {
                let value = workspace.scratch.as_slice()[frame * channels + channel];
                let threshold = policy.channels[channel]
                    .threshold
                    .as_ref()
                    .map(|value| value.threshold_dbfs)
                    .filter(|value| value.is_finite());
                feed_sample(
                    &mut workspace.states.as_mut_slice()[channel],
                    policy.detector,
                    value,
                    cursor - range.start + frame as u64,
                    range.start,
                    geometry,
                    threshold,
                )?;
            }
        }
        cursor = cursor
            .checked_add(copied as u64)
            .ok_or(LambError::ExportInvariant("frozen cursor overflow"))?;
    }
    for (index, config) in policy.channels.iter().enumerate() {
        let threshold = config
            .threshold
            .as_ref()
            .map(|value| value.threshold_dbfs)
            .filter(|value| value.is_finite());
        finish_state(
            &mut workspace.states.as_mut_slice()[index],
            policy.detector,
            range.start,
            geometry,
            threshold,
        )?;
        workspace.decisions.as_mut_slice()[index] =
            channel_decision(config.mode, workspace.states.as_slice()[index]);
    }
    let entire_range_is_finite_zero = workspace
        .states
        .as_slice()
        .iter()
        .all(|state| !state.non_finite && !state.exact_nonzero);
    decision.finalize(
        range,
        workspace.decisions.as_slice(),
        policy.trim_leading_silence,
        policy.whole_export_exact_zero_gate && entire_range_is_finite_zero,
    )?;
    Ok(DecisionOutcome {
        valid: decision.valid(),
        export_range: decision.export_range(),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThresholdSource {
    Manual,
    Calibrated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SilencePolicyPreset {
    AllChannelsExactZero,
    PerChannelExactZero,
}
