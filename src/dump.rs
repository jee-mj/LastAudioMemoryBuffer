use std::path::PathBuf;
use std::sync::Mutex;

use crate::error::{LambError, Result};
use crate::sample_ring::{SampleRing, Snapshot};

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DumpOutcome {
    Written {
        range: FrameRange,
        frames: u64,
        lost_frames: u64,
        output_directory: PathBuf,
        files: Vec<PathBuf>,
    },
    SkippedSilent {
        range: FrameRange,
        frames: u64,
        lost_frames: u64,
    },
    NoNewAudio,
}

#[derive(Default)]
struct DumpState {
    committed_until: Option<u64>,
}

pub struct DumpCoordinator {
    state: Mutex<DumpState>,
}

impl DumpCoordinator {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(DumpState::default()),
        }
    }

    pub fn dump<F>(&self, ring: &SampleRing, publisher: F) -> Result<DumpOutcome>
    where
        F: FnOnce(&SampleSnapshot) -> Result<PublishedOutput>,
    {
        let mut state = self
            .state
            .lock()
            .map_err(|_| LambError::Export("dump state lock poisoned".to_string()))?;
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
            return Ok(DumpOutcome::NoNewAudio);
        }

        let snapshot = SampleSnapshot::from_ring_snapshot(ring_snapshot)?;
        let frames = range.end - range.start;

        if snapshot.is_digital_silence() {
            state.committed_until = Some(range.end);
            return Ok(DumpOutcome::SkippedSilent {
                range,
                frames,
                lost_frames,
            });
        }

        let published = publisher(&snapshot)?;
        state.committed_until = Some(range.end);
        Ok(DumpOutcome::Written {
            range,
            frames,
            lost_frames,
            output_directory: published.output_directory,
            files: published.files,
        })
    }
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
