//! Which of the CLI's settings tiers a session loads: the user's own (`~/.claude`, or the
//! configured account's directory), the project's `.claude/settings.json` plus its `CLAUDE.md`,
//! and the project's `.claude/settings.local.json`.
//!
//! Eitri sessions load all three by default, as the `claude` CLI does in a terminal, so the user's
//! own hooks, plugins, skills, `CLAUDE.md` and permission rules run inside them. `init.lua`'s
//! `agent.user_settings = false` drops the user tier and leaves project and local. The choice is a
//! property of the whole process, like the account (`crate::account`): `shell` pins it once,
//! right after `init.lua` has run, and both backends and the panel read the one answer.
//!
//! Everything that states the selection lives here -- the sidecar's request (via
//! [`loads_user_settings`]), the legacy argv ([`cli_argument`]) and the sentence `prefix i` shows
//! ([`note`]) -- so a change to one has to look at the others.

use std::sync::OnceLock;

/// What `prefix i` says when the user tier loads.
pub const NOTE_WITH_USER: &str =
    "user + project + local, as the claude CLI does: ~/.claude's settings, hooks, plugins, \
     skills and CLAUDE.md load, and so do .claude/ and CLAUDE.md here";

/// What `prefix i` says under `agent.user_settings = false`.
pub const NOTE_WITHOUT_USER: &str = "project + local only (.claude/ and CLAUDE.md here); ~/.claude's settings, hooks, \
     plugins and CLAUDE.md are not loaded (agent.user_settings = false)";

/// The value of the CLI's `--setting-sources` for a selection.
pub fn cli_argument(user_settings: bool) -> &'static str {
    if user_settings {
        "user,project,local"
    } else {
        "project,local"
    }
}

/// The sentence for a selection.
pub fn note_for(user_settings: bool) -> &'static str {
    if user_settings {
        NOTE_WITH_USER
    } else {
        NOTE_WITHOUT_USER
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

/// The `--setting-sources` value the legacy spawn passes.
pub fn configured_cli_argument() -> &'static str {
    cli_argument(loads_user_settings())
}

/// The sentence `prefix i` shows for what is configured now.
pub fn note() -> &'static str {
    note_for(loads_user_settings())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_user_tier_loads_unless_configured_off() {
        // Nothing in this crate's tests calls `configure`, so this is the shipped default.
        assert!(loads_user_settings());
        assert_eq!(configured_cli_argument(), "user,project,local");
        assert_eq!(note(), NOTE_WITH_USER);
    }

    #[test]
    fn each_selection_has_its_own_argument_and_sentence() {
        assert_eq!(cli_argument(true), "user,project,local");
        assert_eq!(cli_argument(false), "project,local");
        assert_eq!(note_for(true), NOTE_WITH_USER);
        assert_eq!(note_for(false), NOTE_WITHOUT_USER);
        assert_ne!(NOTE_WITH_USER, NOTE_WITHOUT_USER);
    }

    #[test]
    fn the_sentences_say_what_loads_and_the_opt_out_names_its_key() {
        assert!(NOTE_WITH_USER.contains("~/.claude") && NOTE_WITH_USER.contains("user"));
        assert!(NOTE_WITHOUT_USER.contains("not loaded") && NOTE_WITHOUT_USER.contains("agent.user_settings"));
    }
}
