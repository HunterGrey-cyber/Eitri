//! `init.lua`'s settings, checked before anything reads them: a value that is not one, and a key nothing reads, is a
//! line in the kernel's startup problems and no more. A bad value is taken out of the store, so every reader that
//! comes after finds the key unset and uses its default; the window opens and says what it did. A bad remote-control
//! key also turns remote control off: a typo must not open a network listener with fallback settings.
//!
//! The checks are the readers' own parsers (`window_config::load` and the shell's later reads call the same
//! functions), so what is refused here is exactly what would have stopped the start. Two settings are not here on
//! purpose: `agent.account`, because a window that fell back to some other account would open sessions under it, and
//! the environment variables that stand in for settings, which belong to whoever exported them.

use eitri_core::lua::kernel::Kernel;

use crate::window_config::{parse_companion_wm, parse_panel_font_size, parse_tiling};

/// `agent.account`: read by `window_config::resolve_account`, which refuses a bad one and does not fall back.
const ACCOUNT_KEY: &str = "agent.account";
/// `agent.font_size`.
const FONT_SIZE_KEY: &str = "agent.font_size";
/// The plugin pages' keyboard kit settings read by `kit_settings::from_config`.
const KIT_KEYS: [&str; 6] = [
    eitri_core::plugin::kit_settings::SCROLL_STEP_KEY,
    eitri_core::plugin::kit_settings::SMOOTH_KEY,
    eitri_core::plugin::kit_settings::PREFIX_WAIT_KEY,
    eitri_core::plugin::kit_settings::WHICH_KEY_DELAY_KEY,
    eitri_core::plugin::kit_settings::HINT_CHARS_KEY,
    eitri_core::plugin::kit_settings::FIND_CASE_KEY,
];

type Check<'a> = Box<dyn Fn(&str) -> Result<(), String> + 'a>;

/// Every setting with a parser: its key and what says whether a value is one. `agents` are the names
/// `eitri.agent.register` accepted, which `agent.default` may name.
fn checks<'a>(agents: &'a [&'a str]) -> Vec<(&'static str, Check<'a>)> {
    use eitri_core::{agent_prefs as prefs, lua::config as cfg};
    fn check<'a, T>(parse: impl Fn(Option<&str>) -> Result<T, String> + 'a) -> Check<'a> {
        Box::new(move |value| parse(Some(value)).map(|_| ()))
    }
    let mut all: Vec<(&'static str, Check<'a>)> = vec![
        (FONT_SIZE_KEY, check(parse_panel_font_size)),
        (
            eitri_core::panel_cadence::CADENCE_KEY,
            check(eitri_core::panel_cadence::parse_config),
        ),
        (
            eitri_core::tab_restore::RESTORE_KEY,
            check(eitri_core::tab_restore::RestorePolicy::parse),
        ),
        (prefs::DEFAULT_MODE_KEY, check(prefs::parse_default_mode)),
        (
            prefs::AGENT_DEFAULT_KEY,
            check(move |value| prefs::parse_agent_default(value, agents)),
        ),
        (prefs::CODEX_COMMAND_KEY, check(prefs::parse_codex_command)),
        (prefs::USER_SETTINGS_KEY, check(prefs::parse_user_settings)),
        (prefs::HISTORY_TURNS_KEY, check(prefs::parse_history_turns)),
        (
            eitri_core::attention::ChatOnPermission::KEY,
            check(eitri_core::attention::ChatOnPermission::parse),
        ),
        (
            eitri_core::keymap::hints::WHICH_KEY_DELAY_KEY,
            check(eitri_core::keymap::hints::parse_which_key_delay),
        ),
        (eitri_core::compositor::CONFIG_KEY, check(parse_companion_wm)),
        (prefs::EDITOR_FOLLOW_KEY, check(prefs::parse_editor_follow)),
        (eitri_core::compositor::mode::TILING_KEY, Box::new(tiling_check)),
        (prefs::AGENTS_ENABLED_KEY, check(prefs::parse_agents_enabled)),
        (prefs::REVIEW_ENABLED_KEY, check(prefs::parse_review_enabled)),
        (prefs::REVIEW_HINT_KEY, check(prefs::parse_review_hint)),
        (prefs::REVIEW_LIVE_KEY, check(prefs::parse_review_live)),
        (prefs::THINKING_KEY, check(prefs::parse_thinking_display)),
        (prefs::DIAGRAMS_KEY, check(prefs::parse_diagrams)),
        (
            eitri_core::editor_tools::switches::TOOLS_KEY,
            Box::new(|value| eitri_core::editor_tools::switches::Switches::parse(Some(value), &[]).map(|_| ())),
        ),
        (
            eitri_core::nvim_verbs::leader::NVIM_PREFIX_KEY,
            check(eitri_core::nvim_verbs::leader::parse_nvim_prefix),
        ),
        (
            eitri_core::keymap::tmux::SETTING,
            check(eitri_core::keymap::tmux::enabled),
        ),
        (cfg::TERMINAL_FONT_KEY, check(cfg::parse_terminal_font)),
        (cfg::TERMINAL_CLAUDE_IDE_KEY, check(cfg::parse_terminal_claude_ide)),
        (cfg::REMOTE_DEV_ENABLED_KEY, check(cfg::parse_remote_dev_enabled)),
        (cfg::REMOTE_DEV_FLUSH_CAP_KEY, check(cfg::parse_remote_dev_flush_cap)),
        (
            cfg::REMOTE_DEV_SEND_WAITS_FOR_FLUSH_KEY,
            check(cfg::parse_remote_dev_send_waits_for_flush),
        ),
        (cfg::REMOTE_DEV_MUTAGEN_KEY, check(cfg::parse_remote_dev_mutagen)),
        (
            cfg::REMOTE_DEV_TRIPWIRE_MIB_KEY,
            check(cfg::parse_remote_dev_tripwire_mib),
        ),
        (cfg::REMOTE_DEV_LSP_GRACE_KEY, check(cfg::parse_remote_dev_lsp_grace)),
        (cfg::REMOTE_DEV_LSP_LOCAL_KEY, check(cfg::parse_remote_dev_lsp_local)),
        (cfg::WINDOW_RESTORE_KEY, check(cfg::parse_window_restore)),
        (cfg::EDITOR_RESTORE_KEY, check(cfg::parse_editor_restore)),
        (cfg::EDITOR_TAKEOVER_KEY, check(cfg::parse_editor_takeover)),
        (cfg::EDITOR_PRESENTATION_KEY, check(cfg::parse_editor_presentation)),
        (cfg::DESKTOP_START_KEY, check(cfg::parse_desktop_start)),
        (cfg::REMOTE_ENABLED_KEY, check(cfg::parse_remote_enabled)),
        (cfg::REMOTE_LISTEN_KEY, check(cfg::parse_remote_listen)),
        (cfg::REMOTE_PORT_KEY, check(cfg::parse_remote_port)),
        (cfg::REMOTE_ORIGINS_KEY, check(cfg::parse_remote_origins)),
        (
            cfg::REMOTE_AUDIT_KEEP_DAYS_KEY,
            check(cfg::parse_remote_audit_keep_days),
        ),
        (cfg::REMOTE_NOTIFY_KEY, check(cfg::parse_remote_notify)),
    ];
    for key in KIT_KEYS {
        all.push((
            key,
            Box::new(move |value| {
                eitri_core::plugin::kit_settings::from_config(|asked| (asked == key).then(|| value.to_string()))
                    .map(|_| ())
            }),
        ));
    }
    all
}

