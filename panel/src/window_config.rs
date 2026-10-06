//! Every `init.lua` key a window reads at startup, in one place.
//!
//! `build_ui` used to read these inline. They are a unit of their own so a window that is not the
//! full editor-plus-agent one reads exactly the same keys, with the same startup-failure texts: a
//! bad value is a hard failure naming its key, never a silent fallback.

use std::rc::Rc;

use crate::agent_panel::{AgentPanelHandle, ReviewConfig};
use eitri_core::lua::kernel::Kernel;

/// A Lua panel as `load` needs to know it: the id `module.<id>` may name, and the key its owner
/// gave it. A host with no Lua panels passes none. (`shell` keeps the panels themselves, which hold
/// widgets, in its own registry and maps each entry to one of these.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LuaPanelMeta {
    pub id: String,
    pub key: Option<String>,
}

/// What `init.lua` decided for this window.
pub struct WindowConfig {
    pub panel_font_size: f32,
    pub typing_cadence: Option<u32>,
    pub restore_policy: eitri_core::tab_restore::RestorePolicy,
    pub default_mode: Option<eitri_core::agent_bridge::SessionModeChoice>,
    pub on_permission: eitri_core::attention::ChatOnPermission,
    pub keymap: Rc<eitri_core::keymap::Keymap>,
    pub tmux_import: Option<eitri_core::keymap::TmuxImport>,
    pub tmux_skipped: Vec<eitri_core::keymap::HelpRow>,
    /// Which window manager the companion window asks to move focus (`companion.wm`). Read by every
    /// window so a bad value fails the same way everywhere; only the companion window uses it.
    pub companion_wm: eitri_core::wm::WmChoice,
    /// `review.enabled` and `review.hint`: whether turn review runs, and whether a finished turn
    /// puts a hint in the status band.
    pub review: ReviewConfig,
}

/// How big the agent panel's text is. One number: every other size in `index.css` is a ratio of
/// it, so this rescales the panel coherently instead of moving one label. It is NOT taken from
/// nvim's `guifont` height, which is the editor's size for a MONOSPACE face while the panel is
/// mostly proportional prose -- following it 1:1 would make the two panes disagree by however
/// much the two faces disagree at the same nominal size. Unset changes nothing.
///
/// Out of range is a startup failure naming the key, the same discipline as `agent.account`:
/// silently clamping a number someone typed is how a knob gets reported as broken. `Err` is the
/// text after "eitri: ".
pub fn parse_panel_font_size(raw: Option<&str>) -> Result<f32, String> {
    match raw {
        None => Ok(eitri_core::theme::DEFAULT_PANEL_FONT_SIZE_PX),
        Some(raw) => match raw.trim().parse::<f32>() {
            Ok(px) if eitri_core::theme::PANEL_FONT_SIZE_RANGE_PX.contains(&px) => {
                eprintln!("[panel] font size {px}px (init.lua's agent.font_size)");
                Ok(px)
            }
            Ok(px) => Err(format!(
                "eitri.config.set(\"agent.font_size\", {raw:?}): {px} is outside {:?}",
                eitri_core::theme::PANEL_FONT_SIZE_RANGE_PX
            )),
            Err(e) => Err(format!(
                "eitri.config.set(\"agent.font_size\", {raw:?}): not a number ({e})"
            )),
        },
    }
}

/// `companion.wm`: `"auto"` (the default), `"hyprland"`, `"sway"`, `"niri"`, `"gnome"` or `"none"`; anything
/// else is a startup failure naming the key, like `agent.font_size`. `Err` is the text after
/// "eitri: ".
pub fn parse_companion_wm(raw: Option<&str>) -> Result<eitri_core::wm::WmChoice, String> {
    eitri_core::wm::parse_config(raw)
}

