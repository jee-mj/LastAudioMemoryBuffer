use crate::activity::{ActivityDetectorKind, ChannelExportMode};
use crate::app_config::ActivityThresholdConfig;
use crate::error::{LambError, Result};
use crate::math::wav_parts_for_channel;
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportCommand {
    Recall,
    Dump,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedLayout {
    CommandDefault,
    FlatDetailed,
    TimestampDirectory,
    Custom {
        directory_pattern: ValidatedPattern,
        filename_pattern: ValidatedPattern,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicationStrategy {
    FileSet,
    AtomicDirectory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EffectiveLayout {
    FlatDetailed,
    TimestampDirectory,
    Custom,
}

impl ResolvedLayout {
    fn effective_layout(&self, command: ExportCommand) -> EffectiveLayout {
        match self {
            Self::CommandDefault => match command {
                ExportCommand::Recall => EffectiveLayout::FlatDetailed,
                ExportCommand::Dump => EffectiveLayout::TimestampDirectory,
            },
            Self::FlatDetailed => EffectiveLayout::FlatDetailed,
            Self::TimestampDirectory => EffectiveLayout::TimestampDirectory,
            Self::Custom { .. } => EffectiveLayout::Custom,
        }
    }

    pub fn publication_strategy(&self, command: ExportCommand) -> PublicationStrategy {
        match self.effective_layout(command) {
            EffectiveLayout::TimestampDirectory => PublicationStrategy::AtomicDirectory,
            EffectiveLayout::FlatDetailed | EffectiveLayout::Custom => PublicationStrategy::FileSet,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PatternToken {
    Timestamp,
    Channel,
    SampleRate,
    StartFrame,
    EndFrame,
    Part,
    PartSuffix,
    Profile,
}

impl PatternToken {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "timestamp" => Some(Self::Timestamp),
            "channel" => Some(Self::Channel),
            "sampleRate" => Some(Self::SampleRate),
            "startFrame" => Some(Self::StartFrame),
            "endFrame" => Some(Self::EndFrame),
            "part" => Some(Self::Part),
            "partSuffix" => Some(Self::PartSuffix),
            "profile" => Some(Self::Profile),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PatternSegment {
    Literal(String),
    Token(PatternToken),
    ZeroPaddedToken(PatternToken, usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedPattern {
    segments: Vec<PatternSegment>,
}

impl ValidatedPattern {
    pub fn parse(pattern: &str) -> Result<Self> {
        let mut segments = Vec::new();
        let mut cursor = 0;
        while cursor < pattern.len() {
            let remainder = &pattern[cursor..];
            let next_brace = remainder.find(['{', '}']);
            let Some(offset) = next_brace else {
                segments.push(PatternSegment::Literal(remainder.to_string()));
                break;
            };
            let brace_index = cursor + offset;
            if brace_index > cursor {
                segments.push(PatternSegment::Literal(
                    pattern[cursor..brace_index].to_string(),
                ));
            }
            if pattern.as_bytes()[brace_index] == b'}' {
                return Err(validation("unmatched closing brace in export pattern"));
            }
            let token_start = brace_index + 1;
            let token_remainder = &pattern[token_start..];
            let Some(close_offset) = token_remainder.find('}') else {
                return Err(validation("unmatched opening brace in export pattern"));
            };
            let close_index = token_start + close_offset;
            let token_name = &pattern[token_start..close_index];
            if token_name.contains('{') {
                return Err(validation(
                    "nested braces are not allowed in export patterns",
                ));
            }
            let token = PatternToken::parse(token_name).ok_or_else(|| {
                validation(format!(
                    "unknown or empty export pattern token {{{token_name}}}"
                ))
            })?;
            segments.push(PatternSegment::Token(token));
            cursor = close_index + 1;
        }
        Ok(Self { segments })
    }

    fn contains(&self, wanted: PatternToken) -> bool {
        self.segments.iter().any(|segment| {
            matches!(segment, PatternSegment::Token(token) | PatternSegment::ZeroPaddedToken(token, _) if *token == wanted)
        })
    }

    fn with_zero_padding(mut self, widths: &[(PatternToken, usize)]) -> Self {
        for segment in &mut self.segments {
            if let PatternSegment::Token(token) = segment {
                if let Some((_, width)) = widths.iter().find(|(candidate, _)| candidate == token) {
                    *segment = PatternSegment::ZeroPaddedToken(*token, *width);
                }
            }
        }
        self
    }

    fn maximum_rendered_bytes(&self, context: &RenderContext<'_>, channel: &str) -> Result<u64> {
        self.segments.iter().try_fold(0_u64, |length, segment| {
            let segment_length = match segment {
                PatternSegment::Literal(value) => value.len() as u64,
                PatternSegment::Token(PatternToken::Timestamp) => context.timestamp.len() as u64,
                PatternSegment::Token(PatternToken::Channel) => channel.len() as u64,
                PatternSegment::Token(PatternToken::SampleRate) => 10,
                PatternSegment::Token(PatternToken::StartFrame)
                | PatternSegment::Token(PatternToken::EndFrame)
                | PatternSegment::Token(PatternToken::Part) => 20,
                PatternSegment::Token(PatternToken::PartSuffix) => 25,
                PatternSegment::Token(PatternToken::Profile) => context.profile.len() as u64,
                PatternSegment::ZeroPaddedToken(token, width) => {
                    let ordinary_width = match token {
                        PatternToken::SampleRate => 10,
                        PatternToken::StartFrame | PatternToken::EndFrame | PatternToken::Part => {
                            20
                        }
                        PatternToken::Timestamp => context.timestamp.len() as u64,
                        PatternToken::Channel => channel.len() as u64,
                        PatternToken::PartSuffix => 25,
                        PatternToken::Profile => context.profile.len() as u64,
                    };
                    ordinary_width.max(*width as u64)
                }
            };
            length
                .checked_add(segment_length)
                .ok_or_else(|| validation("export pattern capacity overflow"))
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RenderContext<'a> {
    pub command: ExportCommand,
    pub profile: &'a str,
    pub timestamp: &'a str,
    pub sample_rate: u32,
    pub export_start_frame: u64,
    pub export_end_frame: u64,
    pub split_when_over_bytes: u64,
    pub maximum_path_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedOutput {
    pub channel_index: usize,
    pub channel: String,
    pub part: u64,
    pub part_count: u64,
    pub part_suffix: String,
    pub start_frame: u64,
    pub end_frame: u64,
    pub relative_directory: PathBuf,
    pub filename: String,
    pub final_path: PathBuf,
}

#[allow(clippy::too_many_arguments)]
pub fn render_output_to(
    pattern: &ValidatedPattern,
    context: &RenderContext<'_>,
    channel: &str,
    part: u64,
    part_suffix: &str,
    start_frame: u64,
    end_frame: u64,
    output: &mut impl std::fmt::Write,
) -> Result<()> {
    for segment in &pattern.segments {
        match segment {
            PatternSegment::Literal(value) => output
                .write_str(value)
                .map_err(|_| validation("could not render literal"))?,
            PatternSegment::Token(PatternToken::Timestamp) => {
                output
                    .write_str(context.timestamp)
                    .map_err(|_| validation("could not render timestamp token"))?
            }
            PatternSegment::Token(PatternToken::Channel) => output
                .write_str(channel)
                .map_err(|_| validation("could not render channel token"))?,
            PatternSegment::Token(PatternToken::SampleRate) => {
                write!(output, "{}", context.sample_rate)
                    .map_err(|_| validation("could not render sample-rate token"))?;
            }
            PatternSegment::Token(PatternToken::StartFrame) => {
                write!(output, "{start_frame}")
                    .map_err(|_| validation("could not render start-frame token"))?;
            }
            PatternSegment::Token(PatternToken::EndFrame) => {
                write!(output, "{end_frame}")
                    .map_err(|_| validation("could not render end-frame token"))?;
            }
            PatternSegment::Token(PatternToken::Part) => {
                write!(output, "{part}").map_err(|_| validation("could not render part token"))?;
            }
            PatternSegment::Token(PatternToken::PartSuffix) => output
                .write_str(part_suffix)
                .map_err(|_| validation("could not render part suffix token"))?,
            PatternSegment::Token(PatternToken::Profile) => output
                .write_str(context.profile)
                .map_err(|_| validation("could not render profile token"))?,
            PatternSegment::ZeroPaddedToken(PatternToken::StartFrame, width) => {
                write!(output, "{start_frame:0width$}")
                    .map_err(|_| validation("could not render start-frame token"))?;
            }
            PatternSegment::ZeroPaddedToken(PatternToken::EndFrame, width) => {
                write!(output, "{end_frame:0width$}")
                    .map_err(|_| validation("could not render end-frame token"))?;
            }
            PatternSegment::ZeroPaddedToken(PatternToken::Part, width) => {
                write!(output, "{part:0width$}")
                    .map_err(|_| validation("could not render part token"))?;
            }
            PatternSegment::ZeroPaddedToken(_, _) => {
                return Err(validation("unsupported zero-padded export token"));
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn render_output_into(
    pattern: &ValidatedPattern,
    context: &RenderContext<'_>,
    channel: &str,
    part: u64,
    part_suffix: &str,
    start_frame: u64,
    end_frame: u64,
    output: &mut String,
) -> Result<()> {
    output.clear();
    render_output_to(
        pattern,
        context,
        channel,
        part,
        part_suffix,
        start_frame,
        end_frame,
        output,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn render_policy_output_into(
    policy: &ResolvedExportPolicy,
    context: &RenderContext<'_>,
    channel: &str,
    part: u64,
    part_suffix: &str,
    start_frame: u64,
    end_frame: u64,
    directory: &mut impl std::fmt::Write,
    filename: &mut impl std::fmt::Write,
) -> Result<()> {
    let (directory_pattern, filename_pattern) = policy.patterns(context.command);
    validate_context_values(directory_pattern, filename_pattern, context)?;
    validate_component_value(channel, "channel")?;
    validate_pattern_capacity(
        directory_pattern,
        filename_pattern,
        &policy.output_dir,
        context,
        channel,
    )?;
    render_output_to(
        directory_pattern,
        context,
        channel,
        part,
        part_suffix,
        start_frame,
        end_frame,
        directory,
    )?;
    render_output_to(
        filename_pattern,
        context,
        channel,
        part,
        part_suffix,
        start_frame,
        end_frame,
        filename,
    )?;
    Ok(())
}

pub fn validate_rendered_output_path(
    output_dir: &Path,
    directory: &str,
    filename: &str,
    final_path: &Path,
    maximum_bytes: u64,
) -> Result<()> {
    validate_rendered_directory(directory)?;
    validate_filename(filename)?;
    validate_final_path(output_dir, final_path, maximum_bytes)
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChannelActivityPolicy {
    pub name: String,
    pub mode: ChannelExportMode,
    pub threshold: Option<ActivityThresholdConfig>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedActivityPolicy {
    pub detector: ActivityDetectorKind,
    pub channels: Vec<ChannelActivityPolicy>,
    pub whole_export_exact_zero_gate: bool,
    pub trim_leading_silence: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedExportPolicy {
    pub output_dir: PathBuf,
    pub layout: ResolvedLayout,
    pub activity: ResolvedActivityPolicy,
    patterns: ResolvedPatternSet,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedPatternSet {
    recall_directory: ValidatedPattern,
    recall_filename: ValidatedPattern,
    dump_directory: ValidatedPattern,
    dump_filename: ValidatedPattern,
}

impl ResolvedExportPolicy {
    pub fn new(
        output_dir: PathBuf,
        layout: ResolvedLayout,
        activity: ResolvedActivityPolicy,
    ) -> Result<Self> {
        let (recall_directory, recall_filename) =
            patterns_for_layout(&layout, ExportCommand::Recall)?;
        let (dump_directory, dump_filename) = patterns_for_layout(&layout, ExportCommand::Dump)?;
        Ok(Self {
            output_dir,
            layout,
            activity,
            patterns: ResolvedPatternSet {
                recall_directory,
                recall_filename,
                dump_directory,
                dump_filename,
            },
        })
    }

    fn patterns(&self, command: ExportCommand) -> (&ValidatedPattern, &ValidatedPattern) {
        match command {
            ExportCommand::Recall => (
                &self.patterns.recall_directory,
                &self.patterns.recall_filename,
            ),
            ExportCommand::Dump => (&self.patterns.dump_directory, &self.patterns.dump_filename),
        }
    }
}

pub fn preview_export_paths(
    policy: &ResolvedExportPolicy,
    context: &RenderContext<'_>,
    retained_channels: &[usize],
) -> Result<Vec<RenderedOutput>> {
    validate_output_dir(&policy.output_dir)?;
    if context.export_end_frame < context.export_start_frame {
        return Err(validation("export end frame precedes export start frame"));
    }
    if context.maximum_path_bytes == 0 {
        return Err(validation("maximum path bytes must be nonzero"));
    }

    let (directory_pattern, filename_pattern) = policy.patterns(context.command);
    validate_context_values(directory_pattern, filename_pattern, context)?;
    let total_frames = context.export_end_frame - context.export_start_frame;
    let parts = wav_parts_for_channel(total_frames, 3, context.split_when_over_bytes)?;
    let part_count = u64::try_from(parts.len())
        .map_err(|_| validation("split part count does not fit in u64"))?;
    let output_capacity = retained_channels
        .len()
        .checked_mul(parts.len())
        .ok_or_else(|| validation("rendered output count overflow"))?;
    let mut outputs = Vec::with_capacity(output_capacity);
    let mut final_paths = HashSet::with_capacity(output_capacity);

    for &channel_index in retained_channels {
        let channel = policy
            .activity
            .channels
            .get(channel_index)
            .ok_or_else(|| {
                validation(format!("retained channel index {channel_index} is invalid"))
            })?
            .name
            .as_str();
        validate_component_value(channel, "channel")?;
        validate_pattern_capacity(
            directory_pattern,
            filename_pattern,
            &policy.output_dir,
            context,
            channel,
        )?;

        for (zero_based_part, wav_part) in parts.iter().enumerate() {
            let part = u64::try_from(zero_based_part)
                .ok()
                .and_then(|value| value.checked_add(1))
                .ok_or_else(|| validation("one-based split part overflow"))?;
            let start_frame = context
                .export_start_frame
                .checked_add(wav_part.start_frame)
                .ok_or_else(|| validation("absolute part start frame overflow"))?;
            let end_frame = start_frame
                .checked_add(wav_part.frame_count)
                .ok_or_else(|| validation("absolute part end frame overflow"))?;
            let part_suffix = if part_count == 1 {
                String::new()
            } else {
                format!("-part{part:03}")
            };
            let mut rendered_directory = String::new();
            let mut filename = String::new();
            render_output_into(
                directory_pattern,
                context,
                channel,
                part,
                &part_suffix,
                start_frame,
                end_frame,
                &mut rendered_directory,
            )?;
            render_output_into(
                filename_pattern,
                context,
                channel,
                part,
                &part_suffix,
                start_frame,
                end_frame,
                &mut filename,
            )?;

            validate_rendered_directory(&rendered_directory)?;
            validate_filename(&filename)?;
            let relative_directory = PathBuf::from(&rendered_directory);
            let final_path = policy.output_dir.join(&relative_directory).join(&filename);
            validate_final_path(&policy.output_dir, &final_path, context.maximum_path_bytes)?;
            if !final_paths.insert(final_path.clone()) {
                return Err(validation(format!(
                    "duplicate rendered export path: {}",
                    final_path.display()
                )));
            }
            outputs.push(RenderedOutput {
                channel_index,
                channel: channel.to_string(),
                part,
                part_count,
                part_suffix,
                start_frame,
                end_frame,
                relative_directory,
                filename,
                final_path,
            });
        }
    }

    validate_no_file_parent_conflicts(&outputs)?;
    Ok(outputs)
}

fn patterns_for_layout(
    layout: &ResolvedLayout,
    command: ExportCommand,
) -> Result<(ValidatedPattern, ValidatedPattern)> {
    match layout.effective_layout(command) {
        EffectiveLayout::FlatDetailed => Ok((
            ValidatedPattern::parse("")?,
            ValidatedPattern::parse(
                "lamb-{timestamp}-{channel}-{sampleRate}Hz-{startFrame}-{endFrame}-part{part}.wav",
            )?
            .with_zero_padding(&[
                (PatternToken::StartFrame, 9),
                (PatternToken::EndFrame, 9),
                (PatternToken::Part, 3),
            ]),
        )),
        EffectiveLayout::TimestampDirectory => Ok((
            ValidatedPattern::parse("{timestamp}")?,
            ValidatedPattern::parse("{channel}{partSuffix}.wav")?,
        )),
        EffectiveLayout::Custom => {
            let ResolvedLayout::Custom {
                directory_pattern,
                filename_pattern,
            } = layout
            else {
                return Err(validation("custom layout resolution is inconsistent"));
            };
            Ok((directory_pattern.clone(), filename_pattern.clone()))
        }
    }
}

fn validate_context_values(
    directory_pattern: &ValidatedPattern,
    filename_pattern: &ValidatedPattern,
    context: &RenderContext<'_>,
) -> Result<()> {
    for (token, value, label) in [
        (PatternToken::Timestamp, context.timestamp, "timestamp"),
        (PatternToken::Profile, context.profile, "profile"),
    ] {
        if directory_pattern.contains(token) || filename_pattern.contains(token) {
            validate_component_value(value, label)?;
        }
    }
    Ok(())
}

fn validate_pattern_capacity(
    directory_pattern: &ValidatedPattern,
    filename_pattern: &ValidatedPattern,
    output_dir: &Path,
    context: &RenderContext<'_>,
    channel: &str,
) -> Result<()> {
    let directory_bytes = directory_pattern.maximum_rendered_bytes(context, channel)?;
    let filename_bytes = filename_pattern.maximum_rendered_bytes(context, channel)?;
    let root_separator = u64::from(output_dir != Path::new("/"));
    let directory_separator = u64::from(directory_bytes != 0);
    let maximum_bytes = path_bytes(output_dir)?
        .checked_add(root_separator)
        .and_then(|value| value.checked_add(directory_bytes))
        .and_then(|value| value.checked_add(directory_separator))
        .and_then(|value| value.checked_add(filename_bytes))
        .ok_or_else(|| validation("maximum rendered path capacity overflow"))?;
    if maximum_bytes > context.maximum_path_bytes {
        return Err(validation(format!(
            "worst-case rendered relative path requires {maximum_bytes} bytes, exceeding maximumPathBytes {}",
            context.maximum_path_bytes
        )));
    }
    Ok(())
}

fn validate_output_dir(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        return Err(validation("export outputDir must be absolute"));
    }
    let text = path
        .to_str()
        .ok_or_else(|| validation("export outputDir must be valid UTF-8"))?;
    if text.contains('\0')
        || text.contains("//")
        || (text.len() > 1 && text.ends_with('/'))
        || text
            .split('/')
            .any(|component| matches!(component, "." | ".."))
    {
        return Err(validation("export outputDir must be lexically canonical"));
    }
    for component in path.components() {
        if matches!(
            component,
            Component::CurDir | Component::ParentDir | Component::Prefix(_)
        ) {
            return Err(validation("export outputDir must be lexically canonical"));
        }
    }
    Ok(())
}

fn validate_rendered_directory(directory: &str) -> Result<()> {
    if directory.is_empty() {
        return Ok(());
    }
    if directory.contains('\0')
        || directory.contains('\\')
        || directory.starts_with('/')
        || directory.ends_with('/')
        || directory.contains("//")
    {
        return Err(validation(
            "rendered export directory is not a safe relative path",
        ));
    }
    if directory
        .split('/')
        .any(|component| component.is_empty() || component == "." || component == "..")
    {
        return Err(validation(
            "rendered export directory contains an unsafe component",
        ));
    }
    Ok(())
}

fn validate_filename(filename: &str) -> Result<()> {
    if filename.is_empty()
        || filename == "."
        || filename == ".."
        || filename.contains('/')
        || filename.contains('\\')
        || filename.contains('\0')
    {
        return Err(validation(
            "rendered export filename must be one nonempty normal component",
        ));
    }
    Ok(())
}

fn validate_component_value(value: &str, label: &str) -> Result<()> {
    if value.is_empty()
        || value == "."
        || value == ".."
        || value.contains('/')
        || value.contains('\\')
        || value.contains('\0')
    {
        return Err(validation(format!("unsafe export {label} value")));
    }
    Ok(())
}

fn validate_final_path(output_dir: &Path, final_path: &Path, maximum_bytes: u64) -> Result<()> {
    if !final_path.starts_with(output_dir) {
        return Err(validation("rendered export path escapes outputDir"));
    }
    let length = path_bytes(final_path)?;
    if length > maximum_bytes {
        return Err(validation(format!(
            "rendered export path requires {length} bytes, exceeding maximumPathBytes {maximum_bytes}"
        )));
    }
    Ok(())
}

fn path_bytes(path: &Path) -> Result<u64> {
    let text = path
        .to_str()
        .ok_or_else(|| validation("rendered export path must be valid UTF-8"))?;
    u64::try_from(text.len()).map_err(|_| validation("rendered export path length overflow"))
}

fn validate_no_file_parent_conflicts(outputs: &[RenderedOutput]) -> Result<()> {
    for (index, output) in outputs.iter().enumerate() {
        for other in &outputs[index + 1..] {
            if output.final_path.starts_with(&other.final_path)
                || other.final_path.starts_with(&output.final_path)
            {
                return Err(validation(format!(
                    "rendered export file/parent path conflict between {} and {}",
                    output.final_path.display(),
                    other.final_path.display()
                )));
            }
        }
    }
    Ok(())
}

fn validation(message: impl Into<String>) -> LambError {
    LambError::Validation(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendered(final_path: &str) -> RenderedOutput {
        RenderedOutput {
            channel_index: 0,
            channel: "channel".to_string(),
            part: 1,
            part_count: 1,
            part_suffix: String::new(),
            start_frame: 0,
            end_frame: 1,
            relative_directory: PathBuf::new(),
            filename: "file.wav".to_string(),
            final_path: PathBuf::from(final_path),
        }
    }

    #[test]
    fn file_parent_collision_checker_rejects_both_path_orders() {
        for paths in [
            [rendered("/exports/a"), rendered("/exports/a/b.wav")],
            [rendered("/exports/a/b.wav"), rendered("/exports/a")],
        ] {
            assert!(validate_no_file_parent_conflicts(&paths).is_err());
        }
        assert!(validate_no_file_parent_conflicts(&[
            rendered("/exports/a.wav"),
            rendered("/exports/a.wav.b")
        ])
        .is_ok());
    }
}
