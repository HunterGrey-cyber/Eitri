//! Replaying a recorded scenario without a CLI: the fixture directory's wire recording goes through the sidecar
//! provider's own translation (`claude_sidecar::replay_frames`) and the result is the domain events the host
//! was handed when the scenario ran.
//!
//! A fixture directory holds `manifest.json`, `proto.bin` (length-delimited `SessionEvent` frames; a resumed
//! session adds `proto-resume1.bin`, `proto-resume2.bin`, ... in order), `outgoing.jsonl` (what the host sent,
//! each line with the position in the wire recording it was sent at) and `expected-events.json`. The manifest's
//! `facts.replay_settings` tells how the recording provider translated: `{splits_messages, requested_cli_mode, plan_offered}`
//! (`plan_offered` absent reads as false: a recording made before plan mode offered none).
use crate::providers::claude_sidecar::{replay_frames, ReplaySettings, ReplaySteer, ReplaySteerOutcome};
use crate::RequestedCliMode;
use claude_runtime_protocol::v1::SessionEvent;
use prost::Message;
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
};

/// Every directory under `root` that holds a `manifest.json`, sorted.
pub fn fixture_dirs(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let mut entries: Vec<PathBuf> = fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .collect();
        entries.sort();
        if dir.join("manifest.json").is_file() {
            out.push(dir.to_path_buf());
        }
        for path in entries.into_iter().filter(|p| p.is_dir()) {
            walk(&path, out);
        }
    }
    let mut out = Vec::new();
    walk(root, &mut out);
    out
}

/// The CLI version `cli.toml` pins.
pub fn pinned_cli_version(cli_toml: &str) -> Option<String> {
    cli_toml.lines().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        (key.trim() == "version").then(|| value.trim().trim_matches('"').to_owned())
    })
}

/// Verdandi revisions whose recordings stay valid under a later pin, as `(pinned, recorded, why)`: the
/// pinned revision's tree differs from the recorded one only in files no build or test reads, so a sidecar built
/// from either is the same program and a fixture recorded on `recorded` is exactly what `pinned` would record.
/// Both revisions are full 40-hex names, and the pair is directional: only a fixture recorded on `recorded`
/// is accepted under `pinned`. An entry is added only with the Verdandi diff between the two shown in the dated
/// record; any other pin move re-records.
pub const DOCS_ONLY_SUCCESSORS: &[(&str, &str, &str)] = &[(
    "ed8e058de696f2c13cc2dc4491af2a0d58d83964",
    "beb58449479f876cdac6f841943aae4b689a8217",
    "one line of crates/claude-runtime-protocol/COMPATIBILITY.md, no code",
)];

/// Whether `recorded` was recorded on a revision that `DOCS_ONLY_SUCCESSORS` lists as the code-identical
/// predecessor of `pin` (`pin` may be the 7-character prefix the constant carries, never shorter).
fn is_docs_only_predecessor(recorded: &str, pin: &str) -> bool {
    pin.len() >= 7
        && DOCS_ONLY_SUCCESSORS
            .iter()
            .any(|(pinned, predecessor, _)| pinned.starts_with(pin) && *predecessor == recorded)
}

/// The fixture's pins against the checkout's: `Err` names both and says how to record again.
pub fn check_pins(manifest: &Value, cli_version: &str, verdandi_revision: &str) -> Result<(), String> {
    let recorded_cli = manifest["cli_version"].as_str().unwrap_or("");
    if recorded_cli != cli_version {
        return Err(format!(
            "fixture recorded on {}, pin is {cli_version}: re-record with run.py --record",
            if recorded_cli.is_empty() {
                "an unnamed CLI"
            } else {
                recorded_cli
            }
        ));
    }
    let recorded = manifest["verdandi_revision"].as_str().unwrap_or("");
    let same = !recorded.is_empty()
        && !verdandi_revision.is_empty()
        && (recorded.starts_with(verdandi_revision) || verdandi_revision.starts_with(recorded));
    if !same && !is_docs_only_predecessor(recorded, verdandi_revision) {
        return Err(format!(
            "fixture recorded on {}, pin is {verdandi_revision}: re-record with run.py --record",
            if recorded.is_empty() {
                "an unnamed Verdandi revision"
            } else {
                recorded
            }
        ));
    }
    Ok(())
}