/// The account this window spends, from `VERDANDI_CLAUDE_ACCOUNT` or else `init.lua`'s
/// `agent.account`, with the rest of the environment (`HOME`, `VERDANDI_CLAUDE_CONFIG_DIR`) read
/// through `env`. `Err` is the text after "eitri: " and names the source of the bad name.
pub fn resolve_account(
    config: &eitri_core::lua::config::ConfigStore,
    env: impl Fn(&str) -> Option<String>,
) -> Result<Option<(agent::account::ClaudeAccount, agent::account::AccountSource)>, String> {
    let from_env = env("VERDANDI_CLAUDE_ACCOUNT");
    agent::account::resolve_for_from(from_env.as_deref(), config.get("agent.account"), env)
        .map_err(|err| format!("the configured claude account is unusable: {err}"))
}

/// What `init.lua` registered that the tmux import must leave alone: each Lua panel's key and each
/// Lua command's keybinding.
fn lua_claims(kernel: &Kernel, panels: &[LuaPanelMeta]) -> eitri_core::keymap::LuaClaims {
    eitri_core::keymap::LuaClaims {
        panel_keys: panels
            .iter()
            .filter_map(|p| p.key.clone().map(|key| (p.id.clone(), key)))
            .collect(),
        command_keybindings: kernel
            .commands
            .borrow()
            .iter()
            .filter_map(|(id, entry)| entry.keybinding.clone().map(|k| (id.clone(), k)))
            .collect(),
    }
}

