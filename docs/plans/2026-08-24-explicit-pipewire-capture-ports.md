# Explicit PipeWire Capture Ports Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Require ordered explicit PipeWire source ports, link those ports to one multichannel capture stream in that order, and preserve configured names for WAV exports.

**Architecture:** Configuration validation normalizes ordered `{ source, name }` selections before capture. PipeWire discovery resolves exact source-port globals on the selected node; the capture thread creates an inactive multichannel stream, orders its input ports by `port.id`, creates explicit non-lingering links, and activates only after link creation succeeds.

**Tech Stack:** Rust 2021, serde/TOML, pipewire-rs 0.10, Cargo integration tests, live PipeWire CLI smoke checks.

**Spec:** `docs/specs/2026-08-24-explicit-pipewire-capture-ports-design.md`

## Global Constraints

- PipeWire source matching is exact against normalized `port.name`.
- `capturePorts` order is capture-channel order; `name` is the WAV/output label.
- PipeWire must never fall back to `AUTOCONNECT`.
- Legacy PipeWire rejects field presence of `channels` and `channelMap`, including `channelMap = []`.
- Profile PipeWire rejects field presence of `pipewire.channelMap`, including an empty array.
- JACK and fake-backend behavior remain unchanged.
- Do not add dependencies or upgrade `pipewire = "0.10"`.
- Do not commit, amend, push, or otherwise modify repository history unless the user explicitly requests it.
- Keep the realtime process callback allocation-free and free of blocking or graph-management calls.

## File Map

- `src/config.rs`: legacy schema, field-presence tracking, port normalization, static validation.
- `src/app_config.rs`: profile-level `pipewire.capturePorts` schema and old-field presence tracking.
- `src/profile.rs`: PipeWire profile validation and ordered `ResolvedCapturePort` construction.
- `src/capture_pipewire.rs`: backend contract, graph models, port resolution, link planning, explicit-link runtime.
- `src/daemon.rs`: derive session channel names from configured PipeWire labels.
- `tests/config_validation.rs`: legacy required/invalid/conflict tests.
- `tests/app_config.rs`: profile parsing and resolution tests.
- `tests/pipewire_backend.rs`: synthetic graph resolution, link planning, and conversion tests.
- `README.md`: required schemas and Pro Audio migration guidance.
- `~/.config/lamb/lamb.toml`: migrate the current Scarlett configuration after code verification.

---

### Task 1: Legacy Explicit-Port Configuration Contract

**Files:**
- Modify: `src/config.rs:6-155`
- Modify: `src/capture_pipewire.rs:33-53,239-278`
- Modify: `src/daemon.rs:217-249`
- Modify: `src/profile.rs:57-90` (temporary compile-compatible construction using the current profile fields)
- Test: `tests/config_validation.rs`
- Test: `tests/pipewire_backend.rs:13-35,195-238`

**Interfaces:**
- Produces: `CapturePortConfig`, `ConfiguredCapturePort`, and `LambConfig::resolved_capture_ports() -> Result<Vec<ConfiguredCapturePort>>`.
- Produces: `PipeWireCaptureConfig { capture_ports: Vec<ConfiguredCapturePort>, ... }`.
- Produces: `PipeWireCaptureConfig::channel_count() -> Result<u32>` and `channel_names() -> Vec<String>`.
- Consumes: existing `LambError::Validation` and existing serde camelCase conventions.

- [ ] **Step 1: Update the legacy test fixture and add failing validation tests**

Change `valid_config()` so fake configuration represents optional legacy fields explicitly:

```rust
use lamb::config::{
    CapturePortConfig, ExportConfig, LambConfig, MemoryConfig,
};

// In valid_config():
channels: Some(4),
channel_map: Some(Vec::new()),
capture_ports: Vec::new(),
```

Add a PipeWire fixture and focused tests:

