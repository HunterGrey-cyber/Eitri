//! The checks a candidate prefix and a generated file pass before anything is written. Both are called on a
//! worker thread: the kernel a replay builds is used and dropped there, and only `Send` values come back.

use std::path::PathBuf;

use super::Generated;
use crate::keymap::tmux::{self, TmuxEnv};
use crate::keymap::{Keymap, KeymapOp, LuaClaims};
use crate::lua::kernel::{refuse_panels, Kernel};

/// The replay's config directory. Nothing is there, so a `require` in the text has nothing to load.
const NO_CONFIG_DIR: &str = "/nonexistent/eitri-first-start";
/// The name a Lua error gives the text; `=` makes Lua print it as it stands.
const CHUNK_NAME: &str = "=generated init.lua";

/// Why a generated file was not what its data says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckFailure {
    /// The text is not Lua the kernel runs, or the kernel refused a call in it.
    Lua(String),
    OpsDiffer,
    SettingsDiffer,
}

/// What a text recorded when it ran.
struct Recorded {
    ops: Vec<KeymapOp>,
    settings: Vec<(String, String)>,
}

/// Runs `text` in a fresh kernel that has no host, no window and no init file.
fn recorded(text: &str) -> Result<Recorded, CheckFailure> {
    let kernel =
        Kernel::new(PathBuf::from(NO_CONFIG_DIR), refuse_panels).map_err(|e| CheckFailure::Lua(e.to_string()))?;
    kernel.run_checked_text(CHUNK_NAME, text).map_err(CheckFailure::Lua)?;
    let ops = kernel.keymap.borrow().ops().to_vec();
    let settings = kernel.config.borrow().entries();
    Ok(Recorded { ops, settings })
}

/// `ops` with each call's options in name order: an option's place in a Lua table means nothing.
fn with_sorted_opts(ops: &[KeymapOp]) -> Vec<KeymapOp> {
    let mut ops = ops.to_vec();
    for op in &mut ops {
        if let KeymapOp::Set { opts, .. } = op {
            opts.sort_by(|a, b| a.0.cmp(&b.0));
        }
    }
    ops
}

/// Runs the exact text of `generated` and compares what it recorded with the data it was rendered from: the calls
/// in order, the settings whatever their order. An error in `init.lua` stops the launch, so a file that cannot pass
/// this is not written.
pub fn replay(generated: &Generated) -> Result<(), CheckFailure> {
    let recorded = recorded(&generated.text())?;
    if with_sorted_opts(&recorded.ops) != with_sorted_opts(&generated.ops) {
        return Err(CheckFailure::OpsDiffer);
    }
    let mut expected = generated.settings.clone();
    expected.sort();
    if recorded.settings != expected {
        return Err(CheckFailure::SettingsDiffer);
    }
    Ok(())
}

