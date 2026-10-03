//! Which of the CLI's settings tiers a session loads: the user's own (`~/.claude`, or the
//! configured account's directory), the project's `.claude/settings.json` plus its `CLAUDE.md` and
//! `.mcp.json`, and the project's `.claude/settings.local.json`.
//!
//! The selection has two halves:
//!
//! - **The user tier, process-wide.** It loads by default, as the `claude` CLI does in a terminal,
//!   so the user's own hooks, plugins, skills, `CLAUDE.md` and permission rules run inside Eitri
//!   sessions. `init.lua`'s `agent.user_settings = false` drops it. Like the account
//!   (`crate::account`), `shell` pins it once, right after `init.lua` has run ([`configure`]), and
//!   [`loads_user_settings`] is the one answer.
//! - **The project and local tiers, per session** ([`ProjectTrust`]). A repository's own
//!   configuration can run commands as the user (a `SessionStart` hook, an `.mcp.json` server) and
//!   pre-approve tool calls, so it loads only for a session the window's trust gate started as
//!   `Trusted`. An untrusted session gets the user tier alone, or nothing at all when the user tier
//!   is off. Every caller that starts a session names the value; there is no default, so a new
//!   session path cannot quietly inherit the wider selection.
//!
//! Everything that states the selection lives here -- the tiers ([`for_session`]), the legacy
//! argv's `--setting-sources` value ([`cli_argument_for`]) and the sentence `prefix i` shows
//! ([`note_for_session`]). The sidecar's request maps the same [`Source`] list onto the wire, so a
//! change to one cannot leave the others behind.

use std::sync::OnceLock;

/// One of the CLI's settings tiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    User,
    Project,
    Local,
}

/// Whether this session may load the project's own tiers. Chosen per session by the shell's trust
/// gate. Deliberately without a `Default`: a session started without anyone deciding must not
/// load a repository's hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectTrust {
    Trusted,
    Untrusted,
}

// Each in the order the tiers stack: user, then project, then local.
const ALL_THREE: &[Source] = &[Source::User, Source::Project, Source::Local];
const PROJECT_AND_LOCAL: &[Source] = &[Source::Project, Source::Local];
const USER_ONLY: &[Source] = &[Source::User];
const NONE: &[Source] = &[];

/// What `prefix i` says for a trusted session with the user tier on.
pub const NOTE_TRUSTED_WITH_USER: &str =
    "user + project + local, as the claude CLI does: ~/.claude's settings, hooks, plugins, skills and \
     CLAUDE.md load, and so do .claude/, .mcp.json and CLAUDE.md here";

/// What `prefix i` says for a trusted session under `agent.user_settings = false`.
pub const NOTE_TRUSTED_WITHOUT_USER: &str =
    "project + local only (.claude/, .mcp.json and CLAUDE.md here); ~/.claude's settings, hooks, plugins \
     and CLAUDE.md are not loaded (agent.user_settings = false)";

/// What `prefix i` says for an untrusted session with the user tier on.
pub const NOTE_UNTRUSTED_WITH_USER: &str =
    "user only: ~/.claude's settings, hooks, plugins, skills and CLAUDE.md load; this project is not \
     trusted, so its own .claude/, .mcp.json and CLAUDE.md are not loaded";

/// What `prefix i` says for an untrusted session under `agent.user_settings = false`.
pub const NOTE_UNTRUSTED_WITHOUT_USER: &str =
    "nothing: this project is not trusted, so its own .claude/, .mcp.json and CLAUDE.md are not loaded, \
     and neither is ~/.claude (agent.user_settings = false)";

/// The tiers a session loads, in stacking order:
///
/// | `user_settings` | `Trusted`                 | `Untrusted` |
/// |---|---|---|
/// | `true`          | `[User, Project, Local]`  | `[User]`    |
/// | `false`         | `[Project, Local]`        | `[]`        |
pub fn for_session(user_settings: bool, project: ProjectTrust) -> &'static [Source] {
    match (user_settings, project) {
        (true, ProjectTrust::Trusted) => ALL_THREE,
        (false, ProjectTrust::Trusted) => PROJECT_AND_LOCAL,
        (true, ProjectTrust::Untrusted) => USER_ONLY,
        (false, ProjectTrust::Untrusted) => NONE,
    }
}