/// The frames of one `proto*.bin`.
pub fn read_frames(path: &Path) -> Result<Vec<SessionEvent>, String> {
    let bytes = fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut rest = bytes.as_slice();
    let mut frames = Vec::new();
    while !rest.is_empty() {
        frames.push(
            SessionEvent::decode_length_delimited(&mut rest)
                .map_err(|e| format!("{}: frame {}: {e}", path.display(), frames.len() + 1))?,
        );
    }
    Ok(frames)
}

/// The wire recordings of a fixture, one per session, in the order the sessions ran.
fn proto_files(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let first = dir.join("proto.bin");
    if !first.is_file() {
        return Err(format!("{}: no proto.bin", dir.display()));
    }
    let mut files = vec![first];
    for k in 1.. {
        let next = dir.join(format!("proto-resume{k}.bin"));
        if !next.is_file() {
            break;
        }
        files.push(next);
    }
    Ok(files)
}

/// The steered messages of each session, and the prompts sent under a client id (`"stamped": true`), from
/// `outgoing.jsonl`. The line that resumed a session was made on the session before it; every line after it
/// belongs to the next recording.
pub fn steers_by_session(outgoing: &str, sessions: usize) -> Result<Vec<Vec<ReplaySteer>>, String> {
    let mut by_session: Vec<Vec<ReplaySteer>> = (0..sessions).map(|_| Vec::new()).collect();
    let mut at = 0;
    for (n, line) in outgoing.lines().filter(|l| !l.trim().is_empty()).enumerate() {
        let entry: Value = serde_json::from_str(line).map_err(|e| format!("outgoing.jsonl line {}: {e}", n + 1))?;
        match entry["rpc"].as_str() {
            Some("steer_turn") if entry["error"].is_null() => {
                let steer = ReplaySteer {
                    after_events: entry["proto_events"].as_u64().unwrap_or(0) as usize,
                    client_message_id: entry["client_message_id"].as_str().unwrap_or_default().to_owned(),
                    as_typed: entry["text"].as_str().unwrap_or_default().to_owned(),
                    outcome: ReplaySteerOutcome::Accepted,
                };
                by_session
                    .get_mut(at)
                    .ok_or_else(|| format!("outgoing.jsonl line {}: a steer after the last session", n + 1))?
                    .push(steer);
            }
            // A prompt sent under a client id the sidecar may name back (`ClaudeSidecarProvider::send_turn_stamped`).
            Some("send_turn") if entry["error"].is_null() && entry["stamped"] == true => {
                let sent = ReplaySteer {
                    after_events: entry["proto_events"].as_u64().unwrap_or(0) as usize,
                    client_message_id: entry["client_message_id"].as_str().unwrap_or_default().to_owned(),
                    as_typed: entry["text"].as_str().unwrap_or_default().to_owned(),
                    outcome: ReplaySteerOutcome::Sent,
                };
                by_session
                    .get_mut(at)
                    .ok_or_else(|| format!("outgoing.jsonl line {}: a send after the last session", n + 1))?
                    .push(sent);
            }
            Some("resume_session") if entry["error"].is_null() => at += 1,
            _ => {}
        }
    }
    Ok(by_session)
}

/// How the recording provider translated, from the manifest.
pub fn settings_of(manifest: &Value) -> Result<ReplaySettings, String> {
    let settings = &manifest["facts"]["replay_settings"];
    let splits_messages = settings["splits_messages"]
        .as_bool()
        .ok_or("manifest.json: facts.replay_settings.splits_messages is missing")?;
    let requested_cli_mode = match settings["requested_cli_mode"].as_str() {
        Some("auto") => RequestedCliMode::Auto,
        Some("default") => RequestedCliMode::Default,
        other => {
            return Err(format!(
                "manifest.json: facts.replay_settings.requested_cli_mode is {other:?}"
            ))
        }
    };
    // A recording made before plan mode existed has no word of it: the session offered none.
    let plan_offered = settings["plan_offered"].as_bool().unwrap_or(false);
    Ok(ReplaySettings {
        splits_messages,
        requested_cli_mode,
        plan_offered,
    })
}