```rust
fn capture_port(source: &str, name: &str) -> CapturePortConfig {
    CapturePortConfig {
        source: Some(source.to_string()),
        name: Some(name.to_string()),
    }
}

fn valid_pipewire_config() -> LambConfig {
    let mut cfg = valid_config();
    cfg.backend = "pipewire".to_string();
    cfg.target = Some("studio-input".to_string());
    cfg.channels = None;
    cfg.channel_map = None;
    cfg.capture_ports = vec![
        capture_port("capture_AUX0", "mic"),
        capture_port("capture_AUX1", "gtr"),
    ];
    cfg
}

#[test]
fn pipewire_requires_capture_ports() {
    let mut cfg = valid_pipewire_config();
    cfg.capture_ports.clear();
    assert!(cfg
        .validate_static()
        .unwrap_err()
        .to_string()
        .contains("capturePorts is required for pipewire backend"));
}

#[test]
fn pipewire_rejects_missing_blank_and_duplicate_port_fields() {
    let mut missing = valid_pipewire_config();
    missing.capture_ports[0].source = None;
    assert!(missing.validate_static().unwrap_err().to_string().contains(
        "capturePorts[0].source is required"
    ));

    let mut blank = valid_pipewire_config();
    blank.capture_ports[0].name = Some("  ".to_string());
    assert!(blank.validate_static().unwrap_err().to_string().contains(
        "capturePorts[0].name is required"
    ));

    let mut duplicate_source = valid_pipewire_config();
    duplicate_source.capture_ports[1].source = Some(" capture_AUX0 ".to_string());
    assert!(duplicate_source.validate_static().unwrap_err().to_string().contains(
        "capturePorts[1].source duplicates capturePorts[0].source"
    ));

    let mut duplicate_name = valid_pipewire_config();
    duplicate_name.capture_ports[1].name = Some(" mic ".to_string());
    assert!(duplicate_name.validate_static().unwrap_err().to_string().contains(
        "capturePorts[1].name duplicates capturePorts[0].name"
    ));
}

#[test]
fn pipewire_rejects_legacy_fields_by_presence() {
    let mut channels = valid_pipewire_config();
    channels.channels = Some(2);
    assert!(channels.validate_static().unwrap_err().to_string().contains(
        "channels conflicts with capturePorts"
    ));

    let mut empty_map = valid_pipewire_config();
    empty_map.channel_map = Some(Vec::new());
    assert!(empty_map.validate_static().unwrap_err().to_string().contains(
        "channelMap conflicts with capturePorts"
    ));
}

#[test]
fn pipewire_ports_derive_ordered_channels_and_names() {
    let cfg = valid_pipewire_config();
    let ports = cfg.resolved_capture_ports().unwrap();
    assert_eq!(ports.len(), 2);
    assert_eq!(ports[0].source, "capture_AUX0");
    assert_eq!(ports[0].name, "mic");
    assert_eq!(ports[1].source, "capture_AUX1");
    assert_eq!(ports[1].name, "gtr");
}
```

- [ ] **Step 2: Run the legacy tests and confirm they fail for the missing schema**

Run: `cargo test --test config_validation`

Expected: compilation fails because `CapturePortConfig`, `capture_ports`, and optional `channel_map` do not yet exist.

- [ ] **Step 3: Add raw and normalized legacy port models**

In `src/config.rs`, add:

```rust
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapturePortConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredCapturePort {
    pub source: String,
    pub name: String,
}
```

Change `LambConfig` fields to preserve legacy-field presence:

```rust
pub channels: Option<u32>,
#[serde(rename = "channelMap", default, skip_serializing_if = "Option::is_none")]
pub channel_map: Option<Vec<String>>,
#[serde(rename = "capturePorts", default, skip_serializing_if = "Vec::is_empty")]
pub capture_ports: Vec<CapturePortConfig>,
```

- [ ] **Step 4: Implement deterministic normalization and duplicate reporting**

Add to `impl LambConfig`:

```rust
pub fn resolved_capture_ports(&self) -> Result<Vec<ConfiguredCapturePort>> {
    if self.capture_ports.is_empty() {
        return Err(LambError::Validation(
            "capturePorts is required for pipewire backend".to_string(),
        ));
    }

    let mut source_indexes = BTreeMap::<String, usize>::new();
    let mut name_indexes = BTreeMap::<String, usize>::new();
    let mut resolved = Vec::with_capacity(self.capture_ports.len());

    for (index, port) in self.capture_ports.iter().enumerate() {
        let source = port
            .source
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                LambError::Validation(format!(
                    "capturePorts[{index}].source is required"
                ))
            })?
            .to_string();
        let name = port
            .name
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                LambError::Validation(format!(
                    "capturePorts[{index}].name is required"
                ))
            })?
            .to_string();

        if let Some(first) = source_indexes.insert(source.clone(), index) {
            return Err(LambError::Validation(format!(
                "capturePorts[{index}].source duplicates capturePorts[{first}].source"
            )));
        }
        if let Some(first) = name_indexes.insert(name.clone(), index) {
            return Err(LambError::Validation(format!(
                "capturePorts[{index}].name duplicates capturePorts[{first}].name"
            )));
        }
        resolved.push(ConfiguredCapturePort { source, name });
    }
    Ok(resolved)
}
```

In `validate_static`, branch legacy channel validation by backend:

```rust
if self.backend == "pipewire" {
    if self.channels.is_some() {
        return Err(LambError::Validation(
            "channels conflicts with capturePorts for pipewire backend".to_string(),
        ));
    }
    if self.channel_map.is_some() {
        return Err(LambError::Validation(
            "channelMap conflicts with capturePorts for pipewire backend".to_string(),
        ));
    }
    self.resolved_capture_ports()?;
} else if let Some(channels) = self.channels {
    if channels == 0 {
        return Err(LambError::Validation("channels must be > 0".to_string()));
    }
    if let Some(channel_map) = self.channel_map.as_ref() {
        if !channel_map.is_empty() && channel_map.len() != channels as usize {
            return Err(LambError::Validation(format!(
                "channelMap length {} must match channels {}",
                channel_map.len(), channels
            )));
        }
    }
}
```

- [ ] **Step 5: Replace the backend's legacy channel fields with normalized ports**

In `src/capture_pipewire.rs`, change the contract:

