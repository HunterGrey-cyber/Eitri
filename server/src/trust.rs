//! The trust scan the helper runs for a laptop: project discovery over a server tree, as the server's account.
use std::path::{Path, PathBuf};

use eitri_core::project_trust::{discover, Discovery, Limits, Profile, ProfileEnv};

use super::sidecars::{canonical_root, leads_to, open_root, refusal_text};

/// The most JSON one scan sends. A gRPC client accepts 4 MiB per message unless it is told otherwise; this leaves
/// room inside that for the response's other fields and framing.
pub const MAX_DISCOVERY_JSON: usize = 3 << 20;

/// The profile a trust-scan key names. Only the agents that run on a server have one.
pub fn profile(key: &str) -> Result<Profile, String> {
    match key {
        "claude" => Ok(Profile::Claude),
        "codex" => Ok(Profile::Codex),
        _ => Err("only claude and codex tabs run on a server".to_owned()),
    }
}

/// Discovery of `root` for the profile `profile_key` names, as JSON; `Err` is the reason the helper will not scan.
/// The scan reads the account's home from passwd and uses no environment of the daemon's own, which a channel
/// that started it may have set. It walks a tree, so callers run it off the async runtime.
pub fn scan(root: &str, profile_key: &str) -> Result<Vec<u8>, String> {
    scan_with(root, profile_key, |_| {})
}

/// [`scan`], calling `before_discovery` with the canonical root just before discovery walks it.
fn scan_with(root: &str, profile_key: &str, before_discovery: impl FnOnce(&Path)) -> Result<Vec<u8>, String> {
    let profile = profile(profile_key)?;
    if root.len() > 4096 {
        return Err("root exceeds byte cap".to_owned());
    }
    // The same checks a sidecar start makes: absolute, normal form, an existing directory the account owns. The
    // scan then names the directory as a sidecar started there records it, through every link on the way, since
    // that is where the agent runs and the directories above it are the ones it loads configuration from.
    let held = open_root(root).map_err(|status| refusal_text(&status))?;
    let canonical = canonical_root(&held).map_err(|status| refusal_text(&status))?;
    let home = super::login_env::minimal_environment()
        .map_err(|error| error.to_string())?
        .remove(std::ffi::OsStr::new("HOME"))
        .ok_or_else(|| "the account has no home directory".to_owned())?;
    // Discovery compares the root's ancestors with the home by path, so a home spelt through a link is resolved.
    let home = PathBuf::from(home);
    let home = home
        .is_absolute()
        .then(|| std::fs::canonicalize(&home).unwrap_or_else(|_| home.clone()));
    before_discovery(Path::new(&canonical));
    let discovery = discover(
        Path::new(&canonical),
        home.as_deref(),
        &Limits::default(),
        &profile,
        &ProfileEnv::default(),
    );
    // Discovery opens the path afresh from `/`. A directory renamed into its place after the checks above would
    // have been scanned in place of the one that passed them, and the answer would be about another tree.
    if !leads_to(&held, &canonical) {
        return Err("the project folder changed during the scan".to_owned());
    }
    encode(&discovery)
}

fn encode(discovery: &Discovery) -> Result<Vec<u8>, String> {
    let json = serde_json::to_vec(discovery).map_err(|error| match name_not_utf8(discovery) {
        Some(name) => format!(
            "the scan result cannot be sent: the name {} is not UTF-8",
            name.to_string_lossy()
        ),
        None => format!("the scan result cannot be sent: {error}"),
    })?;
    if json.len() > MAX_DISCOVERY_JSON {
        return Err(format!(
            "the scan result is {} bytes, more than the 3 MiB a scan may send",
            json.len()
        ));
    }
    Ok(json)
}

/// The first path in `discovery` that JSON cannot carry, so a refusal can say which one.
fn name_not_utf8(discovery: &Discovery) -> Option<&Path> {
    [discovery.root.as_path(), discovery.top.as_path()]
        .into_iter()
        .chain(discovery.walked.iter().map(PathBuf::as_path))
        .chain(discovery.entries.iter().map(|entry| entry.path.as_path()))
        .chain(discovery.instructions.keys().map(PathBuf::as_path))
        .find(|path| path.to_str().is_none())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_scan_accepts_only_the_claude_and_codex_profiles() {
        assert_eq!(profile("claude"), Ok(Profile::Claude));
        assert_eq!(profile("codex"), Ok(Profile::Codex));
        for key in ["", "acp-agent", "Claude", "claude ", "acp-%41"] {
            assert_eq!(
                profile(key),
                Err("only claude and codex tabs run on a server".to_owned()),
                "{key:?}"
            );
        }
    }

    #[test]
    fn trust_scan_refuses_a_root_that_is_relative_unnormalised_or_missing() {
        for root in [
            "",
            "relative/dir",
            "/tmp/../tmp",
            "/tmp//x",
            "/tmp/./x",
            "/nonexistent-eitri-root",
        ] {
            assert!(scan(root, "claude").is_err(), "{root:?}");
        }
        assert!(scan(&format!("/{}", "a".repeat(4096)), "claude")
            .unwrap_err()
            .contains("cap"));
    }

    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!("trust-{label}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            agent::private_fs::create_private_dir_all(&path, &path).unwrap();
            Self(std::fs::canonicalize(&path).unwrap())
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn trust_scan_refuses_a_discovery_too_large_to_send() {
        let scratch = Scratch::new("large");
        std::fs::create_dir_all(scratch.0.join(".claude")).unwrap();
        // A hook keeps every byte of its command, so one long command makes the whole discovery long.
        let command = "x".repeat(3 << 20 | 512 << 10);
        std::fs::write(
            scratch.0.join(".claude/settings.json"),
            format!(
                r#"{{"hooks":{{"PreToolUse":[{{"matcher":"Bash","hooks":[{{"type":"command","command":"{command}"}}]}}]}}}}"#
            ),
        )
        .unwrap();
        let refusal = scan(scratch.0.to_str().unwrap(), "claude").unwrap_err();
        assert!(refusal.starts_with("the scan result is "), "{refusal}");
        assert!(refusal.contains("3 MiB"), "{refusal}");
    }

    #[test]
    fn trust_scan_names_a_file_name_that_is_not_utf8() {
        use std::os::unix::ffi::OsStrExt;
        let scratch = Scratch::new("utf8");
        let rules = scratch.0.join(".claude/rules");
        std::fs::create_dir_all(&rules).unwrap();
        std::fs::write(rules.join(std::ffi::OsStr::from_bytes(b"bad\xffname.md")), "# rule\n").unwrap();
        let refusal = scan(scratch.0.to_str().unwrap(), "claude").unwrap_err();
        assert!(refusal.contains("not UTF-8"), "{refusal}");
        assert!(refusal.contains("bad\u{fffd}name.md"), "{refusal}");
    }

    #[test]
    fn trust_scan_refuses_when_another_directory_takes_the_roots_place() {
        let scratch = Scratch::new("swap");
        let root = scratch.0.join("project");
        std::fs::create_dir(&root).unwrap();
        let refusal = scan_with(root.to_str().unwrap(), "claude", |canonical| {
            assert_eq!(canonical, root);
            std::fs::rename(&root, scratch.0.join("checked")).unwrap();
            std::fs::create_dir(&root).unwrap();
        })
        .unwrap_err();
        assert_eq!(refusal, "the project folder changed during the scan");
        // Left alone, the same root scans.
        std::fs::remove_dir(&root).unwrap();
        std::fs::rename(scratch.0.join("checked"), &root).unwrap();
        assert!(scan_with(root.to_str().unwrap(), "claude", |_| {}).is_ok());
    }
}
