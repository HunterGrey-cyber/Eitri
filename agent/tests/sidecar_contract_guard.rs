//! The workspace's one `claude-runtime-protocol` pin (the root `Cargo.toml`'s `[workspace.dependencies]`, which
//! agent and server take with `workspace = true`) and
//! `agent::BUNDLED_SIDECAR_REVISION` must name the same commit. Either can be a full hash
//! or a short prefix, so compare prefixes in either direction, as Cargo accepts both forms.
//! `providers::claude_sidecar::spawn::describe_checkout` uses the same comparison for runtime drift.
//!
//! This static, offline check reads the manifest and compares it with the compiled constant.
//! The runtime drift warning compares that constant with a running checkout and never reads
//! the manifest, so it cannot catch a manifest-only bump. Updating both pins together keeps
//! this check passing; there is no generated file to regenerate.

use std::fs;
use std::path::PathBuf;

use agent::providers::claude_sidecar::CLIENT_PROTOCOL_MAJOR;
use agent::{BUNDLED_SIDECAR_REVISION, BUNDLED_SIDECAR_VERSION, REQUIRED_CAPABILITIES};

#[test]
fn the_pinned_protocol_exposes_the_preset() {
    assert_eq!(
        claude_runtime_protocol::CAP_SYSTEM_PROMPT_PRESET,
        "system_prompt_preset"
    );
    let preset = claude_runtime_protocol::v1::SystemPromptPreset { append: None };
    assert!(preset.append.is_none());
}

/// The pinned revision speaks protocol 3.25, whose capabilities Eitri reads by name: a protocol bump
/// the client does not know fails here.
#[test]
fn the_pinned_protocol_is_3_26_and_names_its_capabilities() {
    use claude_runtime_protocol as wire;
    assert_eq!(wire::PROTOCOL_MAJOR, 3);
    assert_eq!(wire::PROTOCOL_MINOR, 26);
    let new_in_26: Vec<&str> = wire::CAPABILITY_SINCE_MINOR
        .iter()
        .filter(|(_, minor)| *minor == 26)
        .map(|(name, _)| *name)
        .collect();
    assert_eq!(
        new_in_26,
        ["plan_mode_v1", "turn_content_v1"],
        "the capabilities of minor 26"
    );
    assert_eq!(wire::CAP_TEXT_BLOCK_START, "text_block_start");
    let new_in_25: Vec<&str> = wire::CAPABILITY_SINCE_MINOR
        .iter()
        .filter(|(_, minor)| *minor == 25)
        .map(|(name, _)| *name)
        .collect();
    assert_eq!(new_in_25, ["text_block_start"], "the capabilities of minor 25");
    let block = wire::v1::TextDelta {
        starts_block: true,
        ..Default::default()
    };
    assert!(block.starts_block);
    assert_eq!(wire::CAP_CLIENT_TOOLS, "client_tools");
    assert_eq!(wire::CAP_SET_MODEL, "set_model");
    assert_eq!(wire::CAP_LIST_MODELS, "list_models");
    assert_eq!(wire::CAP_APPLY_EFFORT, "apply_effort");
    assert_eq!(wire::CAP_TASK_OUTPUT_RANGE, "task_output_range");
    let new_in_24: Vec<&str> = wire::CAPABILITY_SINCE_MINOR
        .iter()
        .filter(|(_, minor)| *minor == 24)
        .map(|(name, _)| *name)
        .collect();
    assert_eq!(
        new_in_24,
        [
            "set_model",
            "list_models",
            "apply_effort",
            "session_effort_report",
            "cli_mode_reports",
            "task_output_range"
        ],
        "the capabilities of minor 24"
    );
    let new_in_23: Vec<&str> = wire::CAPABILITY_SINCE_MINOR
        .iter()
        .filter(|(_, minor)| *minor == 23)
        .map(|(name, _)| *name)
        .collect();
    assert_eq!(new_in_23, ["client_tools"], "the one capability of minor 23");
}