/// Reads every `init.lua` key a window needs, in today's order, logging what it logs today, and
/// applies the two process-wide ones (`agent.user_settings`, `agent.account`). `Err` is the text
/// after "eitri: ".
///
/// The account is read here, once: this is the first point where `init.lua` has run, and still
/// before anything reads a transcript or starts a sidecar (the panel computes its greeting from a
/// WebView `ready` signal, i.e. after the main loop starts). Two sources, and the environment wins:
/// `eitri --account <name>` (and this host's own `VERDANDI_CLAUDE_ACCOUNT`, exported for Verdandi
/// and inherited by every `eitri` started from a terminal) arrives as that variable, and
/// `init.lua`'s `eitri.config.set("agent.account", "<name>")` is the per-machine default underneath
/// it -- which is what pins the account for a launch from the app menu, where no shell
/// configuration has run. Nothing set anywhere is the shipped default and changes nothing. A name
/// that is malformed or points at no directory is a hard startup failure naming the source -- never
/// a silent fallback to "whichever shell launched this window", which is the accident that made a
/// resumed session open empty on 2026-09-21 (see `agent::account`).
pub fn load(kernel: &Kernel, panels: &[LuaPanelMeta]) -> Result<WindowConfig, String> {
    let panel_font_size = parse_panel_font_size(kernel.config.borrow().get("agent.font_size"))?;

    // How often the agent panel's stream reaches its page while the user types in the editor
    // (an even cadence, not a hold; `eitri_core::panel_cadence`). Unset is `DEFAULT_CADENCE_HZ` (5)
    // a second; `"off"` is today's full rate; anything else is a startup failure naming the key,
    // like `agent.font_size` above.
    let typing_cadence =
        eitri_core::panel_cadence::parse_config(kernel.config.borrow().get(eitri_core::panel_cadence::CADENCE_KEY))?;
    match typing_cadence {
        Some(hz) => eprintln!("[panel] stream cadence while typing in the editor: {hz}/s"),
        None => eprintln!("[panel] stream cadence while typing in the editor: off (full rate)"),
    }

    // Whether the launch offers the last window's tabs back (`agent.restore`: "offer", the default,
    // "auto" or "off"), and the mode new tabs start in (`agent.default_mode`: "auto" or "bypass").
    // Anything else is a startup failure naming the key, like `agent.font_size` above. Naming bypass
    // here is the one way a window starts in bypass without asking, because the answer is in a file
    // the user wrote.
    let restore_policy = eitri_core::tab_restore::RestorePolicy::parse(
        kernel.config.borrow().get(eitri_core::tab_restore::RESTORE_KEY),
    )?;
    let default_mode = eitri_core::agent_prefs::parse_default_mode(
        kernel.config.borrow().get(eitri_core::agent_prefs::DEFAULT_MODE_KEY),
    )?;
    if let Some(mode) = default_mode {
        eprintln!(
            "[agent] new tabs start in {} (init.lua's agent.default_mode)",
            mode.as_str()
        );
    }

    // Whether sessions load the user's own Claude Code configuration (`agent.user_settings`: true,
    // the default, or false), pinned for the whole process before any session starts; the panel's
    // `prefix i` note and both backends read the one answer. Anything else is a startup failure
    // naming the key, like `agent.font_size` above.
    let user_settings = eitri_core::agent_prefs::parse_user_settings(
        kernel.config.borrow().get(eitri_core::agent_prefs::USER_SETTINGS_KEY),
    )?;
    agent::setting_sources::configure(user_settings);
    if !user_settings {
        eprintln!("[agent] sessions do not load the user's own settings (init.lua's agent.user_settings = false)");
    }

    // What a card for a hidden chat does (the tray's chip and a toast, or with `reveal` the chat
    // itself). Anything but `badge`/`reveal` is a startup failure naming the key, like
    // `agent.font_size` above.
    let on_permission = eitri_core::attention::ChatOnPermission::parse(
        kernel.config.borrow().get(eitri_core::attention::ChatOnPermission::KEY),
    )?;

    if let Some((account, source)) = resolve_account(&kernel.config.borrow(), |key| std::env::var(key).ok())? {
        eprintln!(
            "[account] claude account '{}' from {source} -> {}",
            account.name(),
            account.config_dir().display()
        );
        agent::account::configure(account);
    }

    // The keymap (keymap spec §2.3): stock tmux's defaults, prefix `Ctrl+b`, then the user's own
    // tmux config read from tmux's files (unless `keymap.from_tmux` is "off"), then `init.lua`'s
    // `eitri.keymap` calls. The tmux import never stops the window from opening: what it cannot
    // take is listed in the `?` overlay. A bad key, an unknown action or option, or a collision in
    // `init.lua` is a startup failure naming both sides, as `agent.font_size` is -- never a keymap
    // nobody wrote. The import yields to what `init.lua` registered, so a config that started
    // before it still starts.
    let lua_panel_ids: Vec<String> = panels.iter().map(|p| p.id.clone()).collect();
    let lua_claims = lua_claims(kernel, panels);
    let from_tmux = kernel
        .config
        .borrow()
        .get(eitri_core::keymap::tmux::SETTING)
        .map(str::to_owned);
    let home_dir = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let (keymap, tmux_import) = match eitri_core::keymap::Keymap::for_startup(
        kernel.keymap.borrow().ops(),
        &lua_panel_ids,
        from_tmux.as_deref(),
        eitri_core::keymap::tmux::TmuxEnv::from_process,
        &lua_claims,
    ) {
        Ok((keymap, import)) => (Rc::new(keymap), import),
        Err(err) => return Err(err.to_string()),
    };
    eprintln!(
        "{}",
        eitri_core::keymap::tmux::notice::log_line(tmux_import.as_ref(), home_dir.as_deref())
    );
    let tmux_skipped: Vec<eitri_core::keymap::HelpRow> = tmux_import
        .as_ref()
        .map(|import| eitri_core::keymap::tmux::notice::skipped_rows(import, home_dir.as_deref()))
        .unwrap_or_default();
    // Collision rule 4: a Lua command's accelerator that is the prefix or a root chord would never fire.
    for (id, entry) in kernel.commands.borrow().iter() {
        if let Some(keybinding) = &entry.keybinding {
            eitri_core::keymap::check_command_keybinding(id, keybinding, &keymap).map_err(|err| err.to_string())?;
        }
    }
    println!(
        "[keymap] prefix {} ({} bindings)",
        keymap.prefix(),
        keymap.bindings().len()
    );

    let companion_wm = parse_companion_wm(kernel.config.borrow().get(eitri_core::wm::CONFIG_KEY))?;

    // Whether turn review runs (`review.enabled`, default true) and whether a finished turn puts a
    // hint in the status band (`review.hint`, default false). Anything but true/false is a startup
    // failure naming the key, like `agent.user_settings` above.
    let review = ReviewConfig {
        enabled: eitri_core::agent_prefs::parse_review_enabled(
            kernel.config.borrow().get(eitri_core::agent_prefs::REVIEW_ENABLED_KEY),
        )?,
        hint: eitri_core::agent_prefs::parse_review_hint(
            kernel.config.borrow().get(eitri_core::agent_prefs::REVIEW_HINT_KEY),
        )?,
    };

    Ok(WindowConfig {
        panel_font_size,
        typing_cadence,
        restore_policy,
        default_mode,
        on_permission,
        keymap,
        tmux_import,
        tmux_skipped,
        companion_wm,
        review,
    })
}