```rust
use crate::config::{ConfiguredCapturePort, LambConfig};

pub struct PipeWireCaptureConfig {
    pub target: Option<String>,
    pub capture_ports: Vec<ConfiguredCapturePort>,
    pub sample_rate: u32,
    pub dont_remix: bool,
    pub latency: Option<String>,
}

impl PipeWireCaptureConfig {
    pub fn from_lamb_config(cfg: &LambConfig) -> Result<Self> {
        Ok(Self {
            target: cfg.target.clone(),
            capture_ports: cfg.resolved_capture_ports()?,
            sample_rate: cfg.sample_rate,
            dont_remix: cfg.dont_remix,
            latency: cfg.latency.clone(),
        })
    }

    pub fn channel_count(&self) -> Result<u32> {
        u32::try_from(self.capture_ports.len()).map_err(|_| {
            LambError::Validation("capturePorts exceeds supported channel count".to_string())
        })
    }

    pub fn channel_names(&self) -> Vec<String> {
        self.capture_ports
            .iter()
            .map(|port| port.name.clone())
            .collect()
    }
}
```

Change `resolved_from_node` to use `cfg.channel_count()?` and remove all
`channel_map` checks. The selected target's `audio.channels` must not override
the explicit count.

- [ ] **Step 6: Keep profile construction compiling until Task 2 replaces its behavior**

In `validate_pipewire_profile`, build its current synthesized `ports` once,
clone that into `ResolvedProfile`, and map it into backend selections:

```rust
let ports: Vec<ResolvedCapturePort> = pw
    .channel_map
    .iter()
    .enumerate()
    .map(|(index, name)| ResolvedCapturePort {
        source: format!("pipewire-input-ch{}", index + 1),
        name: name.clone(),
    })
    .collect();

capture_ports: ports
    .iter()
    .map(|port| ConfiguredCapturePort {
        source: port.source.clone(),
        name: port.name.clone(),
    })
    .collect(),
```

This is only a compile bridge; Task 2 replaces the synthesized sources with
`pipewire.capturePorts` before the feature is considered complete.

- [ ] **Step 7: Feed normalized names through legacy daemon startup**

In `run_capture_config`:

```rust
// Fake branch:
let channel_names = cfg.channel_map.clone().unwrap_or_default();

// PipeWire branch:
let pipewire_cfg = PipeWireCaptureConfig::from_lamb_config(&cfg)?;
let channel_names = pipewire_cfg.channel_names();
let resolved = crate::capture_pipewire::resolve_target(&pipewire_cfg)?;
```

Keep `cfg.channels = Some(resolved.channels)` after validation so existing
legacy status reporting continues to expose the effective runtime count.

- [ ] **Step 8: Update backend conversion tests**

Construct a legacy config with `channels: None`, `channel_map: None`, and four
`CapturePortConfig` entries. Assert:

```rust
let pipewire_cfg = PipeWireCaptureConfig::from_lamb_config(&lamb_cfg).unwrap();
assert_eq!(pipewire_cfg.channel_count().unwrap(), 4);
assert_eq!(
    pipewire_cfg
        .capture_ports
        .iter()
        .map(|port| (port.source.as_str(), port.name.as_str()))
        .collect::<Vec<_>>(),
    vec![
        ("capture_AUX0", "mic"),
        ("capture_AUX1", "gtr"),
        ("capture_AUX2", "percL"),
        ("capture_AUX3", "percR"),
    ]
);
```

- [ ] **Step 9: Run focused tests and formatting**

Run:

```bash
cargo fmt --check
cargo test --test config_validation
cargo test --test pipewire_backend pipewire_capture_config_is_derived_from_lamb_config
```

Expected: all commands pass; fake validation still accepts its existing fields.

---

### Task 2: Profile PipeWire Port Schema and Resolution

**Files:**
- Modify: `src/app_config.rs:45-77`
- Modify: `src/profile.rs:57-90,237-306`
- Test: `tests/app_config.rs`

**Interfaces:**
- Consumes: `ConfiguredCapturePort` and `PipeWireCaptureConfig.capture_ports` from Task 1.
- Produces: `PipewireProfileConfig.capture_ports: Vec<CapturePort>` serialized as `capturePorts`.
- Produces: strict `PipewireProfileConfig.channel_map: Option<Vec<String>>` presence tracking.
- Produces: ordered `ResolvedProfile.ports` for PipeWire.

- [ ] **Step 1: Add a complete profile TOML fixture and failing parsing/resolution test**

In `tests/app_config.rs`, import `parse_config_text` and `lamb::profile`, then add:

```rust
fn pipewire_profile_text(extra: &str) -> String {
    format!(
        r#"
[daemon]
startMode = "manual"
activeProfile = "scarlett"

[profiles.scarlett]
backend = "pipewire"

[profiles.scarlett.pipewire]
target = "studio-input"
capturePorts = [
  {{ source = "capture_AUX2", name = "percL" }},
  {{ source = "capture_AUX0", name = "mic" }},
]
{extra}

[profiles.scarlett.buffer]
seconds = 10

[profiles.scarlett.export]
outputDir = "/tmp/lamb-profile"
mode = "per-channel"
format = "wav"
"#
    )
}

#[test]
fn pipewire_capture_ports_parse_and_resolve_in_order() {
    let cfg = parse_config_text(
        std::path::Path::new("profile.toml"),
        &pipewire_profile_text(""),
    )
    .unwrap();
    let resolved = profile::resolve_active_profile(&cfg)
        .unwrap()
        .expect("active profile");

    assert_eq!(resolved.ports[0].source, "capture_AUX2");
    assert_eq!(resolved.ports[0].name, "percL");
    assert_eq!(resolved.ports[1].source, "capture_AUX0");
    assert_eq!(resolved.ports[1].name, "mic");
    assert_eq!(
        resolved.pipewire_config.unwrap().channel_names(),
        vec!["percL".to_string(), "mic".to_string()]
    );
}
```