/// Eitri reads minor 22's capabilities by name. Each constant the protocol crate exports must be the
/// word Eitri compares the handshake against, and each must be one the crate lists as new in minor 22:
/// a rename on either side fails here, not as a feature that silently reads as absent.
#[test]
fn the_capabilities_of_minor_22_are_named_as_eitri_reads_them() {
    use claude_runtime_protocol as wire;
    let read_by_eitri = [
        (wire::CAP_CLI_TURNS, "cli_turns"),
        (wire::CAP_HUMAN_ORIGIN, "human_origin"),
        (wire::CAP_SUBAGENT_FRAMES, "subagent_frames"),
        (wire::CAP_BACKGROUND_TASKS, "background_tasks"),
        (wire::CAP_WORKFLOW_PROGRESS, "workflow_progress"),
        (wire::CAP_TASK_CONTROL, "task_control"),
        (wire::CAP_TASK_OUTPUT, "task_output"),
        (wire::CAP_SESSION_NOTICES, "session_notices"),
        (wire::CAP_CONVERSATION_RESET, "conversation_reset"),
        (wire::CAP_STRUCTURED_QUESTIONS_V1, "structured_questions_v1"),
    ];
    for (exported, word) in read_by_eitri {
        assert_eq!(exported, word, "the capability string Eitri compares against");
        assert_eq!(
            wire::CAPABILITY_SINCE_MINOR
                .iter()
                .find(|(name, _)| *name == word)
                .map(|(_, minor)| *minor),
            Some(22),
            "{word} must be a capability of minor 22"
        );
    }
    let new_in_22: Vec<&str> = wire::CAPABILITY_SINCE_MINOR
        .iter()
        .filter(|(_, minor)| *minor == 22)
        .map(|(name, _)| *name)
        .collect();
    assert_eq!(
        new_in_22,
        read_by_eitri.iter().map(|(_, word)| *word).collect::<Vec<_>>(),
        "every capability minor 22 adds has a ProviderCapabilities flag"
    );
}

/// Both opt-ins have explicit controls in the panel.
#[test]
// Constants on purpose: this test is what a flip has to change.
#[allow(clippy::assertions_on_constants)]
fn the_3_22_opt_ins_match_the_available_controls() {
    assert!(agent::providers::claude_sidecar::CLIENT_IMPLEMENTS_PER_TASK_STOP);
    assert!(agent::providers::claude_sidecar::CLIENT_IMPLEMENTS_QUESTIONS);
}

/// `Cargo.lock` names the manifest's revision in full: the lock decides which commit is built, so a
/// manifest bumped without its lock would build the old protocol under the new name.
#[test]
fn the_lock_names_the_manifest_revision() {
    let manifest_rev = cargo_toml_rev();
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../Cargo.lock");
    let lock = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let package = lock
        .split("[[package]]")
        .find(|block| block.contains("name = \"claude-runtime-protocol\""))
        .unwrap_or_else(|| panic!("{}: no claude-runtime-protocol package", path.display()));
    let source = package
        .lines()
        .find(|line| line.starts_with("source = "))
        .unwrap_or_else(|| panic!("claude-runtime-protocol has no source line: {package}"));
    assert!(
        source.contains(&format!("?rev={manifest_rev}#{manifest_rev}\"")),
        "Cargo.lock's {source} must name the workspace pin's rev {manifest_rev} in full"
    );
    assert_eq!(manifest_rev.len(), 40, "the manifest names the full revision");
}

/// Pulls the `rev = "..."` value off the root `Cargo.toml`'s `claude-runtime-protocol` dependency line,
/// by text, deliberately not by parsing TOML -- this test wants to fail loudly if that one line's
/// shape ever stops looking like a `git` dependency with a `rev`, not silently accept some other
/// shape a TOML parser would happily normalize.
fn cargo_toml_rev() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../Cargo.toml");
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let section = text
        .split("\n[")
        .find(|section| section.starts_with("workspace.dependencies]"))
        .unwrap_or_else(|| panic!("{}: no [workspace.dependencies] section", path.display()));
    rev_from_manifest_text(section, &path.display().to_string())
}

/// Every `Cargo.toml` of the checkout, build outputs and installed packages left out.
fn manifests(dir: &std::path::Path, found: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())) {
        let entry = entry.unwrap();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let kind = entry.file_type().unwrap();
        if kind.is_dir() {
            if !name.starts_with('.') && !matches!(name.as_ref(), "target" | "node_modules" | "docs") {
                manifests(&entry.path(), found);
            }
        } else if kind.is_file() && name == "Cargo.toml" {
            found.push(entry.path());
        }
    }
}