impl WindowConfig {
    /// The settings that live on the panel itself: the typing cadence, the restore policy, the
    /// default mode for new tabs and the turn review switches.
    pub fn apply_to_panel(&self, panel: &AgentPanelHandle) {
        panel.set_typing_cadence(self.typing_cadence);
        panel.set_restore_policy(self.restore_policy);
        panel.set_default_mode(self.default_mode);
        panel.set_review(ReviewConfig {
            enabled: self.review.enabled,
            hint: self.review.hint,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_bad_value_keeps_its_startup_failure_text() {
        assert_eq!(
            parse_panel_font_size(Some("99")).unwrap_err(),
            "eitri.config.set(\"agent.font_size\", \"99\"): 99 is outside 9.0..=32.0"
        );
        let err = parse_panel_font_size(Some("x")).unwrap_err();
        assert!(
            err.starts_with("eitri.config.set(\"agent.font_size\", \"x\"): not a number ("),
            "{err}"
        );
    }

    /// `init.lua`'s review keys run through the real Lua engine: a boolean is read, a table is a
    /// startup failure naming the key.
    #[test]
    fn window_config_reads_both_review_keys() {
        let scratch = std::env::temp_dir().join(format!("eitri-review-keys-{}", uuid::Uuid::new_v4()));
        let config_dir = scratch.join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        let run = |lines: &str| {
            std::fs::write(config_dir.join("init.lua"), lines).unwrap();
            let engine = Kernel::new(config_dir.clone(), eitri_core::lua::kernel::refuse_panels).unwrap();
            engine.run_and_check_init_file(&config_dir.join("init.lua"))?;
            let config = engine.config.borrow();
            Ok::<_, String>((
                eitri_core::agent_prefs::parse_review_enabled(config.get(eitri_core::agent_prefs::REVIEW_ENABLED_KEY))?,
                eitri_core::agent_prefs::parse_review_hint(config.get(eitri_core::agent_prefs::REVIEW_HINT_KEY))?,
            ))
        };
        assert_eq!(run("-- nothing set\n"), Ok((true, false)));
        assert_eq!(run("eitri.config.set(\"review.hint\", true)\n"), Ok((true, true)));
        assert_eq!(run("eitri.config.set(\"review.enabled\", false)\n"), Ok((false, false)));
        assert!(run("eitri.config.set(\"review.hint\", {})\n").is_err());
        std::fs::remove_dir_all(&scratch).unwrap();
    }

    #[test]
    fn window_config_refuses_a_bad_companion_wm_by_name() {
        let err = parse_companion_wm(Some("i3")).unwrap_err();
        assert!(err.contains("companion.wm"), "{err}");
        assert!(err.contains("\"i3\""), "{err}");
        assert_eq!(parse_companion_wm(None), Ok(eitri_core::wm::WmChoice::Auto));
        assert_eq!(
            parse_companion_wm(Some("gnome")),
            Ok(eitri_core::wm::WmChoice::Fixed(eitri_core::wm::Wm::Gnome))
        );
        assert_eq!(
            parse_companion_wm(Some("sway")),
            Ok(eitri_core::wm::WmChoice::Fixed(eitri_core::wm::Wm::Sway))
        );
    }

    #[test]
    fn a_font_size_in_range_or_unset_is_accepted() {
        assert_eq!(
            parse_panel_font_size(None),
            Ok(eitri_core::theme::DEFAULT_PANEL_FONT_SIZE_PX)
        );
        assert_eq!(parse_panel_font_size(Some(" 14 ")), Ok(14.0));
    }

    /// `init.lua`'s `agent.account` run through the real Lua engine, from a scratch config
    /// directory, resolved against a scratch home: every bad value is a startup failure that names
    /// the key, never a window that quietly spends whichever account launched it.
    #[test]
    fn a_bad_agent_account_in_init_lua_is_a_startup_failure_naming_the_key() {
        let scratch = std::env::temp_dir().join(format!("eitri-account-test-{}", uuid::Uuid::new_v4()));
        let config_dir = scratch.join("config");
        let home = scratch.join("home");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::create_dir_all(home.join(".claude-scratch")).unwrap();
        let home_text = home.to_string_lossy().into_owned();
        let env = |key: &str| (key == "HOME").then(|| home_text.clone());

        let run = |value: &str| {
            std::fs::write(
                config_dir.join("init.lua"),
                format!("eitri.config.set(\"agent.account\", {value})\n"),
            )
            .unwrap();
            let engine = Kernel::new(config_dir.clone(), eitri_core::lua::kernel::refuse_panels).unwrap();
            engine.run_and_check_init_file(&config_dir.join("init.lua"))?;
            let config = engine.config.borrow();
            resolve_account(&config, env)
                .map(|resolved| resolved.map(|(account, source)| (account.name().to_string(), source)))
        };

        let malformed = run("\"../x\"").unwrap_err();
        assert!(malformed.contains("init.lua's agent.account"), "{malformed}");
        assert!(malformed.contains("\"../x\""), "{malformed}");

        let missing = run("\"nosuch\"").unwrap_err();
        assert!(missing.contains("init.lua's agent.account"), "{missing}");
        assert!(
            missing.contains(&format!("{}/.claude-nosuch", home.display())),
            "{missing}"
        );

        for not_a_string in ["{}", "function() end"] {
            let refused = run(not_a_string).unwrap_err();
            assert!(
                refused.starts_with("eitri.config.set(\"agent.account\", ...): a "),
                "{not_a_string}: {refused}"
            );
            assert!(refused.contains("init.lua"), "the file is named: {refused}");
        }
        // `nil` unsets, like an empty string: nothing pinned, nothing refused.
        assert_eq!(run("nil"), Ok(None));
        assert_eq!(run("\"\""), Ok(None));
        // Lua turns these into text; neither names an account directory, so the check refuses them.
        for coerced in ["42", "true"] {
            let refused = run(coerced).unwrap_err();
            assert!(refused.contains("init.lua's agent.account"), "{coerced}: {refused}");
            assert!(refused.contains("has no config directory"), "{coerced}: {refused}");
        }

        assert_eq!(
            run("\"scratch\"").unwrap(),
            Some(("scratch".to_string(), agent::account::AccountSource::InitLua))
        );
        std::fs::remove_dir_all(&scratch).unwrap();
    }

    /// The launcher's variable still outranks `init.lua`, and its refusal names it rather than the
    /// key it overrode.
    #[test]
    fn the_environment_outranks_init_lua_and_its_refusal_names_the_variable() {
        let scratch = std::env::temp_dir().join(format!("eitri-account-env-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&scratch).unwrap();
        std::fs::write(
            scratch.join("init.lua"),
            "eitri.config.set(\"agent.account\", \"work\")\n",
        )
        .unwrap();
        let kernel = Kernel::new(scratch.clone(), eitri_core::lua::kernel::refuse_panels).unwrap();
        kernel.run_and_check_init_file(&scratch.join("init.lua")).unwrap();
        std::fs::remove_dir_all(&scratch).unwrap();
        let store = &kernel.config;

        let env = |key: &str| match key {
            "VERDANDI_CLAUDE_ACCOUNT" => Some("../x".to_string()),
            "HOME" => Some("/nonexistent-home".to_string()),
            _ => None,
        };
        let err = resolve_account(&store.borrow(), env).unwrap_err();
        assert!(err.contains("VERDANDI_CLAUDE_ACCOUNT"), "{err}");
        assert!(!err.contains("init.lua"), "{err}");
    }

    /// Runs `init_lua` in a scratch config directory and loads the window config from it, with the
    /// tmux import off so nothing outside the scratch directory is read.
    fn load_from(init_lua: &str, panels: &[LuaPanelMeta]) -> Result<WindowConfig, String> {
        let scratch = std::env::temp_dir().join(format!("eitri-window-config-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&scratch).unwrap();
        let init = scratch.join("init.lua");
        std::fs::write(
            &init,
            format!("eitri.config.set(\"keymap.from_tmux\", \"off\")\n{init_lua}"),
        )
        .unwrap();
        let kernel = Kernel::new(scratch.clone(), eitri_core::lua::kernel::refuse_panels).unwrap();
        let ran = kernel.run_and_check_init_file(&init);
        let loaded = ran.and_then(|()| load(&kernel, panels));
        std::fs::remove_dir_all(&scratch).unwrap();
        loaded
    }

    /// A Lua panel's id is what lets `init.lua` bind `module.<id>`; the loader gets it from the host's
    /// list, so a host that passes none refuses the binding, naming the call.
    #[test]
    fn a_lua_panels_id_lets_the_keymap_name_its_module() {
        let init = "eitri.keymap.set(\"prefix\", \"N\", \"module.notes\")\n";
        let notes = LuaPanelMeta {
            id: "notes".into(),
            key: Some("n".into()),
        };
        let loaded = load_from(init, std::slice::from_ref(&notes)).expect("the panel's module is nameable");
        assert!(loaded
            .keymap
            .bindings()
            .iter()
            .any(|b| format!("{:?}", b.action).contains("notes")));
        let err = load_from(init, &[]).err().expect("no such panel");
        assert!(err.contains("module.notes"), "{err}");
    }

    /// A Lua panel's key is claimed against the tmux import (a tmux binding of that key is skipped, so a
    /// configuration that starts without the import still starts with it), alongside each Lua command's
    /// keybinding. This is the one place the claims are built, from the host's list and the kernel.
    #[test]
    fn a_lua_panels_key_and_a_commands_keybinding_become_claims() {
        let scratch = std::env::temp_dir().join(format!("eitri-claims-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&scratch).unwrap();
        let init = scratch.join("init.lua");
        std::fs::write(
            &init,
            "eitri.command.register({ id = \"hello\", title = \"Hello\", keybinding = \"<primary>g\", action = function() end })\n",
        )
        .unwrap();
        let kernel = Kernel::new(scratch.clone(), eitri_core::lua::kernel::refuse_panels).unwrap();
        kernel.run_and_check_init_file(&init).unwrap();
        std::fs::remove_dir_all(&scratch).unwrap();
        let panels = [
            LuaPanelMeta {
                id: "notes".into(),
                key: Some("n".into()),
            },
            LuaPanelMeta {
                id: "keyless".into(),
                key: None,
            },
        ];
        let claims = lua_claims(&kernel, &panels);
        assert_eq!(claims.panel_keys, [("notes".to_string(), "n".to_string())]);
        assert_eq!(
            claims.command_keybindings,
            [("hello".to_string(), "<primary>g".to_string())]
        );
    }
}