- [ ] **Step 2: Add failing profile validation cases**

Add tests that mutate the parsed `ProfileConfig` directly so every failure is
isolated:

```rust
#[test]
fn pipewire_profile_rejects_omitted_blank_duplicate_and_legacy_ports() {
    let cfg = parse_config_text(
        std::path::Path::new("profile.toml"),
        &pipewire_profile_text(""),
    )
    .unwrap();
    let base = cfg.profiles.get("scarlett").unwrap().clone();

    let mut omitted = base.clone();
    omitted.pipewire.capture_ports.clear();
    assert!(profile::validate_profile("scarlett", &omitted)
        .unwrap_err()
        .to_string()
        .contains("pipewire.capturePorts is required"));

    let mut duplicate = base.clone();
    duplicate.pipewire.capture_ports[1].name = Some(" percL ".to_string());
    assert!(profile::validate_profile("scarlett", &duplicate)
        .unwrap_err()
        .to_string()
        .contains("pipewire.capturePorts[1].name duplicates pipewire.capturePorts[0].name"));

    let mut old_map = base.clone();
    old_map.pipewire.channel_map = Some(Vec::new());
    assert!(profile::validate_profile("scarlett", &old_map)
        .unwrap_err()
        .to_string()
        .contains("pipewire.channelMap conflicts with pipewire.capturePorts"));

    let mut jack_fields = base;
    jack_fields.capture.sources = vec!["system:capture_1".to_string()];
    assert!(profile::validate_profile("scarlett", &jack_fields)
        .unwrap_err()
        .to_string()
        .contains("capture.ports and capture.sources are only valid for jack profiles"));
}
```

Also cover `source = " "`, `name = None`, duplicate normalized source, and a
non-empty generic `capture.ports` entry in separate assertions or tests.

- [ ] **Step 3: Run profile tests and verify the new schema is absent**

Run: `cargo test --test app_config`

Expected: compilation fails because `PipewireProfileConfig.capture_ports` and
optional `channel_map` do not exist.

- [ ] **Step 4: Add profile-level camelCase fields**

In `PipewireProfileConfig`:

```rust
#[serde(rename = "capturePorts", default, skip_serializing_if = "Vec::is_empty")]
pub capture_ports: Vec<CapturePort>,
#[serde(
    rename = "channelMap",
    default,
    skip_serializing_if = "Option::is_none"
)]
pub channel_map: Option<Vec<String>>,
```

Keep `CapturePort`'s optional `source` and `name` fields so profile validation
can produce indexed errors rather than generic TOML deserialization errors.

- [ ] **Step 5: Implement PipeWire profile resolution without synthesized ports**

Add a dedicated helper in `src/profile.rs`:

```rust
fn resolve_pipewire_capture_ports(
    profile_name: &str,
    profile: &ProfileConfig,
) -> Result<Vec<ResolvedCapturePort>> {
    let ports = &profile.pipewire.capture_ports;
    if ports.is_empty() {
        return Err(LambError::Validation(format!(
            "profile {profile_name}: pipewire.capturePorts is required"
        )));
    }

    let mut source_indexes = std::collections::BTreeMap::<String, usize>::new();
    let mut name_indexes = std::collections::BTreeMap::<String, usize>::new();
    let mut resolved = Vec::with_capacity(ports.len());
    for (index, port) in ports.iter().enumerate() {
        let source = required_string(
            &format!("pipewire.capturePorts[{index}].source"),
            port.source.as_deref(),
        )?;
        let name = required_string(
            &format!("pipewire.capturePorts[{index}].name"),
            port.name.as_deref(),
        )?;
        if let Some(first) = source_indexes.insert(source.clone(), index) {
            return Err(LambError::Validation(format!(
                "pipewire.capturePorts[{index}].source duplicates pipewire.capturePorts[{first}].source"
            )));
        }
        if let Some(first) = name_indexes.insert(name.clone(), index) {
            return Err(LambError::Validation(format!(
                "pipewire.capturePorts[{index}].name duplicates pipewire.capturePorts[{first}].name"
            )));
        }
        resolved.push(ResolvedCapturePort { source, name });
    }
    Ok(resolved)
}
```

At the beginning of `validate_pipewire_profile`, reject conflicts:

```rust
if pw.channel_map.is_some() {
    return Err(LambError::Validation(format!(
        "profile {name}: pipewire.channelMap conflicts with pipewire.capturePorts"
    )));
}
if !profile.capture.ports.is_empty() || !profile.capture.sources.is_empty() {
    return Err(LambError::Validation(format!(
        "profile {name}: capture.ports and capture.sources are only valid for jack profiles"
    )));
}
let ports = resolve_pipewire_capture_ports(name, profile)?;
```

Use `ports.clone()` in `ResolvedProfile` and map it to backend selections:

```rust
capture_ports: ports
    .iter()
    .map(|port| ConfiguredCapturePort {
        source: port.source.clone(),
        name: port.name.clone(),
    })
    .collect(),
```