/// `window.tiling`, and the macOS host's refusal of the compositor choice.
fn tiling_check(value: &str) -> Result<(), String> {
    let choice = parse_tiling(Some(value))?;
    crate::window_config::refuse_compositor_on_mac(choice, cfg!(target_os = "macos"))
}

/// Every key the window reads: the settings above, `agent.account`, and `agent.editor.tool.<name>`.
fn known_keys(agents: &[&str]) -> Vec<&'static str> {
    let mut keys: Vec<&'static str> = checks(agents).into_iter().map(|(key, _)| key).collect();
    keys.push(ACCOUNT_KEY);
    keys
}

/// How far apart two keys are before one is taken for a typo of the other: edits to turn one into the other.
fn distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let above = row[j + 1];
            row[j + 1] = (diagonal + usize::from(ca != cb)).min(row[j] + 1).min(above + 1);
            diagonal = above;
        }
    }
    row[b.len()]
}

/// The key `key` is most likely a typo of, when it is within three edits of one.
fn nearest<'a>(key: &str, known: &[&'a str]) -> Option<&'a str> {
    known
        .iter()
        .map(|candidate| (distance(key, candidate), *candidate))
        .filter(|(edits, _)| *edits <= 3)
        .min_by_key(|(edits, _)| *edits)
        .map(|(_, candidate)| candidate)
}

/// Whether `key` is one nothing reads and is plainly meant to be one of Eitri's: it is in a family of settings
/// Eitri has (`agent.`, `window.`, ...), or one typo from a key it reads. A key outside those is the person's own,
/// read by their own Lua with `eitri.config.get`, and says nothing.
fn is_unknown_setting(key: &str, known: &[&str]) -> bool {
    use eitri_core::editor_tools::switches::TOOL_KEY_PREFIX;
    if known.contains(&key) || key.starts_with(TOOL_KEY_PREFIX) {
        return false;
    }
    let family = key.split('.').next().unwrap_or(key);
    let families = known.iter().filter_map(|known| known.split('.').next());
    let in_a_family = key.contains('.') && families.into_iter().any(|name| name == family);
    in_a_family || nearest(key, known).is_some()
}

