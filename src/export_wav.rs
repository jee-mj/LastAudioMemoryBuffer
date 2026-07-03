use crate::error::{io_error, LambError, Result};
use crate::math::wav_parts_for_channel;
use crate::sample_ring::Snapshot;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

pub struct ExportRequest<'a> {
    pub snapshot: &'a Snapshot,
    pub output_dir: &'a Path,
    pub timestamp: &'a str,
    pub split_when_over_bytes: u64,
    pub channel_names: &'a [String],
    pub simple_names: bool,
}

#[derive(Debug, Clone)]
pub struct ExportResult {
    pub files: Vec<PathBuf>,
}

pub fn export_snapshot_wav(request: ExportRequest<'_>) -> Result<ExportResult> {
    fs::create_dir_all(request.output_dir)
        .map_err(|source| io_error(request.output_dir, source))?;
    let mut files = Vec::new();
    for channel in 0..request.snapshot.channels() {
        let samples = request.snapshot.read_channel_samples(channel)?;
        let parts = wav_parts_for_channel(samples.len() as u64, 3, request.split_when_over_bytes)?;
        for (part_index, part) in parts.iter().enumerate() {
            let channel_name = request
                .channel_names
                .get(channel as usize)
                .cloned()
                .unwrap_or_else(|| format!("ch{:02}", channel + 1));
            let final_path = if request.simple_names {
                if parts.len() > 1 {
                    request.output_dir.join(format!(
                        "{}-part{:03}.wav",
                        channel_name,
                        part_index + 1
                    ))
                } else {
                    request.output_dir.join(format!("{channel_name}.wav"))
                }
            } else {
                request.output_dir.join(format!(
                    "lamb-{}-{}-{}Hz-{:09}-{:09}-part{:03}.wav",
                    request.timestamp,
                    channel_name,
                    request.snapshot.sample_rate(),
                    part.start_frame,
                    part.start_frame + part.frame_count,
                    part_index + 1
                ))
            };
            let temp_path = final_path.with_extension("wav.partial");
            let start = part.start_frame as usize;
            let end = (part.start_frame + part.frame_count) as usize;
            write_mono_wav(
                &temp_path,
                &samples[start..end],
                request.snapshot.sample_rate(),
            )?;
            fs::rename(&temp_path, &final_path).map_err(|source| io_error(&final_path, source))?;
            files.push(final_path);
        }
    }
    Ok(ExportResult { files })
}

fn write_u16le(writer: &mut impl Write, value: u16) -> std::io::Result<()> {
    writer.write_all(&value.to_le_bytes())
}

fn write_u32le(writer: &mut impl Write, value: u32) -> std::io::Result<()> {
    writer.write_all(&value.to_le_bytes())
}

fn f32_to_s24_bytes(sample: f32) -> [u8; 3] {
    let clamped = sample.clamp(-1.0, 1.0);
    let scaled = if clamped >= 0.0 {
        (clamped * 8_388_607.0).round() as i32
    } else {
        (clamped * 8_388_608.0).round() as i32
    };
    let bounded = scaled.clamp(-8_388_608, 8_388_607);
    let bytes = bounded.to_le_bytes();
    [bytes[0], bytes[1], bytes[2]]
}

fn write_mono_wav(path: &Path, samples: &[f32], sample_rate: u32) -> Result<()> {
    let data_bytes = (samples.len() as u64)
        .checked_mul(3)
        .ok_or_else(|| LambError::Export("WAV data size overflow".to_string()))?;
    if data_bytes > u64::from(u32::MAX - 36) {
        return Err(LambError::Export(
            "classic WAV data exceeds RIFF size limit".to_string(),
        ));
    }

    let file = File::create(path).map_err(|source| io_error(path, source))?;
    let mut writer = BufWriter::new(file);

    writer
        .write_all(b"RIFF")
        .map_err(|source| io_error(path, source))?;
    write_u32le(&mut writer, 36 + data_bytes as u32).map_err(|source| io_error(path, source))?;
    writer
        .write_all(b"WAVE")
        .map_err(|source| io_error(path, source))?;

    writer
        .write_all(b"fmt ")
        .map_err(|source| io_error(path, source))?;
    write_u32le(&mut writer, 16).map_err(|source| io_error(path, source))?;
    write_u16le(&mut writer, 1).map_err(|source| io_error(path, source))?;
    write_u16le(&mut writer, 1).map_err(|source| io_error(path, source))?;
    write_u32le(&mut writer, sample_rate).map_err(|source| io_error(path, source))?;
    write_u32le(&mut writer, sample_rate * 3).map_err(|source| io_error(path, source))?;
    write_u16le(&mut writer, 3).map_err(|source| io_error(path, source))?;
    write_u16le(&mut writer, 24).map_err(|source| io_error(path, source))?;

    writer
        .write_all(b"data")
        .map_err(|source| io_error(path, source))?;
    write_u32le(&mut writer, data_bytes as u32).map_err(|source| io_error(path, source))?;

    for sample in samples {
        writer
            .write_all(&f32_to_s24_bytes(*sample))
            .map_err(|source| io_error(path, source))?;
    }
    writer.flush().map_err(|source| io_error(path, source))?;
    Ok(())
}