/// Puts `generated`'s data through the window's own startup path, so a file the window would refuse is refused here
/// with the same message. `lua_panels` and `claims` are the window's; `tmux` is asked for only when the file leaves
/// the import on.
pub fn data_check(
    generated: &Generated,
    lua_panels: &[String],
    claims: &LuaClaims,
    tmux: impl FnOnce() -> TmuxEnv,
) -> Result<(), String> {
    let from_tmux = generated
        .settings
        .iter()
        .rev()
        .find(|(key, _)| key == tmux::SETTING)
        .map(|(_, value)| value.as_str());
    Keymap::for_startup(&generated.ops, lua_panels, from_tmux, tmux, claims).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::super::presets::test_matrix::{cases, choice, key, stock_table};
    use super::super::presets::{bound_in, generate, start_table};
    use super::super::{Build, Lang, PresetId};
    use super::*;
    use crate::keymap::OptValue;
    use crate::plugin::test_support::Scratch;
    use std::collections::BTreeMap;
    use std::path::Path;

    const WITH_COMPOSITOR: Build = Build {
        compositor_mode: true,
        nvim_prefix: true,
    };

    /// A generated file for `preset`, in the shape the screen's pressed-key path gives it.
    fn pressed(preset: PresetId, text: &str) -> Generated {
        let mut chosen = choice(preset);
        let pressed = key(text);
        chosen.prefix = Some(pressed);
        chosen.bound_as = bound_in(&start_table(preset, chosen.wm_mod, &stock_table()), &pressed);
        generate(&chosen, Build::THIS_BINARY, Lang::En)
    }

    fn just_ops(ops: Vec<KeymapOp>) -> Generated {
        Generated {
            lines: Vec::new(),
            ops,
            settings: Vec::new(),
        }
    }

    fn text_of(line: &str) -> Generated {
        Generated {
            lines: vec![line.to_string()],
            ops: Vec::new(),
            settings: Vec::new(),
        }
    }

    fn prefix_op(text: &str) -> KeymapOp {
        KeymapOp::Prefix { key: text.to_string() }
    }

    /// A computer with no tmux configuration.
    fn no_tmux() -> TmuxEnv {
        TmuxEnv {
            home: None,
            env: BTreeMap::new(),
            system_file: PathBuf::from("/nonexistent/eitri-first-start/tmux.conf"),
        }
    }

    fn tmux_in(home: &Path) -> TmuxEnv {
        TmuxEnv {
            home: Some(home.to_path_buf()),
            env: BTreeMap::from([("HOME".to_string(), home.display().to_string())]),
            system_file: home.join("no-such-etc/tmux.conf"),
        }
    }

    #[test]
    fn every_generated_text_replays_to_its_data() {
        let cases = cases();
        assert!(cases.len() > 100, "the matrix is {} cases", cases.len());
        for case in cases {
            let generated = generate(&case.choice, case.build, case.lang);
            assert_eq!(replay(&generated), Ok(()), "{}", case.name);
        }
    }

    #[test]
    fn every_generated_file_passes_the_data_check() {
        for case in cases() {
            let generated = generate(&case.choice, case.build, case.lang);
            assert_eq!(
                data_check(&generated, &[], &LuaClaims::default(), no_tmux),
                Ok(()),
                "{}",
                case.name
            );
        }
    }

    #[test]
    fn the_pressed_key_path_gives_text_the_kernel_accepts() {
        for text in ["M-\"", "M-\\", "M-]", "M-'"] {
            let generated = pressed(PresetId::OtherPrefix, text);
            assert_eq!(replay(&generated), Ok(()), "{text}");
            let ops = recorded(&generated.text()).unwrap().ops;
            assert_eq!(ops, generated.ops, "{text}");
            assert_eq!(ops[0], prefix_op(text));
            assert_eq!(
                data_check(&generated, &[], &LuaClaims::default(), no_tmux),
                Ok(()),
                "{text}"
            );
        }
    }

    #[test]
    fn a_forced_mismatch_is_refused() {
        let mut generated = pressed(PresetId::OtherPrefix, "C-a");
        assert_eq!(replay(&generated), Ok(()));
        generated.ops[0] = prefix_op("C-z");
        assert_eq!(replay(&generated), Err(CheckFailure::OpsDiffer));

        let mut generated = pressed(PresetId::OtherPrefix, "C-a");
        generated.ops.pop();
        assert_eq!(replay(&generated), Err(CheckFailure::OpsDiffer));

        let mut generated = pressed(PresetId::OtherPrefix, "C-a");
        generated.ops.swap(0, 1);
        assert_eq!(
            replay(&generated),
            Err(CheckFailure::OpsDiffer),
            "the calls are compared in order"
        );

        let mut generated = generate(&choice(PresetId::NvimNoTmux), Build::THIS_BINARY, Lang::En);
        assert_eq!(replay(&generated), Ok(()));
        generated.settings[0].1 = "on".to_string();
        assert_eq!(replay(&generated), Err(CheckFailure::SettingsDiffer));

        let mut generated = generate(&choice(PresetId::NvimNoTmux), Build::THIS_BINARY, Lang::En);
        generated.settings.pop();
        assert_eq!(replay(&generated), Err(CheckFailure::SettingsDiffer));

        assert!(
            matches!(replay(&text_of("eitri.keymap.prefix(")), Err(CheckFailure::Lua(_))),
            "a truncated line is a Lua error"
        );
    }

    /// A call the kernel refuses is a failure even when the text catches the error, as it is for `init.lua`.
    #[test]
    fn a_refusal_the_text_catches_is_still_a_failure() {
        let Err(CheckFailure::Lua(message)) = replay(&text_of("pcall(eitri.config.set, {}, true)")) else {
            panic!("the refusal was not reported");
        };
        assert!(message.contains("eitri.config.set"), "{message}");
    }

    #[test]
    fn a_call_lua_could_not_record_differs_from_the_data() {
        assert_eq!(replay(&text_of("eitri.keymap.prefix(1)")), Err(CheckFailure::OpsDiffer));
    }

    #[test]
    fn require_finds_nothing_in_a_replay() {
        assert!(matches!(
            replay(&text_of("require('anything')")),
            Err(CheckFailure::Lua(_))
        ));
    }

    #[test]
    fn the_settings_are_compared_whatever_their_order() {
        let mut chosen = choice(PresetId::NvimNoTmux);
        chosen.nvim_lhs = Some("<leader>W");
        let mut generated = generate(&chosen, Build::THIS_BINARY, Lang::En);
        assert!(generated.settings.len() > 1, "{:?}", generated.settings);
        generated.settings.reverse();
        assert_eq!(replay(&generated), Ok(()));
    }

    #[test]
    fn a_bundles_opts_are_compared_sorted() {
        let mut generated = generate(&choice(PresetId::I3Style), Build::THIS_BINARY, Lang::En);
        let mut reversed = 0;
        for op in &mut generated.ops {
            if let KeymapOp::Set { opts, .. } = op {
                if opts.len() > 1 {
                    opts.reverse();
                    reversed += 1;
                }
            }
        }
        assert!(reversed > 0, "the bundle has a row with two options");
        assert_eq!(replay(&generated), Ok(()));

        let changed = generated.ops.iter_mut().find_map(|op| match op {
            KeymapOp::Set { opts, .. } if opts.len() > 1 => Some(opts),
            _ => None,
        });
        changed.unwrap()[0].1 = OptValue::Int(7);
        assert_eq!(
            replay(&generated),
            Err(CheckFailure::OpsDiffer),
            "sorting hides no value"
        );
    }

    #[test]
    fn replay_runs_on_a_plain_thread_and_touches_no_file() {
        fn sendable<T: Send>(value: T) -> T {
            value
        }
        let home = std::env::var_os("HOME");
        let config_dir = Path::new(NO_CONFIG_DIR);
        assert!(!config_dir.exists());
        let generated = sendable(generate(&choice(PresetId::NvimNoTmux), WITH_COMPOSITOR, Lang::ZhCn));
        let outcome = std::thread::spawn(move || sendable(replay(&generated))).join().unwrap();
        assert_eq!(outcome, Ok(()));
        assert!(!config_dir.exists());
        assert_eq!(std::env::var_os("HOME"), home);
    }

    #[test]
    fn data_check_refuses_what_for_startup_refuses() {
        for (text, says) in [
            ("C-h", "already a root key"),
            ("x", "must be a chord"),
            ("C-S-x", "uses S- on a character"),
        ] {
            let generated = just_ops(vec![prefix_op(text)]);
            let refused = data_check(&generated, &[], &LuaClaims::default(), no_tmux).unwrap_err();
            assert!(refused.contains(says), "{text}: {refused}");
            let startup = Keymap::for_startup(&generated.ops, &[], None, no_tmux, &LuaClaims::default());
            assert_eq!(Some(refused), startup.err(), "{text}");
        }
    }

    #[test]
    fn data_check_with_a_tmux_prefix_lets_init_lua_win() {
        let home = Scratch::new("first-start-check");
        std::fs::write(
            home.0.join(".tmux.conf"),
            "unbind C-b\nset -g prefix C-a\nbind C-a send-prefix\n",
        )
        .unwrap();
        let mut chosen = choice(PresetId::OtherPrefix);
        chosen.prefix = Some(key("C-Space"));
        chosen.tmux_prefix = Some(key("C-a"));
        chosen.tmux_config_found = true;
        chosen.running_prefix = key("C-a");
        let generated = generate(&chosen, Build::THIS_BINARY, Lang::En);
        let tmux = || tmux_in(&home.0);
        assert_eq!(data_check(&generated, &[], &LuaClaims::default(), tmux), Ok(()));
        let (map, import) = Keymap::for_startup(&generated.ops, &[], None, tmux, &LuaClaims::default()).unwrap();
        assert_eq!(*map.prefix(), key("C-Space"));
        assert_eq!(import.unwrap().prefix, None);
    }

    #[test]
    fn data_check_reads_the_import_setting_from_the_file() {
        let generated = generate(&choice(PresetId::NvimNoTmux), Build::THIS_BINARY, Lang::En);
        let tmux = || -> TmuxEnv { panic!("a file that turns the import off does not read tmux") };
        assert_eq!(data_check(&generated, &[], &LuaClaims::default(), tmux), Ok(()));

        let mut generated = generated;
        generated.settings = vec![(tmux::SETTING.to_string(), "maybe".to_string())];
        let refused = data_check(&generated, &[], &LuaClaims::default(), no_tmux).unwrap_err();
        assert!(
            refused.contains(tmux::SETTING) && refused.contains("\"maybe\""),
            "{refused}"
        );
    }

    #[test]
    fn data_check_hands_the_panels_through() {
        let generated = just_ops(vec![KeymapOp::Set {
            table: "prefix".to_string(),
            key: "F5".to_string(),
            action: "module.notes".to_string(),
            opts: Vec::new(),
        }]);
        let check = |panels: &[String]| data_check(&generated, panels, &LuaClaims::default(), no_tmux);
        assert!(check(&[]).is_err());
        assert_eq!(check(&["notes".to_string()]), Ok(()));
    }
}
