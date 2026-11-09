//! The prefix keys moved for a window whose sway or i3 uses Alt as `$mod`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::{bundles, KeySpec, Keymap, KeymapOp};
use crate::onboarding::{detect, WmMod};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Desktop {
    Sway,
    I3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Answer {
    pub desktop: Desktop,
    pub modifier: Option<WmMod>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Applied {
    pub desktop: Desktop,
}

pub fn detect(env: &dyn Fn(&str) -> Option<OsString>) -> Option<Desktop> {
    match detect::wm_name(env) {
        Some("sway") => Some(Desktop::Sway),
        Some("i3") => Some(Desktop::I3),
        _ => None,
    }
}

impl Desktop {
    pub fn name(self) -> &'static str {
        match self {
            Self::Sway => "sway",
            Self::I3 => "i3",
        }
    }

    pub fn argv(self) -> &'static [&'static str] {
        match self {
            Self::Sway => &["swaymsg", "-t", "get_config"],
            Self::I3 => &["i3-msg", "-t", "get_config"],
        }
    }
}

/// The running compositor's config wins; when it says nothing, follow the same config paths and includes as onboarding.
pub fn from_texts(
    desktop: Desktop,
    reply: Option<&str>,
    env: &dyn Fn(&str) -> Option<OsString>,
    read: &dyn Fn(&Path) -> Option<String>,
) -> Answer {
    let from_reply = reply.and_then(|text| match serde_json::from_str::<serde_json::Value>(text) {
        Ok(value) => value.get("config").and_then(|c| c.as_str()).and_then(detect::sway_mod),
        Err(_) => detect::sway_mod(text),
    });
    let modifier = from_reply.or_else(|| {
        detect::wm_config_paths(desktop.name(), env)
            .iter()
            .find_map(|path| detect::read_config(path, read))
            .and_then(|text| detect::sway_mod(&text))
    });
    Answer { desktop, modifier }
}

fn touched_key(op: &KeymapOp) -> Option<KeySpec> {
    match op {
        KeymapOp::Set { table, key, .. } | KeymapOp::Del { table, key }
            if matches!(table.as_str(), "prefix" | "root") =>
        {
            KeySpec::parse(key).ok()
        }
        _ => None,
    }
}

/// A single user choice anywhere in the moved set keeps the whole bundle in the user's hands. Expanded `use` calls
/// touch those same keys, so they opt out too. Even a call left out at startup remains an explicit choice. A key the
/// tmux import bound or unbound is a choice too: applied over it, the bundle would delete or replace the user's own
/// tmux binding without a word.
pub fn apply(map: &mut Keymap, answer: Option<Answer>, user: &[KeymapOp]) -> Option<Applied> {
    let answer = answer.filter(|answer| answer.modifier == Some(WmMod::Alt))?;
    let ops = bundles::bundle("i3-alt").expect("the built-in bundle exists");
    let moved: Vec<_> = ops.iter().filter_map(touched_key).collect();
    if user.iter().filter_map(touched_key).any(|key| moved.contains(&key))
        || moved.iter().any(|key| map.tmux_chose(key))
    {
        return None;
    }
    if !map.apply_alt_mod(&ops) {
        return None;
    }
    Some(Applied {
        desktop: answer.desktop,
    })
}

impl Applied {
    pub fn text(self) -> String {
        format!(
            "{}'s $mod is Alt, so Eitri moved its Alt keys after the prefix to Ctrl+Alt for this window; \
             eitri.keymap.use(\"i3-alt\") in init.lua makes it yours, eitri.keymap.del or a key of your own stops it.",
            self.desktop.name(),
        )
    }