Delete the `pipewire-input-chN` synthesis entirely. Leave JACK's existing
`resolve_capture_ports` path unchanged.

- [ ] **Step 6: Run focused profile and legacy tests**

Run:

```bash
cargo fmt --check
cargo test --test app_config
cargo test --test config_validation
```

Expected: all pass, including exact configured profile names and ordering.

---

### Task 3: Synthetic PipeWire Graph Discovery and Source Resolution

**Files:**
- Modify: `src/capture_pipewire.rs:9-18,55-66,156-184,239-381`
- Test: `tests/pipewire_backend.rs:13-95,241-255`

**Interfaces:**
- Consumes: ordered `PipeWireCaptureConfig.capture_ports` from Tasks 1 and 2.
- Produces: `AvailablePort`, `ResolvedSourcePort`, and `ResolvedTarget.source_ports`.
- Produces: `resolve_target_from_graph(cfg, nodes, ports) -> Result<ResolvedTarget>`.
- Produces internally: `AvailableGraph { nodes, ports, link_factory }` from one registry roundtrip.

- [ ] **Step 1: Replace target-only fixtures with node-and-port graph fixtures**

In `tests/pipewire_backend.rs`, import the new API and define:

```rust
fn port(
    id: u32,
    node_id: u32,
    port_id: u32,
    direction: &str,
    name: &str,
    format_dsp: Option<&str>,
) -> AvailablePort {
    AvailablePort {
        id,
        object_type: "PipeWire:Interface:Port".to_string(),
        node_id: Some(node_id),
        direction: Some(direction.to_string()),
        port_id: Some(port_id),
        name: Some(name.to_string()),
        format_dsp: format_dsp.map(str::to_string),
        audio_format: None,
    }
}
```

Make `cfg()` select two ports in a deliberately different order from the
synthetic registry:

```rust
capture_ports: vec![
    ConfiguredCapturePort {
        source: "capture_AUX2".to_string(),
        name: "percL".to_string(),
    },
    ConfiguredCapturePort {
        source: "capture_AUX0".to_string(),
        name: "mic".to_string(),
    },
],
```

- [ ] **Step 2: Add a failing successful-order test**

```rust
#[test]
fn explicit_sources_resolve_in_configured_order() {
    let source = node(72, "PipeWire:Interface:Node", "Audio/Source", "studio-input");
    let registry_ports = vec![
        port(113, 72, 0, "out", "capture_AUX0", Some("32 bit float mono audio")),
        port(115, 72, 2, "out", "capture_AUX2", Some("32 bit float mono audio")),
    ];

    let resolved = resolve_target_from_graph(
        &cfg(Some("studio-input")),
        &[source],
        &registry_ports,
    )
    .unwrap();

    assert_eq!(resolved.channels, 2);
    assert_eq!(resolved.source_ports[0].global_id, 115);
    assert_eq!(resolved.source_ports[0].name, "capture_AUX2");
    assert_eq!(resolved.source_ports[1].global_id, 113);
    assert_eq!(resolved.source_ports[1].name, "capture_AUX0");
}
```

- [ ] **Step 3: Add each independent resolver failure**

Add tests with exact message fragments for:

```rust
// Missing on every node:
"source port 'capture_AUX2' was not found on target 'studio-input'"

// Existing only on node 99:
"source port 'capture_AUX2' belongs to node 'other-input', expected 'studio-input'"

// Matching selected-node port has `direction = in`:
"source port 'capture_AUX2' has direction 'in'; expected 'out'"

// Matching selected-node port has MIDI DSP metadata:
"source port 'capture_AUX2' is not audio (format.dsp='8 bit raw midi')"

// Two selected-node ports share the exact name:
"source port 'capture_AUX2' is ambiguous on target 'studio-input'"

// Programmatically constructed config repeats a source:
"capture source 'capture_AUX2' is configured more than once"
```

The missing-port assertion must also verify that available target audio outputs
are listed in sorted name order.

- [ ] **Step 4: Run the backend test and confirm graph APIs are missing**

Run: `cargo test --test pipewire_backend`

Expected: compilation fails for `AvailablePort`, `ResolvedSourcePort`,
`source_ports`, and `resolve_target_from_graph`.

- [ ] **Step 5: Add graph and resolved-port models**