/// The protocol crate is pinned in one place, so no crate can build against another revision of it than the
/// sidecar and the agent: the root manifest names its source, and every crate that uses it takes it from there.
#[test]
fn the_protocol_crate_is_pinned_once_for_the_workspace() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut found = Vec::new();
    manifests(&root, &mut found);
    let mut pins = Vec::new();
    let mut users = Vec::new();
    for path in &found {
        let text = fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        for line in text.lines().filter(|l| !l.trim_start().starts_with('#')) {
            if line.contains("verdandi.git") || (line.contains("claude-runtime-protocol") && line.contains("rev")) {
                pins.push(path.clone());
            } else if line.trim_start().starts_with("claude-runtime-protocol") {
                assert!(
                    line.replace(' ', "").contains("workspace=true"),
                    "{}: {line:?} must take the workspace's pin",
                    path.display()
                );
                users.push(path.clone());
            }
        }
    }
    let root_manifest = root.join("Cargo.toml");
    assert_eq!(pins, [root_manifest], "one pin, in the root manifest");
    for crate_dir in ["agent", "server"] {
        let manifest = root.join(crate_dir).join("Cargo.toml");
        assert!(
            users.contains(&manifest),
            "{} takes the workspace's pin",
            manifest.display()
        );
    }
}

/// The parsing logic `cargo_toml_rev` runs, pulled out so it can be exercised on a synthetic
/// manifest string (`the_active_line_wins_over_a_commented_one_above_it`, below) without touching
/// the real `Cargo.toml` or its git dependency resolution.
///
/// Skips any line that is commented out in TOML (its first non-whitespace character is `#`) --
/// `.lines().find()` otherwise returns the FIRST line mentioning both `claude-runtime-protocol` and
/// `rev`, comment or not, so a stale pin left commented out *above* the real one would be read
/// instead of it.
fn rev_from_manifest_text(text: &str, path_for_errors: &str) -> String {
    let line = text
        .lines()
        .find(|l| !l.trim_start().starts_with('#') && l.contains("claude-runtime-protocol") && l.contains("rev"))
        .unwrap_or_else(|| {
            panic!(
                "{path_for_errors}: no active (non-comment) line mentions both claude-runtime-protocol and rev \
                 -- has the pin moved to a different shape (a [patch] section, a path override, a version), or \
                 is it commented out?"
            )
        });
    let rev_at = line
        .find("rev")
        .unwrap_or_else(|| panic!("{line:?}: 'rev' vanished between the two .contains() checks and this find"));
    let after_rev = &line[rev_at..];
    let open_quote = after_rev
        .find('"')
        .unwrap_or_else(|| panic!("{line:?}: no opening quote after 'rev'"));
    let rest = &after_rev[open_quote + 1..];
    let close_quote = rest
        .find('"')
        .unwrap_or_else(|| panic!("{line:?}: no closing quote for rev's value"));
    rest[..close_quote].to_string()
}

fn is_hex(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_hexdigit())
}

#[test]
fn manifest_pin_and_expected_revision_name_the_same_commit() {
    let manifest_rev = cargo_toml_rev();
    let expected = BUNDLED_SIDECAR_REVISION;

    assert!(
        is_hex(&manifest_rev) && manifest_rev.len() >= 7,
        "Cargo.toml's workspace claude-runtime-protocol rev {manifest_rev:?} must be at least 7 hex characters"
    );
    assert!(
        is_hex(expected) && expected.len() >= 7,
        "agent::BUNDLED_SIDECAR_REVISION {expected:?} must be at least 7 hex characters"
    );
    assert!(
        manifest_rev.starts_with(expected) || expected.starts_with(manifest_rev.as_str()),
        "Cargo.toml pins the workspace claude-runtime-protocol at {manifest_rev:?}, but \
         agent::BUNDLED_SIDECAR_REVISION is {expected:?} -- these must name the same commit. \
         Bump both together and re-verify against the real proto diff \
         (the drill: `git diff <old>..<new> -- proto crates` \
         should be empty for a \
         mechanical follow-up, or the capability/wire changes it adds should be named in the commit \
         message)."
    );
}

