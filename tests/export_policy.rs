use lamb::activity::{ActivityDetectorKind, ChannelExportMode};
use lamb::export_policy::{
    preview_export_paths, render_output_into, ChannelActivityPolicy, ExportCommand,
    PublicationStrategy, RenderContext, ResolvedActivityPolicy, ResolvedExportPolicy,
    ResolvedLayout, ValidatedPattern,
};
use std::path::{Path, PathBuf};

fn policy(layout: ResolvedLayout, output_dir: &str, channels: &[&str]) -> ResolvedExportPolicy {
    ResolvedExportPolicy {
        output_dir: PathBuf::from(output_dir),
        layout,
        activity: ResolvedActivityPolicy {
            detector: ActivityDetectorKind::ExactZero,
            channels: channels
                .iter()
                .map(|name| ChannelActivityPolicy {
                    name: (*name).to_string(),
                    mode: ChannelExportMode::Always,
                    threshold: None,
                })
                .collect(),
            whole_export_exact_zero_gate: false,
            trim_leading_silence: false,
        },
    }
}

fn context(command: ExportCommand) -> RenderContext<'static> {
    RenderContext {
        command,
        profile: "studio",
        timestamp: "20260826T130000",
        sample_rate: 48_000,
        export_start_frame: 1_000_000,
        export_end_frame: 1_208_000,
        split_when_over_bytes: 312_044,
        maximum_path_bytes: 512,
    }
}

#[test]
fn parser_accepts_exact_token_set_and_formatter_uses_all_values() {
    let pattern = ValidatedPattern::parse(
        "{timestamp}-{channel}-{sampleRate}-{startFrame}-{endFrame}-{part}-{partSuffix}-{profile}",
    )
    .unwrap();
    let mut rendered = String::new();
    render_output_into(
        &pattern,
        &context(ExportCommand::Recall),
        "left",
        2,
        "-part002",
        1_104_000,
        1_208_000,
        &mut rendered,
    )
    .unwrap();

    assert_eq!(
        rendered,
        "20260826T130000-left-48000-1104000-1208000-2--part002-studio"
    );
}

#[test]
fn custom_preview_renders_nested_paths_absolute_ranges_and_split_metadata() {
    let policy = policy(
        ResolvedLayout::Custom {
            directory_pattern: "{profile}/{channel}/{part}".to_string(),
            filename_pattern: "{timestamp}-{startFrame}-{endFrame}{partSuffix}.wav".to_string(),
        },
        "/exports",
        &["left", "right"],
    );
    let rendered = preview_export_paths(&policy, &context(ExportCommand::Recall), &[1]).unwrap();

    assert_eq!(rendered.len(), 2);
    assert_eq!(rendered[0].channel_index, 1);
    assert_eq!(rendered[0].channel, "right");
    assert_eq!(rendered[0].part, 1);
    assert_eq!(rendered[0].part_count, 2);
    assert_eq!(rendered[0].part_suffix, "-part001");
    assert_eq!(rendered[0].start_frame, 1_000_000);
    assert_eq!(rendered[0].end_frame, 1_104_000);
    assert_eq!(rendered[1].start_frame, 1_104_000);
    assert_eq!(rendered[1].end_frame, 1_208_000);
    assert_eq!(rendered[1].relative_directory, Path::new("studio/right/2"));
    assert_eq!(
        rendered[1].final_path,
        Path::new("/exports/studio/right/2/20260826T130000-1104000-1208000-part002.wav")
    );
}

#[test]
fn unsplit_output_has_one_based_part_and_empty_suffix() {
    let mut context = context(ExportCommand::Recall);
    context.split_when_over_bytes = 1_000_000;
    let policy = policy(
        ResolvedLayout::Custom {
            directory_pattern: String::new(),
            filename_pattern: "{channel}{partSuffix}.wav".to_string(),
        },
        "/exports",
        &["left"],
    );

    let rendered = preview_export_paths(&policy, &context, &[0]).unwrap();
    assert_eq!(rendered[0].part, 1);
    assert_eq!(rendered[0].part_suffix, "");
    assert_eq!(rendered[0].filename, "left.wav");
}