    /// The desktop and the exact bundle operations, so a changed set of moves is news again.
    pub fn fingerprint(self) -> String {
        let text = format!("{}\n{:?}", self.desktop.name(), bundles::bundle("i3-alt").unwrap());
        Sha256::digest(text.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}

pub fn state_file(xdg_state_home: Option<&std::ffi::OsStr>, home: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    crate::state_paths::state_subdir(xdg_state_home, home, "keymap-alt-mod")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::{bundles, root_conflicts, KeySpec, Source};
    use crate::onboarding::detect;
    use crate::plugin::test_support::Scratch;

    fn alt() -> Option<Answer> {
        Some(Answer {
            desktop: Desktop::Sway,
            modifier: Some(WmMod::Alt),
        })
    }

    #[test]
    fn alt_mod_applies_the_bundle_with_its_own_source_and_no_user_notice() {
        let mut map = Keymap::defaults();
        assert!(apply(&mut map, alt(), &[]).is_some());
        let expected = Keymap::apply_user(&bundles::bundle("i3-alt").unwrap(), &[]).unwrap();
        let mut rows = map.bindings().to_vec();
        for row in &mut rows {
            if expected.lookup(&row.key).unwrap().source == Source::User {
                assert!(row.source.to_string().contains("Alt"));
                assert_ne!(row.source, Source::User);
                row.source = Source::User;
            }
        }
        assert_eq!(rows, expected.bindings());
        assert!(map.user_overrides().is_empty());
        let keys = crate::layout::ModuleKeys::build(&[], &map).unwrap();
        assert!(map.help(&keys).iter().any(|row| row.what.contains("Alt $mod")));
    }

    /// The tmux import's rows on a moved key are the user's, as `init.lua`'s are: the bundle leaves the whole table as
    /// it is. A tmux config that touches none of them still gets it.
    #[test]
    fn alt_mod_leaves_a_keymap_whose_tmux_import_touched_a_moved_key() {
        let imported = |conf: &str| {
            let home = crate::test_scratch_dir::ScratchDir::new("eitri-tmux", "alt-mod");
            std::fs::write(home.join(".tmux.conf"), conf).unwrap();
            let read = crate::keymap::tmux::read(&crate::keymap::tmux::tests::env_for(&home));
            Keymap::with_tmux(&read, &crate::keymap::LuaClaims::default()).0
        };
        for conf in [
            "bind M-1 select-layout tiled\n",
            "unbind M-H\n",
            "bind -r M-Up resize-pane -U 5\n",
        ] {
            let mut map = imported(conf);
            let before = map.clone();
            assert_eq!(apply(&mut map, alt(), &[]), None, "{conf}");
            assert_eq!(map, before, "{conf}");
        }
        let mut map = imported("bind M-y select-layout tiled\nunbind z\n");
        assert!(apply(&mut map, alt(), &[]).is_some());
    }

    #[test]
    fn alt_mod_decision_respects_user_choices_and_unanswered_or_non_alt_probes() {
        let bundle = bundles::bundle("i3-alt").unwrap();
        let no_change = |answer, ops: &[KeymapOp]| {
            let mut map = Keymap::defaults();
            let before = map.clone();
            assert_eq!(apply(&mut map, answer, ops), None, "{ops:?}");
            assert_eq!(map, before);
        };
        no_change(alt(), &bundle);
        for op in &bundle {
            let (KeymapOp::Set { key, .. } | KeymapOp::Del { key, .. }) = op else {
                unreachable!()
            };
            for table in ["prefix", "root"] {
                no_change(
                    alt(),
                    &[KeymapOp::Del {
                        table: table.into(),
                        key: key.clone(),
                    }],
                );
                no_change(
                    alt(),
                    &[KeymapOp::Set {
                        table: table.into(),
                        key: key.clone(),
                        action: "zoom".into(),
                        opts: vec![],
                    }],
                );
            }
        }
        no_change(
            alt(),
            &[KeymapOp::Del {
                table: "prefix".into(),
                key: "M-C-w".into(),
            }],
        );
        no_change(None, &[]);
        for modifier in [None, Some(WmMod::Super)] {
            no_change(
                Some(Answer {
                    desktop: Desktop::Sway,
                    modifier,
                }),
                &[],
            );
        }
        let mut map = Keymap::defaults();
        assert!(apply(&mut map, alt(), &[]).is_some(), "Alt with no user choice applies");
    }

    #[test]
    fn alt_mod_keeps_unrelated_user_keys_and_their_notices() {
        let ops = [KeymapOp::Set {
            table: "prefix".into(),
            key: "l".into(),
            action: "zoom".into(),
            opts: vec![],
        }];
        let mut map = Keymap::apply_user(&ops, &[]).unwrap();
        let notices = map.user_overrides();
        assert!(apply(&mut map, alt(), &ops).is_some());
        assert_eq!(map.user_overrides(), notices);
        assert_eq!(map.lookup(&KeySpec::parse("l").unwrap()).unwrap().source, Source::User);
    }

    #[test]
    fn alt_mod_reads_the_ipc_config_first_then_the_existing_config_reader() {
        let env = |name: &str| (name == "HOME").then(|| OsString::from("/home/u"));
        for desktop in [Desktop::Sway, Desktop::I3] {
            let read = |path: &Path| {
                if path == Path::new("/home/u/.config/sway/config") || path == Path::new("/home/u/.config/i3/config") {
                    Some("include /etc/alt-keys\n".into())
                } else if path == Path::new("/etc/alt-keys") {
                    Some("set $mod Mod1\n".into())
                } else {
                    None
                }
            };
            let super_reply = if desktop == Desktop::Sway {
                r#"{"config":"set $mod Mod4\n"}"#
            } else {
                "set $mod Mod4\n"
            };
            assert_eq!(
                from_texts(desktop, Some(super_reply), &env, &read).modifier,
                Some(WmMod::Super)
            );
            assert_eq!(
                from_texts(desktop, Some("set $mod Mod4\n"), &env, &read).modifier,
                Some(WmMod::Super)
            );
            for reply in [None, Some("unreadable"), Some(r##"{"config":"# no mod"}"##)] {
                assert_eq!(from_texts(desktop, reply, &env, &read).modifier, Some(WmMod::Alt));
            }
            assert_eq!(from_texts(desktop, None, &env, &|_| None).modifier, None);
        }
        assert_eq!(detect::sway_mod("set $mod Mod1"), Some(WmMod::Alt));
    }

    #[test]
    fn alt_mod_notice_names_the_desktop_and_never_echoes_reply_text() {
        let reply = serde_json::json!({ "config": "set $mod Mod1\n# UNTRUSTED_REPLY\u{202e}" }).to_string();
        for (desktop, name) in [(Desktop::Sway, "sway"), (Desktop::I3, "i3")] {
            let answer = from_texts(desktop, Some(&reply), &|_| None, &|_| None);
            let applied = apply(&mut Keymap::defaults(), Some(answer), &[]).unwrap();
            let text = applied.text();
            assert!(text.starts_with(&format!("{name}'s $mod is Alt")), "{text}");
            for word in [
                "Ctrl+Alt",
                "this window",
                "eitri.keymap.use(\"i3-alt\")",
                "init.lua",
                "eitri.keymap.del",
            ] {
                assert!(text.contains(word), "{text}");
            }
            assert!(!text.contains("UNTRUSTED_REPLY"));
            assert!(!text.contains('\u{202e}'));
        }
    }

    #[test]
    fn alt_mod_notice_record_is_once_per_change() {
        let scratch = Scratch::new("alt-mod-notice");
        let file = scratch.0.join("notice");
        let first = Applied { desktop: Desktop::Sway }.fingerprint();
        assert!(root_conflicts::needs_notice(&file, &first));
        root_conflicts::remember(&file, &first).unwrap();
        assert!(!root_conflicts::needs_notice(&file, &first));
        let changed = Applied { desktop: Desktop::I3 }.fingerprint();
        assert!(root_conflicts::needs_notice(&file, &changed));
        root_conflicts::remember(&file, &changed).unwrap();
        assert!(!root_conflicts::needs_notice(&file, &changed));
    }
}
