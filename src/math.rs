use crate::error::{LambError, Result};

pub const WAV_SPLIT_DEFAULT_BYTES: u64 = 3_900_000_000;
pub const WAV_HEADER_BYTES: u64 = 44;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WavPart {
    pub start_frame: u64,
    pub frame_count: u64,
}

pub fn estimate_ring_bytes(
    seconds: u32,
    sample_rate: u32,
    channels: u32,
    bytes_per_sample: u32,
    headroom: f64,
) -> Result<u64> {
    if headroom < 1.0 || !headroom.is_finite() {
        return Err(LambError::Validation(
            "headroom must be finite and >= 1.0".to_string(),
        ));
    }
    let frames = u64::from(seconds)
        .checked_mul(u64::from(sample_rate))
        .ok_or_else(|| LambError::Validation("frame count overflow".to_string()))?;
    let raw = frames
        .checked_mul(u64::from(channels))
        .and_then(|v| v.checked_mul(u64::from(bytes_per_sample)))
        .ok_or_else(|| LambError::Validation("ring byte estimate overflow".to_string()))?;
    let with_headroom = (raw as f64) * headroom;
    if with_headroom > u64::MAX as f64 {
        return Err(LambError::Validation(
            "ring byte estimate overflow".to_string(),
        ));
    }
    Ok(with_headroom.ceil() as u64)
}

pub fn derive_chunk_frames(sample_rate: u32, requested: Option<u32>) -> Result<u32> {
    if sample_rate == 0 {
        return Err(LambError::Validation("sampleRate must be > 0".to_string()));
    }
    if let Some(frames) = requested {
        if frames == 0 {
            return Err(LambError::Validation("chunkFrames must be > 0".to_string()));
        }
        return Ok(frames);
    }
    let frames = (u64::from(sample_rate) * 250) / 1000;
    Ok(frames.max(1) as u32)
}

pub fn descriptor_count(seconds: u32, sample_rate: u32, chunk_frames: u32) -> Result<u64> {
    if chunk_frames == 0 {
        return Err(LambError::Validation("chunkFrames must be > 0".to_string()));
    }
    let frames = u64::from(seconds)
        .checked_mul(u64::from(sample_rate))
        .ok_or_else(|| LambError::Validation("descriptor frame count overflow".to_string()))?;
    Ok(frames.div_ceil(u64::from(chunk_frames)))
}

pub fn wav_parts_for_channel(
    total_frames: u64,
    bytes_per_frame: u32,
    split_when_over_bytes: u64,
) -> Result<Vec<WavPart>> {
    if bytes_per_frame == 0 {
        return Err(LambError::Validation(
            "bytes_per_frame must be > 0".to_string(),
        ));
    }
    if split_when_over_bytes <= WAV_HEADER_BYTES {
        return Err(LambError::Validation(
            "splitWhenOverBytes must leave room for WAV data".to_string(),
        ));
    }
    let max_data = split_when_over_bytes - WAV_HEADER_BYTES;
    let frames_per_part = max_data / u64::from(bytes_per_frame);
    if frames_per_part == 0 {
        return Err(LambError::Validation(
            "splitWhenOverBytes is too small for one frame".to_string(),
        ));
    }
    let mut parts = Vec::new();
    let mut start = 0;
    while start < total_frames {
        let remaining = total_frames - start;
        let count = remaining.min(frames_per_part);
        parts.push(WavPart {
            start_frame: start,
            frame_count: count,
        });
        start += count;
    }
    Ok(parts)
}