In `src/capture_pipewire.rs`:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvailablePort {
    pub id: u32,
    pub object_type: String,
    pub node_id: Option<u32>,
    pub direction: Option<String>,
    pub port_id: Option<u32>,
    pub name: Option<String>,
    pub format_dsp: Option<String>,
    pub audio_format: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSourcePort {
    pub global_id: u32,
    pub node_id: u32,
    pub port_id: u32,
    pub name: String,
}

// Add to ResolvedTarget:
pub source_ports: Vec<ResolvedSourcePort>,

#[derive(Debug, Clone, Default)]
struct AvailableGraph {
    nodes: Vec<AvailableNode>,
    ports: Vec<AvailablePort>,
    link_factory: Option<String>,
}
```

- [ ] **Step 6: Implement exact source resolution as a pure function**

Add helpers with these signatures:

```rust
fn is_audio_port(port: &AvailablePort) -> bool;
fn node_display_name(nodes: &[AvailableNode], node_id: u32) -> String;
fn resolve_source_ports(
    cfg: &PipeWireCaptureConfig,
    selected: &AvailableNode,
    nodes: &[AvailableNode],
    ports: &[AvailablePort],
) -> Result<Vec<ResolvedSourcePort>>;

pub fn resolve_target_from_graph(
    cfg: &PipeWireCaptureConfig,
    nodes: &[AvailableNode],
    ports: &[AvailablePort],
) -> Result<ResolvedTarget>;
```

`is_audio_port` must implement the spec exactly:

```rust
fn is_audio_port(port: &AvailablePort) -> bool {
    port.format_dsp
        .as_deref()
        .map(str::trim)
        .map(|format| {
            format
                .split_whitespace()
                .last()
                .is_some_and(|word| word.eq_ignore_ascii_case("audio"))
        })
        .unwrap_or(false)
        || port
            .audio_format
            .as_deref()
            .is_some_and(|format| !format.trim().is_empty())
}
```

Resolve each configured source sequentially, find selected-node matches before
reporting wrong-node matches, validate object type/direction/format/IDs, and
append to the result without sorting. Pass those ports into `resolved_from_node`
and derive `channels` from their length.

- [ ] **Step 7: Extend registry discovery to nodes, ports, removals, and link factory**

Replace `discover_available_nodes()` with `discover_available_graph()`. In the
registry global callback:

```rust
match global.type_.to_str() {
    "PipeWire:Interface:Node" => graph.nodes.push(available_node_from_global(global)),
    "PipeWire:Interface:Port" => graph.ports.push(available_port_from_global(global)),
    "PipeWire:Interface:Factory" => {
        let props = global.props.as_ref().map(|props| props.as_ref());
        if string_prop(props, "factory.type.name").as_deref()
            == Some(pipewire::types::ObjectType::Link.to_str())
        {
            graph.link_factory = string_prop(props, "factory.name");
        }
    }
    _ => {}
}
```

Parse ports with:

```rust
AvailablePort {
    id: global.id,
    object_type: global.type_.to_string(),
    node_id: u32_prop(props, *pipewire::keys::NODE_ID),
    direction: string_prop(props, *pipewire::keys::PORT_DIRECTION),
    port_id: u32_prop(props, *pipewire::keys::PORT_ID),
    name: string_prop(props, *pipewire::keys::PORT_NAME),
    format_dsp: string_prop(props, *pipewire::keys::FORMAT_DSP),
    audio_format: string_prop(props, *pipewire::keys::AUDIO_FORMAT),
}
```

Add `global_remove` handling that removes matching node/port IDs. Change
`resolve_target()` to call `resolve_target_from_graph()` with the completed
snapshot.

- [ ] **Step 8: Update target and log tests for explicit channels**

Every target-selection test must provide matching ports. Update
`ResolvedTarget` literals with `source_ports`. Keep log output stable except
that the channel count now reflects selected ports rather than the node's full
`audio.channels` property.

- [ ] **Step 9: Run focused tests**

Run:

```bash
cargo fmt --check
cargo test --test pipewire_backend
cargo test --test config_validation
cargo test --test app_config
```

Expected: all pass; source result order follows configuration, not registry
order.

---

### Task 4: Explicit Destination Ordering and PipeWire Link Runtime

**Files:**
- Modify: `src/capture_pipewire.rs:81-153,310-518`
- Test: `tests/pipewire_backend.rs`

**Interfaces:**
- Consumes: `ResolvedTarget.source_ports`, `AvailablePort`, and discovered link factory from Task 3.
- Produces: `PlannedLink` and `plan_explicit_links(...) -> Result<Vec<PlannedLink>>`.
- Produces: runtime startup with no `target.object` and no `AUTOCONNECT`.
- Preserves: existing interleaved F32LE process callback and `CaptureIngress` API.

- [ ] **Step 1: Add failing destination-order and link-plan tests**

Expose a pure plan model:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedLink {
    pub output_node_id: u32,
    pub output_port_id: u32,
    pub input_node_id: u32,
    pub input_port_id: u32,
}
```

Add:

```rust
#[test]
fn explicit_link_plan_pairs_sources_with_stream_ports_by_numeric_port_id() {
    let sources = vec![
        ResolvedSourcePort {
            global_id: 115,
            node_id: 72,
            port_id: 2,
            name: "capture_AUX2".to_string(),
        },
        ResolvedSourcePort {
            global_id: 113,
            node_id: 72,
            port_id: 0,
            name: "capture_AUX0".to_string(),
        },
    ];
    let stream_ports = vec![
        port(902, 900, 1, "in", "input_FR", Some("32 bit float mono audio")),
        port(901, 900, 0, "in", "input_FL", Some("32 bit float mono audio")),
    ];

    let links = plan_explicit_links(&sources, 900, &stream_ports).unwrap();
    assert_eq!(links[0].output_port_id, 115);
    assert_eq!(links[0].input_port_id, 901);
    assert_eq!(links[1].output_port_id, 113);
    assert_eq!(links[1].input_port_id, 902);
}
```

Add failures for too few/many stream ports, duplicate destination `port.id`,
missing destination `port.id`, wrong direction, and non-audio destination.

- [ ] **Step 2: Run the plan tests and verify they fail**

Run: `cargo test --test pipewire_backend explicit_link_plan`

Expected: compilation fails because `PlannedLink` and
`plan_explicit_links` do not exist.

- [ ] **Step 3: Implement pure destination ordering and link planning**

Add:

```rust
pub fn plan_explicit_links(
    sources: &[ResolvedSourcePort],
    stream_node_id: u32,
    ports: &[AvailablePort],
) -> Result<Vec<PlannedLink>>;
```

Implementation requirements:

1. Select only ports owned by `stream_node_id`.
2. Require `direction == "in"` and `is_audio_port(port)`.
3. Require every selected port to have `port_id`.
4. Sort by `port_id`, then reject duplicate node-local IDs.
5. Require destination count exactly equal to `sources.len()`.
6. Zip sources in their existing order with sorted destinations.
7. Put global IDs, not node-local IDs, in `output_port_id` and
   `input_port_id`.

- [ ] **Step 4: Register a persistent graph listener on the capture thread**

Refactor discovery setup into a helper usable by both one-shot resolution and
runtime startup. The runtime form must return the registry listener plus
`Rc<RefCell<AvailableGraph>>`, because the listener must survive stream and link
creation:

```rust
fn register_graph_listener(
    registry: &pipewire::registry::RegistryBox<'_>,
) -> (
    Rc<RefCell<AvailableGraph>>,
    pipewire::registry::Listener,
);
```

Keep the existing core synchronization pattern as a reusable helper so every
barrier runs the main loop until its matching sequence completes and captures
core errors.

- [ ] **Step 5: Re-resolve sources on the capture connection before creating the stream**

At the beginning of `run_pipewire_stream_loop`:

1. Create core and registry.
2. Register the persistent graph listener.
3. Complete an initial core roundtrip.
4. Clone the current node/port snapshot.
5. Call `resolve_target_from_graph(&cfg, &nodes, &ports)`.
6. Reject a missing link factory with
   `PipeWire link factory was not advertised by the registry`.

Use this freshly resolved target for link IDs and verify its name and channel
count agree with the caller's earlier `ResolvedTarget`. A disagreement is a
startup capture error, not a fallback.

- [ ] **Step 6: Connect an inactive stream without automatic targeting**

Remove all insertion of `target.object`. Keep media, `dontRemix`, and latency
properties. Change stream connection to:

```rust
stream.connect(
    spa::utils::Direction::Input,
    None,
    pw::stream::StreamFlags::INACTIVE
        | pw::stream::StreamFlags::MAP_BUFFERS
        | pw::stream::StreamFlags::RT_PROCESS,
    &mut params,
)?;
```

Do not include `AUTOCONNECT`. Complete a core roundtrip, read
`stream.node_id()`, and call `plan_explicit_links` with the updated registry
snapshot.

- [ ] **Step 7: Create and retain non-lingering Link objects**

For each `PlannedLink`, create:

```rust
let properties = pw::properties::properties! {
    *pw::keys::LINK_OUTPUT_NODE => plan.output_node_id.to_string(),
    *pw::keys::LINK_OUTPUT_PORT => plan.output_port_id.to_string(),
    *pw::keys::LINK_INPUT_NODE => plan.input_node_id.to_string(),
    *pw::keys::LINK_INPUT_PORT => plan.input_port_id.to_string(),
};
let link = core
    .create_object::<pw::link::Link>(&link_factory, &properties)
    .map_err(pipewire_error)?;
```

Do not set `object.linger`. Register a link-info listener for each proxy and
store errors from `LinkState::Error(message)` in shared startup state. Keep
both `Vec<Link>` and `Vec<LinkListener>` in scope until the main loop exits.

- [ ] **Step 8: Acknowledge links, activate, then report readiness**

After creating all links:

1. Complete a core synchronization barrier.
2. Return any recorded core/proxy/link error.
3. Call `stream.set_active(true).map_err(pipewire_error)?`.
4. Complete one final barrier.
5. Send `ready_sender.send(Ok(()))`.
6. Enter the normal main loop with graph, stream, links, and listeners alive.

Ensure every fallible operation before step 5 propagates through the existing
thread wrapper, which sends `Err` to `ready_sender`; the daemon therefore never
constructs an active session after partial link startup.

- [ ] **Step 9: Verify source contains no automatic connection path**

Search `src/capture_pipewire.rs` for `AUTOCONNECT` and `target.object`.

Expected: no runtime occurrences. Mentions are allowed only in documentation
or assertions that explicitly prohibit them.

- [ ] **Step 10: Run backend and full tests**

Run:

```bash
cargo fmt --check
cargo test --test pipewire_backend
cargo test
```

Expected: all pass. The existing process-chunk test must still produce
`[1.0, 2.0, 3.0, 4.0]` and no graph-management code runs in the process
callback.

---

### Task 5: Documentation, User Migration, and End-to-End Verification

**Files:**
- Modify: `README.md:76-132`
- Modify: `~/.config/lamb/lamb.toml:1-23`
- Verify: all changed Rust and test files

**Interfaces:**
- Consumes: completed configuration, resolver, link runtime, and daemon-label behavior.
- Produces: documented migration and the requested live Scarlett configuration.

- [ ] **Step 1: Replace the README legacy PipeWire example**

Use the Pro Audio target form and explicit ports:

```toml
backend = "pipewire"
target = "alsa_input.usb-YourDevice-00.pro-input-0"
capturePorts = [
  { source = "capture_AUX0", name = "mic" },
  { source = "capture_AUX1", name = "gtr" },
]
```

Remove `channels` and `channelMap` from that PipeWire example.

- [ ] **Step 2: Add a profile PipeWire example and migration rules**

Document this schema:

```toml
[profiles.my-profile]
backend = "pipewire"

[profiles.my-profile.pipewire]
target = "alsa_input.usb-YourDevice-00.pro-input-0"
capturePorts = [
  { source = "capture_AUX0", name = "mic" },
  { source = "capture_AUX1", name = "gtr" },
]
```

State explicitly:

- Pro Audio exposes independently linkable source ports.
- `source` is an exact PipeWire `port.name` on the selected target.
- Array order defines captured channel order.
- `name` defines per-channel WAV filenames.
- PipeWire configurations without `capturePorts` fail before capture.
- Legacy `channels`, `channelMap`, and profile `pipewire.channelMap` must be removed.

- [ ] **Step 3: Run deterministic repository verification**

Run:

```bash
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
```

Expected: every command exits successfully with no warnings or whitespace
errors.

- [ ] **Step 4: Build a short live-smoke binary**

Run: `cargo build`

Expected: `target/debug/lamb` builds successfully against PipeWire 0.10.

- [ ] **Step 5: Create a temporary low-retention Scarlett smoke config**

Using the file-editing tool, create
`/tmp/opencode/lamb-pipewire-smoke.toml` with:

```toml
configVersion = 1
user = "kalki"
backend = "pipewire"
target = "alsa_input.usb-Focusrite_Scarlett_16i16_4th_Gen_S6GMC8F4901BC0-00.pro-input-0"
capturePorts = [
  { source = "capture_AUX0", name = "mic" },
  { source = "capture_AUX1", name = "gtr" },
  { source = "capture_AUX2", name = "percL" },
  { source = "capture_AUX3", name = "percR" },
]
seconds = 2
sampleRate = 44100
sampleFormat = "F32LE"
outputDir = "/tmp/opencode/lamb-pipewire-smoke-out"
dontRemix = true
maxActiveSnapshots = 1
allowQueuedRecall = false
controlSocketPath = "/tmp/opencode/lamb-pipewire-smoke.sock"
controlPermissions = "0600"

[memory]
headroom = 1.2

[export]
mode = "per-channel"
format = "wav"
splitWhenOverBytes = 1073741824
```

Verify `/tmp/opencode` before creating the output directory, then create only
`/tmp/opencode/lamb-pipewire-smoke-out`.

- [ ] **Step 6: Run a live startup/link/status/stop smoke cycle**

Run the daemon, checks, and cleanup sequentially in one shell invocation so a
failure cannot leave the test daemon intentionally running:

```bash
target/debug/lamb daemon --config /tmp/opencode/lamb-pipewire-smoke.toml & daemon_pid=$!; sleep 2; target/debug/lamb status --socket /tmp/opencode/lamb-pipewire-smoke.sock; pw-link -l -I -v; target/debug/lamb stop --socket /tmp/opencode/lamb-pipewire-smoke.sock; wait "$daemon_pid"
```

Expected:

- daemon startup succeeds;
- status reports `channel_count = 4`;
- the graph contains four links from Scarlett `capture_AUX0` through
  `capture_AUX3` to LAMB input ports with node-local IDs 0 through 3 in that
  order;
- stop succeeds and the LAMB stream and non-lingering links disappear.

If startup fails, preserve the output, diagnose before changing the user's
real configuration, and rerun deterministic tests after any code correction.

- [ ] **Step 7: Migrate the real user configuration exactly**

Using the file-editing tool, change `~/.config/lamb/lamb.toml` to:

```toml
configVersion = 1
user = "kalki"
backend = "pipewire"
target = "alsa_input.usb-Focusrite_Scarlett_16i16_4th_Gen_S6GMC8F4901BC0-00.pro-input-0"
capturePorts = [
  { source = "capture_AUX0", name = "mic" },
  { source = "capture_AUX1", name = "gtr" },
  { source = "capture_AUX2", name = "percL" },
  { source = "capture_AUX3", name = "percR" },
]
seconds = 1800
sampleRate = 44100
sampleFormat = "F32LE"
outputDir = "/home/kalki/.cache/lamb/out"
dontRemix = true
maxActiveSnapshots = 4
allowQueuedRecall = false
controlSocketPath = "/run/user/1002/lamb/control.sock"
controlPermissions = "0600"

[memory]
headroom = 1.2

[export]
mode = "per-channel"
format = "wav"
splitWhenOverBytes = 1073741824
```

Confirm there are no remaining `channels` or `channelMap` keys.

- [ ] **Step 8: Inspect final scope and rerun the release gate**

Inspect `git status --short` and `git diff` to ensure only the approved source,
tests, README, spec, and plan changed in the repository. Then rerun:

```bash
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
```

Expected: all commands pass. Report the live link evidence separately from the
deterministic test evidence, and report that the external user config was
migrated.