#[test]
fn presets_render_flat_detailed_and_timestamp_directory_paths() {
    let flat = preview_export_paths(
        &policy(ResolvedLayout::FlatDetailed, "/exports", &["left"]),
        &context(ExportCommand::Recall),
        &[0],
    )
    .unwrap();
    assert_eq!(flat[0].relative_directory, Path::new(""));
    assert_eq!(
        flat[0].filename,
        "lamb-20260826T130000-left-48000Hz-001000000-001104000-part001.wav"
    );

    let timestamp = preview_export_paths(
        &policy(ResolvedLayout::TimestampDirectory, "/exports", &["left"]),
        &context(ExportCommand::Recall),
        &[0],
    )
    .unwrap();
    assert_eq!(
        timestamp[0].relative_directory,
        Path::new("20260826T130000")
    );
    assert_eq!(timestamp[0].filename, "left-part001.wav");
}

#[test]
fn publication_strategy_depends_on_effective_layout_not_timestamp_token() {
    assert_eq!(
        ResolvedLayout::TimestampDirectory.publication_strategy(ExportCommand::Recall),
        PublicationStrategy::AtomicDirectory
    );
    assert_eq!(
        ResolvedLayout::Custom {
            directory_pattern: "{timestamp}".to_string(),
            filename_pattern: "{channel}.wav".to_string(),
        }
        .publication_strategy(ExportCommand::Dump),
        PublicationStrategy::FileSet
    );
    assert_eq!(
        ResolvedLayout::CommandDefault.publication_strategy(ExportCommand::Recall),
        PublicationStrategy::FileSet
    );
    assert_eq!(
        ResolvedLayout::CommandDefault.publication_strategy(ExportCommand::Dump),
        PublicationStrategy::AtomicDirectory
    );
}

#[test]
fn command_default_renders_recall_flat_and_dump_timestamp_directory() {
    let policy = policy(ResolvedLayout::CommandDefault, "/exports", &["left"]);
    let recall = preview_export_paths(&policy, &context(ExportCommand::Recall), &[0]).unwrap();
    let dump = preview_export_paths(&policy, &context(ExportCommand::Dump), &[0]).unwrap();

    assert!(recall[0].relative_directory.as_os_str().is_empty());
    assert!(recall[0].filename.starts_with("lamb-20260826T130000-left-"));
    assert_eq!(dump[0].relative_directory, Path::new("20260826T130000"));
    assert_eq!(dump[0].filename, "left-part001.wav");
}

#[test]
fn parser_rejects_malformed_or_unknown_tokens() {
    for pattern in ["{channel", "channel}", "{{channel}}", "{}", "{unknown}"] {
        assert!(
            ValidatedPattern::parse(pattern).is_err(),
            "accepted {pattern:?}"
        );
    }
}

fn custom(directory: &str, filename: &str) -> ResolvedLayout {
    ResolvedLayout::Custom {
        directory_pattern: directory.to_string(),
        filename_pattern: filename.to_string(),
    }
}

#[test]
fn preview_rejects_unsafe_output_roots_directories_and_filenames() {
    for (output, directory, filename) in [
        ("exports", "ok", "a.wav"),
        ("/exports/../escape", "ok", "a.wav"),
        ("/exports", "/absolute", "a.wav"),
        ("/exports", ".", "a.wav"),
        ("/exports", "..", "a.wav"),
        ("/exports", "a/../b", "a.wav"),
        ("/exports", "a//b", "a.wav"),
        ("/exports", "/leading", "a.wav"),
        ("/exports", "trailing/", "a.wav"),
        ("/exports", "nul\0dir", "a.wav"),
        ("/exports", "ok", ""),
        ("/exports", "ok", "a/b.wav"),
        ("/exports", "ok", "nul\0.wav"),
    ] {
        let policy = policy(custom(directory, filename), output, &["left"]);
        assert!(
            preview_export_paths(&policy, &context(ExportCommand::Recall), &[0]).is_err(),
            "accepted output={output:?}, directory={directory:?}, filename={filename:?}"
        );
    }
}