/// A stale commented pin above the dependency must not win: `.lines().find()` otherwise
/// returns the first matching line even if it starts with `#`. Use synthetic manifest text
/// rather than mutating the real manifest, because an unresolvable revision would fail Cargo
/// before the pin-agreement test's assertion could run.
#[test]
fn the_active_line_wins_over_a_commented_one_above_it() {
    let manifest = "[dependencies]\n\
                     # claude-runtime-protocol = { git = \"ssh://git@git.example.org/owner/verdandi.git\", rev = \"b3aa188\" }\n\
                     claude-runtime-protocol = { git = \"ssh://git@git.example.org/owner/verdandi.git\", rev = \"c0b309e\" }\n";
    assert_eq!(
        rev_from_manifest_text(manifest, "<synthetic>"),
        "c0b309e",
        "the commented-out b3aa188 pin above the active c0b309e one must not win"
    );
}

/// The same fixture with no active line at all (every mention commented out) must fail loudly,
/// naming the file, rather than silently falling back to the commented value.
#[test]
#[should_panic(expected = "no active (non-comment) line")]
fn an_entirely_commented_out_dependency_is_not_a_pin() {
    let manifest = "[dependencies]\n\
                     # claude-runtime-protocol = { git = \"ssh://git@git.example.org/owner/verdandi.git\", rev = \"b3aa188\" }\n";
    rev_from_manifest_text(manifest, "<synthetic>");
}

/// A regression this test exists to catch, run in reverse: two revisions that are genuinely
/// different (not prefixes of each other) must fail the same comparison this test makes -- otherwise
/// the prefix check itself could be silently satisfying everything.
#[test]
fn the_prefix_comparison_itself_rejects_two_different_revisions() {
    let a = "b3aa1889111111111111111111111111111111";
    let b = "c0b309e0222222222222222222222222222222";
    assert!(
        !(a.starts_with(b) || b.starts_with(a)),
        "these two fixture revisions must not be prefixes of each other"
    );
}

/// The value of `key` in `packaging/sidecar-contract.env`: `KEY=value` lines, `#` lines and blanks skipped.
fn contract_value(key: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../packaging/sidecar-contract.env");
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let prefix = format!("{key}=");
    let mut found = text
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .filter_map(|line| line.strip_prefix(prefix.as_str()));
    let value = found
        .next()
        .unwrap_or_else(|| panic!("{}: no {key} line", path.display()));
    assert!(found.next().is_none(), "{}: {key} appears twice", path.display());
    value.trim().to_string()
}

/// The shell side reads the contract file and this crate holds the same values as constants:
/// the two must not drift apart.
#[test]
fn the_contract_file_says_what_the_constants_say() {
    assert_eq!(contract_value("SIDECAR_VERSION"), BUNDLED_SIDECAR_VERSION);
    assert_eq!(
        contract_value("SIDECAR_PROTOCOL_MAJOR"),
        CLIENT_PROTOCOL_MAJOR.to_string()
    );
    let from_file = contract_value("SIDECAR_REQUIRED_CAPABILITIES");
    let from_file: Vec<&str> = from_file.split(' ').filter(|word| !word.is_empty()).collect();
    assert_eq!(
        from_file,
        REQUIRED_CAPABILITIES.to_vec(),
        "same capabilities, same order"
    );
}

/// A selected update is held to `REQUIRED_CAPABILITIES`, so each one must be a capability every
/// sidecar of the protocol major advertises without a condition, and one the protocol crate knows.
#[test]
fn required_capabilities_are_advertised_unconditionally() {
    let conditional = ["steer_turn", "task_output", "task_output_range", "egress_restricted"];
    for capability in REQUIRED_CAPABILITIES {
        assert!(
            !conditional.contains(&capability),
            "{capability} is advertised only under a condition, so an update cannot be required to offer it"
        );
        assert!(
            claude_runtime_protocol::CAPABILITY_SINCE_MINOR
                .iter()
                .any(|(name, _)| *name == capability),
            "{capability} is not a capability the protocol crate lists"
        );
    }
}