/// The domain events a fixture's recording yields, as the JSON `expected-events.json` holds them.
pub fn replay_dir(dir: &Path) -> Result<Vec<Value>, String> {
    let manifest: Value = serde_json::from_str(
        &fs::read_to_string(dir.join("manifest.json")).map_err(|e| format!("{}: manifest.json: {e}", dir.display()))?,
    )
    .map_err(|e| format!("{}: manifest.json: {e}", dir.display()))?;
    let settings = settings_of(&manifest)?;
    let files = proto_files(dir)?;
    let outgoing = fs::read_to_string(dir.join("outgoing.jsonl")).unwrap_or_default();
    let steers = steers_by_session(&outgoing, files.len())?;
    let mut events = Vec::new();
    for (file, steers) in files.iter().zip(&steers) {
        let frames = read_frames(file)?;
        let replayed = replay_frames(&frames, steers, settings).map_err(|e| format!("{}: {e}", file.display()))?;
        for event in replayed {
            events.push(serde_json::to_value(&event).map_err(|e| format!("a domain event does not serialize: {e}"))?);
        }
    }
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_pinned_cli_version_is_read_from_the_pin_file() {
        let toml = "# comment\nversion = \"2.1.311\"\nsha256 = \"ab\"\n";
        assert_eq!(pinned_cli_version(toml).as_deref(), Some("2.1.311"));
        assert_eq!(pinned_cli_version("sha256 = \"x\""), None);
    }

    #[test]
    fn a_fixture_recorded_on_another_pin_says_what_to_do() {
        let manifest = json!({"cli_version": "2.1.311", "verdandi_revision": "30d1838"});
        let err = check_pins(&manifest, "2.1.311", "785a18a").unwrap_err();
        assert_eq!(
            err,
            "fixture recorded on 30d1838, pin is 785a18a: re-record with run.py --record"
        );
        let err = check_pins(&manifest, "2.1.400", "30d1838").unwrap_err();
        assert_eq!(
            err,
            "fixture recorded on 2.1.311, pin is 2.1.400: re-record with run.py --record"
        );
        assert!(
            check_pins(&manifest, "2.1.311", "30d1838ab12").is_ok(),
            "a short pin names the same revision"
        );
    }

    #[test]
    fn a_listed_docs_only_successor_accepts_the_predecessors_recordings_in_that_direction_only() {
        let (pinned, predecessor, _) = DOCS_ONLY_SUCCESSORS[0];
        let on = |rev: &str| json!({"cli_version": "2.1.311", "verdandi_revision": rev});
        assert!(
            check_pins(&on(predecessor), "2.1.311", pinned).is_ok(),
            "the listed pair"
        );
        assert!(
            check_pins(&on(predecessor), "2.1.311", &pinned[..7]).is_ok(),
            "the pin's 7-character prefix"
        );
        let err = check_pins(&on(pinned), "2.1.311", predecessor).unwrap_err();
        assert_eq!(
            err,
            format!("fixture recorded on {pinned}, pin is {predecessor}: re-record with run.py --record"),
            "the wrong direction"
        );
        let other = "30d1838ab12c0b6f3d6e0f6d9b8a7c5e4d3b2a19";
        assert!(check_pins(&on(other), "2.1.311", pinned).is_err(), "an unlisted pair");
        assert!(
            check_pins(&on(&predecessor[..7]), "2.1.311", pinned).is_err(),
            "the listing names full revisions"
        );
        assert!(
            check_pins(&on(predecessor), "2.1.311", "ed8e0").is_err(),
            "a prefix shorter than seven"
        );
        assert!(
            check_pins(&on(predecessor), "2.1.400", pinned).is_err(),
            "the CLI check is unchanged"
        );
    }

    #[test]
    fn a_resume_moves_the_following_steers_to_the_next_recording() {
        let outgoing = [
            json!({"rpc": "steer_turn", "error": null, "proto_events": 4, "client_message_id": "a", "text": "one"}),
            json!({"rpc": "resume_session", "error": null, "proto_events": 9}),
            json!({"rpc": "steer_turn", "error": null, "proto_events": 2, "client_message_id": "b", "text": "two"}),
            json!({"rpc": "steer_turn", "error": "refused", "proto_events": 3, "client_message_id": "c", "text": "three"}),
        ]
        .map(|l| l.to_string())
        .join("\n");
        let by_session = steers_by_session(&outgoing, 2).unwrap();
        assert_eq!(by_session[0].len(), 1);
        assert_eq!(by_session[0][0].after_events, 4);
        assert_eq!(by_session[0][0].as_typed, "one");
        assert_eq!(by_session[1].len(), 1, "a call that failed opened nothing");
        assert_eq!(by_session[1][0].client_message_id, "b");
    }
}