/// The CLI's `--setting-sources` value for a selection: the tier names joined by `,`. The empty
/// selection is the empty string, passed as an argument of its own, never left out: without the
/// flag the CLI would load every tier.
pub fn cli_argument_for(sources: &[Source]) -> String {
    sources
        .iter()
        .map(|source| match source {
            Source::User => "user",
            Source::Project => "project",
            Source::Local => "local",
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// The sentence `prefix i`'s `settings` row shows for a selection.
pub fn note_for_session(user_settings: bool, project: ProjectTrust) -> &'static str {
    match (user_settings, project) {
        (true, ProjectTrust::Trusted) => NOTE_TRUSTED_WITH_USER,
        (false, ProjectTrust::Trusted) => NOTE_TRUSTED_WITHOUT_USER,
        (true, ProjectTrust::Untrusted) => NOTE_UNTRUSTED_WITH_USER,
        (false, ProjectTrust::Untrusted) => NOTE_UNTRUSTED_WITHOUT_USER,
    }
}

static USER_SETTINGS: OnceLock<bool> = OnceLock::new();

/// Pins whether this process's sessions load the user tier. Called once, by `shell`, after
/// `init.lua` has run. A second call keeps the first answer and says so: it cannot happen in the
/// product, and changing the selection under live sessions would make the note a lie.
pub fn configure(user_settings: bool) {
    if USER_SETTINGS.set(user_settings).is_err() {
        eprintln!(
            "[settings] configure() called twice; keeping user_settings={} and ignoring {user_settings}",
            loads_user_settings()
        );
    }
}

/// Whether sessions load the user tier: `true` unless [`configure`] said otherwise, which is the
/// shipped default and the state every test runs in.
pub fn loads_user_settings() -> bool {
    USER_SETTINGS.get().copied().unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ProjectTrust::{Trusted, Untrusted};
    use Source::{Local, Project, User};

    #[test]
    fn the_user_tier_loads_unless_configured_off() {
        // Nothing in this crate's tests calls `configure`, so this is the shipped default.
        assert!(loads_user_settings());
    }

    #[test]
    fn for_session_states_four_selections() {
        assert_eq!(for_session(true, Trusted), &[User, Project, Local]);
        assert_eq!(for_session(false, Trusted), &[Project, Local]);
        assert_eq!(for_session(true, Untrusted), &[User]);
        assert_eq!(for_session(false, Untrusted), &[] as &[Source]);
    }

    #[test]
    fn cli_argument_for_each_selection() {
        assert_eq!(cli_argument_for(for_session(true, Trusted)), "user,project,local");
        assert_eq!(cli_argument_for(for_session(false, Trusted)), "project,local");
        assert_eq!(cli_argument_for(for_session(true, Untrusted)), "user");
        assert_eq!(cli_argument_for(for_session(false, Untrusted)), "");
    }

    #[test]
    fn the_four_notes_differ_and_name_what_is_left_out() {
        let all = [(true, Trusted), (false, Trusted), (true, Untrusted), (false, Untrusted)];
        let notes: Vec<&str> = all.iter().map(|&(u, p)| note_for_session(u, p)).collect();
        for (i, a) in notes.iter().enumerate() {
            for b in &notes[i + 1..] {
                assert_ne!(a, b, "two selections share a sentence");
            }
        }
        for (&(user, project), note) in all.iter().zip(&notes) {
            assert_eq!(
                note.contains("not trusted"),
                project == Untrusted,
                "`not trusted` must appear exactly in the untrusted sentences: {note}"
            );
            assert_eq!(
                note.contains("agent.user_settings = false"),
                !user,
                "the opt-out key must appear exactly when the user tier is off: {note}"
            );
            assert!(note.contains(".mcp.json"), "{note}");
        }
    }

    /// The tier lists are spelled only here, and the proto's project and local tiers are named only
    /// where the sidecar maps a [`Source`] onto the wire. A second spelling elsewhere would be a
    /// selection that does not follow the session's trust. Production code only: test code states
    /// expected values on its own, which is the point of a test.
    #[test]
    fn only_setting_sources_names_the_tier_lists() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let files = crate::wire_guard::production_code(&root);
        for must in ["process.rs", "providers/claude_sidecar/mod.rs"] {
            assert!(files.contains_key(must), "{must} was not scanned");
        }
        const LITERALS: &[&str] = &["\"user,project,local\"", "\"project,local\"", "\"user,project\""];
        const PROTO_NAMES: &[&str] = &[
            "SettingSource::Project",
            "SettingSource::Local",
            "SettingSource as",
            "SettingSource::{",
            "SettingSource::*",
        ];
        let mut offenders = Vec::new();
        for (rel, code) in &files {
            if rel == "setting_sources.rs" {
                continue;
            }
            let code = if rel == "providers/claude_sidecar/mod.rs" {
                crate::wire_guard::without_fn_body(code, "proto_sources").unwrap_or_else(|why| panic!("{rel}: {why}"))
            } else {
                code.clone()
            };
            for needle in LITERALS.iter().chain(PROTO_NAMES) {
                if code.contains(needle) {
                    offenders.push(format!("{rel}: {needle}"));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "the tier selection is spelled outside setting_sources.rs / proto_sources: {offenders:?}"
        );
    }

    #[test]
    fn the_scan_cuts_only_the_named_function_and_sees_through_line_breaks() {
        let code = crate::wire_guard::collapse(
            "fn proto_sources(s: &[Source]) -> Vec<i32> { vec![SettingSource :: Project as i32] }\n\
             fn other() { SettingSource\n    ::Local }",
        );
        let cut = crate::wire_guard::without_fn_body(&code, "proto_sources").unwrap();
        assert!(!cut.contains("SettingSource::Project"), "{cut}");
        assert!(cut.contains("SettingSource::Local"), "{cut}");
        assert!(crate::wire_guard::without_fn_body(&code, "missing").is_err());
        let twice = format!("{code} fn proto_sources() {{}}");
        assert!(crate::wire_guard::without_fn_body(&twice, "proto_sources").is_err());
    }
}
