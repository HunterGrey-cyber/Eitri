//! `init.lua`'s settings, checked before anything reads them: a value that is not one, and a key nothing reads, is a
//! line in the kernel's startup problems and no more. A bad value is taken out of the store, so every reader that
//! comes after finds the key unset and uses its default; the window opens and says what it did.
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
        (prefs::REVIEW_ENABLED_KEY, check(prefs::parse_review_enabled)),
        (prefs::REVIEW_HINT_KEY, check(prefs::parse_review_hint)),
        (prefs::THINKING_KEY, check(prefs::parse_thinking_display)),
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
        (cfg::WINDOW_RESTORE_KEY, check(cfg::parse_window_restore)),
        (cfg::EDITOR_RESTORE_KEY, check(cfg::parse_editor_restore)),
        (cfg::EDITOR_PRESENTATION_KEY, check(cfg::parse_editor_presentation)),
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
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let mut unknown = Vec::new();
        for dir in ["core/src", "panel/src", "shell/src", "agent/src", "mac/src"] {
            let mut stack = vec![root.join(dir)];
            while let Some(path) = stack.pop() {
                let Ok(entries) = std::fs::read_dir(&path) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        stack.push(path);
                    } else if path.extension().is_some_and(|e| e == "rs") {
                        let text = std::fs::read_to_string(&path).unwrap_or_default();
                        let production = text.split("#[cfg(test)]").next().unwrap_or_default();
                        for line in production.lines() {
                            let declared = line.contains("const ") && line.contains("&str = \"");
                            let read = line.contains("config.get(\"") || line.contains(".get(\"agent.");
                            if !(declared || read) {
                                continue;
                            }
                            for literal in line.split('"').skip(1).step_by(2) {
                                let family = literal.split('.').next().unwrap_or_default();
                                let key_like = literal.contains('.')
                                    && literal.chars().all(|c| c.is_ascii_lowercase() || c == '_' || c == '.');
                                if key_like
                                    && families.contains(&family)
                                    && !known.contains(&literal)
                                    && !literal.starts_with(eitri_core::editor_tools::switches::TOOL_KEY_PREFIX)
                                {
                                    unknown.push(format!("{}: {literal}", path.display()));
                                }
                            }
                        }
                    }
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