#[test]
fn preview_rejects_raw_dot_components_in_output_root() {
    let mut unsplit = context(ExportCommand::Recall);
    unsplit.split_when_over_bytes = 1_000_000;
    for output_dir in ["/exports/./nested", "/exports/."] {
        let result = preview_export_paths(
            &policy(custom("", "{channel}.wav"), output_dir, &["left"]),
            &unsplit,
            &[0],
        );

        assert!(result.is_err(), "accepted output root {output_dir:?}");
    }
}

#[test]
fn preview_rejects_unsafe_injected_channel_profile_and_timestamp_values() {
    for (channel, profile, timestamp) in [
        ("../left", "studio", "20260826T130000"),
        ("left", "../studio", "20260826T130000"),
        ("left", "studio", "../20260826T130000"),
    ] {
        let policy = policy(
            custom("{profile}", "{channel}-{timestamp}.wav"),
            "/exports",
            &[channel],
        );
        let mut context = context(ExportCommand::Recall);
        context.profile = profile;
        context.timestamp = timestamp;
        assert!(preview_export_paths(&policy, &context, &[0]).is_err());
    }
}

#[test]
fn preview_rejects_path_overflow_and_duplicates() {
    let mut short = context(ExportCommand::Recall);
    short.maximum_path_bytes = 20;
    assert!(preview_export_paths(
        &policy(custom("nested", "{channel}.wav"), "/exports", &["left"]),
        &short,
        &[0]
    )
    .is_err());

    let duplicate = policy(custom("", "same.wav"), "/exports", &["left", "right"]);
    assert!(preview_export_paths(&duplicate, &context(ExportCommand::Recall), &[0, 1]).is_err());
}

#[test]
fn worst_case_path_capacity_includes_the_absolute_output_root() {
    let mut bounded = context(ExportCommand::Recall);
    bounded.maximum_path_bytes = 55;
    let policy = policy(
        custom("", "{startFrame}.wav"),
        "/123456789012345678901234567890",
        &["left"],
    );

    assert!(preview_export_paths(&policy, &bounded, &[0]).is_err());
}

#[test]
fn flat_detailed_capacity_uses_canonical_part_literal() {
    let mut bounded = context(ExportCommand::Recall);
    // By hand: /exports/ (9) + the old discarded pattern's maximum (105).
    // The canonical preset includes the four-byte `part` literal as well.
    bounded.maximum_path_bytes = 114;

    let result = preview_export_paths(
        &policy(ResolvedLayout::FlatDetailed, "/exports", &["left"]),
        &bounded,
        &[0],
    );

    assert!(result.is_err());
}

#[test]
fn preview_checks_collisions_across_all_channel_parts_and_channel_indexes() {
    let collisions = policy(custom("", "{part}.wav"), "/exports", &["left", "right"]);
    assert!(preview_export_paths(&collisions, &context(ExportCommand::Recall), &[0, 1]).is_err());

    assert!(preview_export_paths(
        &policy(custom("", "{channel}.wav"), "/exports", &["left"]),
        &context(ExportCommand::Recall),
        &[1]
    )
    .is_err());
}

#[test]
fn empty_range_validates_retained_channels_without_fabricating_outputs() {
    let valid_policy = policy(custom("", "{channel}.wav"), "/exports", &["left"]);
    let mut empty = context(ExportCommand::Recall);
    empty.export_end_frame = empty.export_start_frame;

    assert!(preview_export_paths(&valid_policy, &empty, &[1]).is_err());
    assert!(preview_export_paths(
        &policy(custom("", "{channel}.wav"), "/exports", &["../left"]),
        &empty,
        &[0],
    )
    .is_err());
    assert_eq!(
        preview_export_paths(&valid_policy, &empty, &[0]).unwrap(),
        vec![]
    );
}