/// What a refused value of a setting becomes.
enum Fallback {
    /// The key reads as unset and its reader uses its default (every setting that only decides how things look, and
    /// the ones whose default is already the stricter side).
    Default,
    /// The key is given this stricter value, and the problem line says what is in force until it is fixed. A setting
    /// that decides what an agent may do or what is loaded must not land on the looser side because of a typo.
    Stricter { value: &'static str, said: &'static str },
}

fn fallback(key: &str) -> Fallback {
    use eitri_core::{agent_prefs as prefs, editor_tools::switches::TOOLS_KEY};
    match key {
        prefs::DEFAULT_MODE_KEY => Fallback::Stricter {
            value: "manual",
            said: "new tabs start in Manual until it is fixed",
        },
        prefs::USER_SETTINGS_KEY => Fallback::Stricter {
            value: "off",
            said: "sessions do not load your own Claude Code settings until it is fixed",
        },
        TOOLS_KEY => Fallback::Stricter {
            value: "off",
            said: "the agent's editor tools are off until it is fixed",
        },
        _ => Fallback::Default,
    }
}

/// What the person is told is in force in place of a refused value of `key`.
const DEFAULT_SAID: &str = "the default is used";
const TOOL_OFF_SAID: &str = "that editor tool is off until it is fixed";
const ALL_TOOLS_OFF_SAID: &str = "the agent's editor tools are all off until it is fixed";

/// Applies the stricter side for one editor tool key whose value or name is not one: a known tool with a bad value is
/// off; a name no tool has cannot be told from a mistyped tool the person meant to turn off, so every tool is.
fn tool_fallback(kernel: &Kernel, key: &str, name_is_known: bool) -> &'static str {
    use eitri_core::editor_tools::switches::TOOLS_KEY;
    if name_is_known {
        kernel.config.borrow_mut().set_text(key, "off");
        TOOL_OFF_SAID
    } else {
        kernel.config.borrow_mut().unset(key);
        kernel.config.borrow_mut().set_text(TOOLS_KEY, "off");
        ALL_TOOLS_OFF_SAID
    }
}

/// Checks every setting `init.lua` stored. A value its parser refuses is reported with where it was set and the
/// parser's own words, then replaced: by the stricter side for a setting that decides what an agent may do or what is
/// loaded (`fallback`), else by nothing, so it reads as unset. A key nothing reads is reported with the nearest one
/// that is read, and left alone. A value `eitri.config.set` refused to store for a setting with a stricter side gets
/// that side as well. Every line is also in `kernel.startup_problems()`.
pub fn settle(kernel: &Kernel) {
    use eitri_core::editor_tools::switches::{Switches, TOOL_KEY_PREFIX};
    let agents: Vec<String> = kernel.agents.borrow().agents().iter().map(|a| a.name.clone()).collect();
    let agent_names: Vec<&str> = agents.iter().map(String::as_str).collect();
    let known = known_keys(&agent_names);
    let stored = kernel.config.borrow().entries();
    for (key, value) in &stored {
        let origin = kernel
            .config
            .borrow()
            .origin(key)
            .map_or_else(|| format!("init.lua: {key}"), str::to_string);
        if let Some((_, check)) = checks(&agent_names).into_iter().find(|(known, _)| known == key) {
            if let Err(why) = check(value) {
                if key.starts_with("remote.") {
                    kernel.config.borrow_mut().unset(key);
                    kernel
                        .config
                        .borrow_mut()
                        .set_text(eitri_core::lua::config::REMOTE_ENABLED_KEY, "false");
                    kernel.report_problem(format!("{origin}: Remote control: off: {why}"));
                } else {
                    let said = match fallback(key) {
                        Fallback::Default => {
                            kernel.config.borrow_mut().unset(key);
                            DEFAULT_SAID
                        }
                        Fallback::Stricter { value, said } => {
                            kernel.config.borrow_mut().set_text(key, value);
                            said
                        }
                    };
                    kernel.report_problem(format!("{origin}: {why}; {said}"));
                }
            }
        } else if key.starts_with(TOOL_KEY_PREFIX) {
            let pair = [(key.clone(), value.clone())];
            if let Err(why) = Switches::parse(None, &pair) {
                let known_name = Switches::parse(None, &[(key.clone(), "on".to_string())]).is_ok();
                let said = tool_fallback(kernel, key, known_name);
                kernel.report_problem(format!("{origin}: {why}; {said}"));
            }
        } else if is_unknown_setting(key, &known) {
            let hint = nearest(key, &known).map_or_else(String::new, |near| format!(" (did you mean {near}?)"));
            kernel.report_problem(format!("{origin}: no setting {key:?}{hint}"));
        }
    }
    let refused_remote = kernel.config.borrow().refusals_under("remote.");
    if !refused_remote.is_empty() {
        kernel
            .config
            .borrow_mut()
            .set_text(eitri_core::lua::config::REMOTE_ENABLED_KEY, "false");
        for (key, why) in refused_remote {
            kernel.report_problem(format!("Remote control: off: {key} could not be stored ({why})"));
        }
    }
    // A value that could not be stored reads as unset, which for these settings is the looser side.
    for key in [
        eitri_core::agent_prefs::DEFAULT_MODE_KEY,
        eitri_core::agent_prefs::USER_SETTINGS_KEY,
        eitri_core::editor_tools::switches::TOOLS_KEY,
    ] {
        let refused = kernel.config.borrow().refusal_for(key);
        if let (Some(why), Fallback::Stricter { value, said }) = (refused, fallback(key)) {
            kernel.config.borrow_mut().set_text(key, value);
            kernel.report_problem(format!("init.lua: {key} could not be stored ({why}); {said}"));
        }
    }
    {
        use eitri_core::agent_prefs as prefs;
        let config = kernel.config.borrow();
        if config.get(prefs::REVIEW_LIVE_KEY).is_some()
            && prefs::parse_review_live(config.get(prefs::REVIEW_LIVE_KEY)) == Ok(true)
            && prefs::parse_review_enabled(config.get(prefs::REVIEW_ENABLED_KEY)) == Ok(false)
        {
            let why = "review.live has no effect while review.enabled is false";
            let origin = config.origin(prefs::REVIEW_LIVE_KEY).unwrap_or("init.lua");
            let problem = format!("{origin}: {why}");
            if !kernel.startup_problems().contains(&problem) {
                kernel.report_problem(problem);
            }
        }
    }
    let refused_tools = kernel.config.borrow().refusals_under(TOOL_KEY_PREFIX);
    for (key, why) in refused_tools {
        let known_name = Switches::parse(None, &[(key.clone(), "on".to_string())]).is_ok();
        let said = tool_fallback(kernel, &key, known_name);
        kernel.report_problem(format!("init.lua: {key} could not be stored ({why}); {said}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eitri_core::lua::kernel::refuse_panels;
    use std::path::PathBuf;

    fn settled(init: &str) -> (Kernel, Vec<String>) {
        let scratch = eitri_core::plugin::test_support::Scratch::new("startup-config");
        let file = scratch.0.join("init.lua");
        std::fs::write(&file, init).unwrap();
        let kernel = Kernel::new(PathBuf::from(&scratch.0), refuse_panels).unwrap();
        kernel.load_init_file(&file);
        settle(&kernel);
        let problems = kernel.startup_problems();
        (kernel, problems)
    }

    #[test]
    fn remote_development_values_and_typos_are_checked_at_startup() {
        use eitri_core::lua::config::{parse_remote_dev_enabled, REMOTE_DEV_ENABLED_KEY};
        for value in ["true", "false", "'on'", "'off'"] {
            let (kernel, problems) = settled(&format!("eitri.config.set('remote_dev.enabled', {value})"));
            assert!(problems.is_empty(), "{value}: {problems:?}");
            assert!(parse_remote_dev_enabled(kernel.config.borrow().get(REMOTE_DEV_ENABLED_KEY)).is_ok());
        }
        let (kernel, problems) = settled("eitri.config.set('remote_dev.enabled', 'maybe')");
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("remote_dev.enabled"), "{problems:?}");
        assert!(problems[0].contains("default is used"), "{problems:?}");
        assert_eq!(kernel.config.borrow().get(REMOTE_DEV_ENABLED_KEY), None);
        assert_eq!(parse_remote_dev_enabled(None), Ok(false));
        let (_, problems) = settled("eitri.config.set('remote_dev.enable', true)");
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("remote_dev.enabled"), "{problems:?}");
    }

    #[test]
    fn remote_sync_values_and_typos_are_checked_at_startup() {
        let (_, problems) = settled(
            "eitri.config.set('remote_dev.flush_cap', 2.5)\n\
             eitri.config.set('remote_dev.send_waits_for_flush', false)\n\
             eitri.config.set('remote_dev.mutagen', '/opt/mutagen/bin/mutagen')\n\
             eitri.config.set('remote_dev.tripwire_mib', 128)\n",
        );
        assert!(problems.is_empty(), "{problems:?}");
        for (key, value) in [
            ("remote_dev.flush_cap", "60"),
            ("remote_dev.send_waits_for_flush", "'maybe'"),
            ("remote_dev.mutagen", "'mutagen'"),
            ("remote_dev.tripwire_mib", "4"),
        ] {
            let (kernel, problems) = settled(&format!("eitri.config.set('{key}', {value})"));
            assert_eq!(problems.len(), 1, "{key}: {problems:?}");
            assert!(problems[0].contains(key), "{key}: {problems:?}");
            assert_eq!(kernel.config.borrow().get(key), None, "{key}");
        }
        let (_, problems) = settled("eitri.config.set('remote_dev.flushcap', 2)");
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("remote_dev.flush_cap"), "{problems:?}");
    }

    #[test]
    fn remote_language_server_values_and_typos_are_checked_at_startup() {
        let (_, problems) = settled(
            "eitri.config.set('remote_dev.lsp_grace', 3600)\n\
             eitri.config.set('remote_dev.lsp_local', 'lua_ls, taplo')\n",
        );
        assert!(problems.is_empty(), "{problems:?}");
        for (key, value) in [
            ("remote_dev.lsp_grace", "59"),
            ("remote_dev.lsp_grace", "'a while'"),
            ("remote_dev.lsp_local", "'caf\\u{e9}'"),
        ] {
            let (kernel, problems) = settled(&format!("eitri.config.set('{key}', {value})"));
            assert_eq!(problems.len(), 1, "{key}: {problems:?}");
            assert!(problems[0].contains(key), "{key}: {problems:?}");
            assert_eq!(kernel.config.borrow().get(key), None, "{key}");
        }
        let (_, problems) = settled("eitri.config.set('remote_dev.lsp_graze', 600)");
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("remote_dev.lsp_grace"), "{problems:?}");
    }

    #[test]
    fn a_wildcard_remote_listen_is_said_and_remote_stays_off() {
        use eitri_core::lua::config::{RemoteListen, RemoteSettings};
        let (kernel, problems) =
            settled("eitri.config.set('remote.enabled', true)\neitri.config.set('remote.listen', '0.0.0.0')\n");
        let read = RemoteSettings::read(&kernel.config.borrow()).unwrap();
        assert!(!read.enabled);
        assert_eq!(read.listen, RemoteListen::Loopback);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("Remote control: off:"));
        assert!(
            problems[0].contains("remote.listen") && problems[0].contains("wildcard"),
            "{problems:?}"
        );
        assert!(
            problems[0].contains("Remote control: off:") && problems[0].contains("init.lua:2:"),
            "{problems:?}"
        );
    }

    #[test]
    fn a_mistyped_remote_key_is_said() {
        let (_kernel, problems) = settled("eitri.config.set('remote.enable', true)\n");
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("remote.enabled"), "{problems:?}");
    }

    #[test]
    fn every_bad_remote_value_and_refused_store_keeps_remote_off() {
        use eitri_core::lua::config::RemoteSettings;
        for (key, bad) in [
            ("remote.enabled", "'maybe'"),
            ("remote.listen", "'0.0.0.0'"),
            ("remote.port", "0"),
            ("remote.origins", "'*'"),
            ("remote.audit.keep_days", "0"),
            ("remote.notify", "'maybe'"),
            ("remote.listen", "{}"),
            ("remote.port", "{}"),
            ("remote.origins", "{}"),
        ] {
            let (kernel, problems) = settled(&format!(
                "eitri.config.set('remote.enabled', true)\neitri.config.set('{key}', {bad})\n"
            ));
            assert!(
                !RemoteSettings::read(&kernel.config.borrow()).unwrap().enabled,
                "{key}: {bad}"
            );
            let off = problems
                .iter()
                .find(|line| line.contains("Remote control: off:"))
                .unwrap();
            assert!(off.contains(key), "{problems:?}");
            if bad == "{}" {
                let why = kernel.config.borrow().refusal_for(key).unwrap();
                assert!(off.contains(&why), "{problems:?}");
            }
        }
    }

    #[test]
    fn review_hint_accepts_key_and_reports_a_bad_value() {
        use eitri_core::agent_prefs as prefs;
        let (kernel, problems) = settled("eitri.config.set('review.hint', 'key')\n");
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(
            prefs::parse_review_hint(kernel.config.borrow().get(prefs::REVIEW_HINT_KEY)),
            Ok(prefs::ReviewHintLevel::Key)
        );
        let (kernel, problems) = settled("eitri.config.set('review.hint', 'perhaps')\n");
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].contains("review.hint") && problems[0].contains("perhaps"),
            "{problems:?}"
        );
        assert!(
            problems[0].contains("\"key\"") && problems[0].ends_with("the default is used"),
            "{problems:?}"
        );
        assert_eq!(kernel.config.borrow().get(prefs::REVIEW_HINT_KEY), None);
    }

    #[test]
    fn a_bad_review_live_opens_on_the_default_and_is_said() {
        use eitri_core::agent_prefs::{parse_review_live, REVIEW_LIVE_DEFAULT, REVIEW_LIVE_KEY};
        let (kernel, problems) = settled("eitri.config.set('review.live', 'perhaps')\n");
        assert_eq!(
            parse_review_live(kernel.config.borrow().get(REVIEW_LIVE_KEY)),
            Ok(REVIEW_LIVE_DEFAULT)
        );
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("review.live") && problems[0].contains("the default is used"));
        assert!(problems[0].contains("init.lua:1:"));
    }

    #[test]
    fn review_live_set_with_review_off_is_said_once() {
        for value in ["true", "on"] {
            let (kernel, problems) = settled(&format!(
                "eitri.config.set('review.enabled', false)\neitri.config.set('review.live', '{value}')\n"
            ));
            assert_eq!(problems.len(), 1, "{problems:?}");
            assert!(problems[0].ends_with("review.live has no effect while review.enabled is false"));
            assert!(problems[0].contains("init.lua:2:"));
            settle(&kernel);
            assert_eq!(kernel.startup_problems(), problems);
        }
        let (_, problems) =
            settled("eitri.config.set('review.enabled', false)\neitri.config.set('review.live', false)\n");
        assert!(problems.is_empty(), "{problems:?}");
    }

    #[test]
    fn review_live_unset_with_review_off_says_nothing() {
        let (_, problems) = settled("eitri.config.set('review.enabled', false)\n");
        assert!(problems.is_empty(), "{problems:?}");
    }

    #[test]
    fn invalid_history_turns_reports_a_problem_and_falls_back_to_200() {
        for raw in ["9", "2001", "x"] {
            let (kernel, problems) = settled(&format!("eitri.config.set(\"agent.history_turns\", {raw:?})\n"));
            assert_eq!(problems.len(), 1, "{problems:?}");
            assert!(problems[0].contains("agent.history_turns"));
            assert!(problems[0].contains("10..=2000"));
            assert!(problems[0].contains("the default is used"));
            assert_eq!(crate::window_config::load(&kernel, &[]).unwrap().history_turns, 200);
        }
        let (kernel, problems) = settled("eitri.config.set(\"agent.history_turns\", 10)\n");
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(
            eitri_core::agent_prefs::parse_history_turns(kernel.config.borrow().get("agent.history_turns")),
            Ok(10)
        );
        assert_eq!(
            crate::window_config::load(&kernel, &[]).unwrap().history_turns,
            agent::history_turns()
        );
    }

    #[test]
    fn a_bad_value_is_reported_where_it_was_set_and_reads_as_unset() {
        let (kernel, problems) = settled("eitri.config.set(\"ok\", 1)\n\neitri.config.set(\"agent.font_size\", 900)\n");
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("init.lua:3: "), "{problems:?}");
        assert!(problems[0].contains("agent.font_size"), "{problems:?}");
        assert!(problems[0].ends_with("the default is used"), "{problems:?}");
        assert_eq!(kernel.config.borrow().get("agent.font_size"), None);
        assert_eq!(
            kernel.config.borrow().get("ok"),
            Some("1"),
            "a key of the person's own stays"
        );
    }

    #[test]
    fn a_bad_value_is_reported_and_is_not_an_unknown_key() {
        let (kernel, problems) = settled("eitri.config.set(\"window.desktop_start\", \"later\")\n");
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("must be choose, last or cwd"), "{problems:?}");
        assert!(problems[0].ends_with("the default is used"), "{problems:?}");
        assert_eq!(kernel.config.borrow().get("window.desktop_start"), None);

        for good in ["choose", "last", "cwd"] {
            let (kernel, problems) = settled(&format!("eitri.config.set(\"window.desktop_start\", {good:?})\n"));
            assert!(problems.is_empty(), "{good}: {problems:?}");
            assert_eq!(kernel.config.borrow().get("window.desktop_start"), Some(good));
        }
    }

    #[test]
    fn editor_takeover_is_a_known_switch_that_refuses_other_values() {
        for good in ["true", "false", "\"on\"", "\"off\""] {
            let (_, problems) = settled(&format!("eitri.config.set(\"editor.takeover\", {good})\n"));
            assert!(problems.is_empty(), "{good}: {problems:?}");
        }
        let (kernel, problems) = settled("eitri.config.set(\"editor.takeover\", \"offer\")\n");
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("editor.takeover"), "{problems:?}");
        assert!(!problems[0].contains("no setting"), "{problems:?}");
        assert!(problems[0].ends_with("the default is used"), "{problems:?}");
        assert_eq!(kernel.config.borrow().get("editor.takeover"), None);
    }

    #[test]
    fn a_good_value_and_the_persons_own_keys_say_nothing() {
        let (kernel, problems) = settled(
            "eitri.config.set(\"agent.font_size\", 14)\neitri.config.set(\"mytool.color\", \"red\")\n\
             eitri.config.set(\"agent.editor.tool.open_file\", true)\n",
        );
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(kernel.config.borrow().get("agent.font_size"), Some("14"));
    }

    #[test]
    fn a_key_nothing_reads_names_the_one_it_is_near() {
        let (kernel, problems) =
            settled("eitri.config.set(\"agent.font_sizee\", 14)\neitri.config.set(\"windw.tiling\", \"eitri\")\n");
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems[0].contains("did you mean agent.font_size?"), "{problems:?}");
        assert!(problems[1].contains("did you mean window.tiling?"), "{problems:?}");
        assert_eq!(
            kernel.config.borrow().get("agent.font_sizee"),
            Some("14"),
            "left as it was"
        );
    }

    #[test]
    fn a_misspelled_tool_name_is_a_problem_and_a_known_one_is_not() {
        let (_, problems) = settled(
            "eitri.config.set(\"agent.editor.tool.open_fil\", true)\neitri.config.set(\"agent.editor.tool.open_file\", false)\n",
        );
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("open_fil"), "{problems:?}");
    }

    fn value_of(kernel: &Kernel, key: &str) -> Option<String> {
        kernel.config.borrow().get(key).map(str::to_string)
    }

    /// The settings that decide what an agent may do or what is loaded take the stricter side on a bad value, and say
    /// in words what is in force.
    #[test]
    fn a_bad_posture_setting_lands_on_the_stricter_side_and_says_so() {
        let (kernel, problems) = settled(
            "eitri.config.set(\"agent.default_mode\", \"manaul\")\n\
             eitri.config.set(\"agent.user_settings\", \"maybe\")\n\
             eitri.config.set(\"agent.editor.tools\", \"some\")\n\
             eitri.config.set(\"agent.editor.tool.open_file\", \"sure\")\n",
        );
        assert_eq!(value_of(&kernel, "agent.default_mode").as_deref(), Some("manual"));
        assert_eq!(value_of(&kernel, "agent.user_settings").as_deref(), Some("off"));
        assert_eq!(value_of(&kernel, "agent.editor.tools").as_deref(), Some("off"));
        assert_eq!(value_of(&kernel, "agent.editor.tool.open_file").as_deref(), Some("off"));
        assert_eq!(problems.len(), 4, "{problems:#?}");
        let said = problems.join("\n");
        for words in [
            "new tabs start in Manual until it is fixed",
            "do not load your own Claude Code settings until it is fixed",
            "editor tools are off until it is fixed",
            "that editor tool is off until it is fixed",
        ] {
            assert!(said.contains(words), "{words}: {problems:#?}");
        }
        assert!(problems.iter().all(|line| line.contains("init.lua:")), "{problems:#?}");
    }

    #[test]
    fn a_posture_setting_that_could_not_be_stored_lands_on_the_stricter_side_too() {
        let (kernel, problems) = settled(
            "pcall(eitri.config.set, \"agent.default_mode\", {})\n\
             pcall(eitri.config.set, \"agent.user_settings\", {})\n\
             pcall(eitri.config.set, \"agent.editor.tools\", {})\n\
             pcall(eitri.config.set, \"agent.editor.tool.read_buffer\", {})\n",
        );
        assert_eq!(value_of(&kernel, "agent.default_mode").as_deref(), Some("manual"));
        assert_eq!(value_of(&kernel, "agent.user_settings").as_deref(), Some("off"));
        assert_eq!(value_of(&kernel, "agent.editor.tools").as_deref(), Some("off"));
        assert_eq!(
            value_of(&kernel, "agent.editor.tool.read_buffer").as_deref(),
            Some("off")
        );
        // the refusal of each, and what is in force in its place
        assert_eq!(problems.len(), 8, "{problems:#?}");
    }

    #[test]
    fn a_name_no_editor_tool_has_turns_every_tool_off() {
        let (kernel, problems) = settled("eitri.config.set(\"agent.editor.tool.open_fil\", false)\n");
        assert_eq!(value_of(&kernel, "agent.editor.tools").as_deref(), Some("off"));
        assert_eq!(value_of(&kernel, "agent.editor.tool.open_fil"), None);
        assert_eq!(problems.len(), 1, "{problems:#?}");
        assert!(problems[0].contains("all off until it is fixed"), "{problems:#?}");
    }

    /// The settings that only decide how things look, or whose default is already the stricter side, read as unset.
    #[test]
    fn a_bad_setting_of_looks_reads_as_unset() {
        let (kernel, problems) = settled(
            "eitri.config.set(\"agent.thinking\", \"loud\")\n\
             eitri.config.set(\"terminal.claude_ide\", \"yes please\")\n\
             eitri.config.set(\"agent.restore\", \"autoo\")\n",
        );
        for key in ["agent.thinking", "terminal.claude_ide", "agent.restore"] {
            assert_eq!(value_of(&kernel, key), None, "{key}");
        }
        assert!(
            problems.iter().all(|line| line.ends_with("the default is used")),
            "{problems:#?}"
        );
    }

    #[test]
    fn a_syntax_error_names_the_file_and_line() {
        let (_, problems) = settled("eitri.config.set(\"agent.font_size\", 14)\n\nlocal = = 3\n");
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("init.lua:3: "), "{problems:?}");
        assert!(!problems[0].contains('\n'), "{problems:?}");
    }

    #[test]
    fn a_runtime_error_keeps_what_ran_before_it_and_names_the_line() {
        let (kernel, problems) = settled("eitri.config.set(\"agent.font_size\", 14)\nlocal x = nil\nx.y = 1\n");
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("init.lua:3: "), "{problems:?}");
        assert_eq!(kernel.config.borrow().get("agent.font_size"), Some("14"));
    }

    #[test]
    fn a_refused_call_is_a_problem_with_its_line_and_did_nothing() {
        let (kernel, problems) = settled("\npcall(eitri.config.set, {}, true)\neitri.config.set(\"a\", {})\n");
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems.iter().all(|line| line.contains("init.lua")), "{problems:?}");
        assert_eq!(kernel.config.borrow().get("a"), None);
    }

    /// Every setting the code reads is one `settle` knows, so none is reported as unknown. The list is the source's
    /// own: each `const ...: &str = "family.key"` and `config.get("family.key")` outside a test.
    #[test]
    fn every_setting_the_sources_read_is_known() {
        assert_settings_known(eitri_core::source_scan::PRODUCT_SOURCES);
    }

    #[test]
    #[should_panic(expected = "cannot read")]
    fn the_settings_scan_fails_if_a_source_directory_cannot_be_read() {
        assert_settings_known(&["desk/missing-source-directory"]);
    }

    fn setting_literals(text: &str) -> Vec<&str> {
        use eitri_core::source_scan::{code_only, word_ends};
        let code = code_only(text);
        let literal_after = |tail: &str, tokens: &[&str]| {
            let tail = tokens
                .iter()
                .try_fold(tail, |tail, token| tail.trim_start().strip_prefix(token))?;
            text[code.len() - tail.len()..]
                .trim_start()
                .strip_prefix('"')?
                .split_once('"')
                .map(|(key, _)| key)
        };
        let mut keys = Vec::new();
        for end in word_ends(&code, "const") {
            let tail = code[end..].trim_start();
            let tail = tail.trim_start_matches(|ch: char| ch.is_alphanumeric() || ch == '_');
            keys.extend(literal_after(tail, &[":", "&", "str", "="]));
        }
        for end in word_ends(&code, "get") {
            let Some(receiver) = code[..end - 3].trim_end().strip_suffix('.') else {
                continue;
            };
            if let Some(key) = literal_after(&code[end..], &["("]) {
                if receiver.trim_end().ends_with("config") || key.starts_with("agent.") {
                    keys.push(key);
                }
            }
        }
        keys
    }

    fn assert_config_reads(newline: &str) {
        let text = r#"const KEY: &str = "window.constant";
config.get("window.inline");
config
    . get(
        "window.split",
    );
other_config.get("window.other");
store.get("window.unrelated");
store.borrow().get("agent.borrowed");"#
            .replace('\n', newline);
        assert_eq!(
            setting_literals(&text),
            [
                "window.constant",
                "window.inline",
                "window.split",
                "window.other",
                "agent.borrowed"
            ]
        );
    }

    #[test]
    fn multi_line_config_get_read_is_found() {
        assert_config_reads("\n");
    }

    #[test]
    fn multi_line_config_get_read_is_found_crlf() {
        assert_config_reads("\r\n");
    }

    #[test]
    fn settings_reads_through_prefixed_config_receivers_are_found() {
        let source = "window_config.get(\"window.typo\");\nself.lua_config\n    .get(\"editor.typo\");";
        for newline in ["\n", "\r\n"] {
            assert_eq!(
                setting_literals(&source.replace('\n', newline)),
                ["window.typo", "editor.typo"]
            );
        }
    }

    fn assert_settings_known(dirs: &[&str]) {
        let known = known_keys(&[]);
        let families = [
            "agent",
            "window",
            "terminal",
            "editor",
            "keymap",
            "review",
            "companion",
            "plugins",
            "modules",
        ];
        let mut unknown = Vec::new();
        for source in eitri_core::source_scan::rust_sources(dirs) {
            let production = eitri_core::source_scan::production_text(&source.text);
            for literal in setting_literals(&production) {
                let family = literal.split('.').next().unwrap_or_default();
                let key_like =
                    literal.contains('.') && literal.chars().all(|c| c.is_ascii_lowercase() || c == '_' || c == '.');
                if key_like
                    && families.contains(&family)
                    && !known.contains(&literal)
                    && !literal.starts_with(eitri_core::editor_tools::switches::TOOL_KEY_PREFIX)
                {
                    unknown.push(format!("{}: {literal}", source.path));
                }
            }
        }
        // Event names share the families' spelling (`editor.closed`); they are not settings.
        unknown.retain(|line| {
            !line.contains("editor.closed") && !line.contains("editor.text") && !line.contains("editor.view")
        });
        assert!(
            unknown.is_empty(),
            "settings the code reads that `settle` does not know: {unknown:#?}"
        );
    }
}
