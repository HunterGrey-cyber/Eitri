//! The import end to end: tmux config text in a scratch home, the keymap and the skip list out.
//! No test here reads the real `~/.config/tmux`: every `TmuxEnv` points at a scratch directory.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::*;
use crate::keymap::{Action, KeySpec, Keymap, KeymapOp, LuaClaims, Source, SwapTarget, TabAction, TmuxImport};
use crate::layout::{Axis, Direction, ModuleId};
use crate::test_scratch_dir::ScratchDir;

pub(crate) fn env_for(home: &Path) -> TmuxEnv {
    let mut env = BTreeMap::new();
    env.insert("HOME".to_string(), home.display().to_string());
    TmuxEnv {
        home: Some(home.to_path_buf()),
        env,
        system_file: home.join("no-such-etc/tmux.conf"),
    }
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

struct Imported {
    _home: ScratchDir,
    home: PathBuf,
    map: Keymap,
    import: TmuxImport,
}

fn import_with(conf: &str, claims: &LuaClaims) -> Imported {
    let home = ScratchDir::new("eitri-tmux", "import");
    write(&home.join(".tmux.conf"), conf);
    let read = read(&env_for(&home));
    let (map, import) = Keymap::with_tmux(&read, claims);
    let path = home.to_path_buf();
    Imported {
        _home: home,
        home: path,
        map,
        import,
    }
}

fn import(conf: &str) -> Imported {
    import_with(conf, &LuaClaims::default())
}

fn k(name: &str) -> KeySpec {
    KeySpec::parse(name).unwrap()
}

impl Imported {
    fn bound(&self, key: &str) -> Option<(Action, bool)> {
        self.map.lookup(&k(key)).map(|b| (b.action.clone(), b.repeatable))
    }

    fn source(&self, key: &str) -> Option<Source> {
        self.map.lookup(&k(key)).map(|b| b.source)
    }

    fn reasons(&self) -> Vec<String> {
        self.import.skipped.iter().map(|s| s.reason.clone()).collect()
    }

    /// The one skip, for a config of one line that is skipped.
    fn only_reason(&self) -> String {
        assert_eq!(self.import.skipped.len(), 1, "{:?}", self.import.skipped);
        self.import.skipped[0].reason.clone()
    }
}

// --- What maps to what: one test per row of the mapping. -------------------------------------

#[test]
fn set_g_prefix_sets_the_prefix() {
    let got = import("set -g prefix C-a\n");
    assert_eq!(got.map.prefix(), &k("C-a"));
    assert_eq!(got.import.prefix, Some(k("C-a")));
    let got = import("set-option -g prefix M-a\n");
    assert_eq!(got.map.prefix(), &k("M-a"));
    // A later file or line wins, as in tmux.
    let got = import("set -g prefix C-a\nset -g prefix C-x\n");
    assert_eq!(got.map.prefix(), &k("C-x"));
}

#[test]
fn unbind_removes_a_key_and_an_unbound_one_is_no_error() {
    let got = import("unbind C-b\nunbind-key -T prefix %\nunbind q\n");
    assert_eq!(got.bound("C-b"), None);
    assert_eq!(got.bound("%"), None);
    assert!(got.import.skipped.is_empty(), "{:?}", got.import.skipped);
    assert_eq!(got.import.removed, vec![k("C-b"), k("%")]);
}

#[test]
fn unbind_all_removes_tmux_actions_and_keeps_eitris_own() {
    let got = import("bind m kill-pane\nunbind -a\nbind z kill-window\n");
    assert_eq!(got.bound("m"), None, "imported before -a");
    for gone in [
        "C-b", "%", "\"", "Up", "C-Up", ";", "o", "{", "M-1", "x", "c", "n", "p", "l", "&", "w", ",", ":", "C-l", "[",
        "PPage",
    ] {
        assert_eq!(got.bound(gone), None, "{gone}");
    }
    for kept in ["f", "e", "a", "t", "?", "r", "F11", "1", "i"] {
        assert!(got.bound(kept).is_some(), "{kept}: Eitri's own action stays");
    }
    assert_eq!(got.bound("z"), Some((Action::Tab(TabAction::Close), false)));
    assert!(got.import.removed.contains(&k("C-b")) && got.import.removed.contains(&k("m")));
    let got = import("unbind -a -T prefix\n");
    assert_eq!(got.bound("C-b"), None);
}

#[test]
fn send_prefix() {
    assert_eq!(
        import("bind C-a send-prefix\n").bound("C-a"),
        Some((Action::SendPrefix, false))
    );
}

#[test]
fn send_keys_of_a_key_every_pane_can_take() {
    assert_eq!(
        import("bind A send-keys C-a\n").bound("A"),
        Some((Action::SendKeys(k("C-a")), false))
    );
}

/// The panel's composer acts on a literal key only when it is `C-a` (select all); everything else
/// the terminal and the editor would take, the panel drops. So that is all the import accepts.
#[test]
fn send_keys_of_a_key_the_panel_drops_is_skipped() {
    for (conf, key) in [("bind C-l send-keys C-l\n", "C-l"), ("bind y send q\n", "y")] {
        let got = import(conf);
        assert!(got.only_reason().contains("cannot send"), "{conf}: {:?}", got.reasons());
        assert_eq!(got.source(key).unwrap_or(Source::Default), Source::Default, "{conf}");
    }
}

#[test]
fn the_panels_literal_key_rule_is_still_c_a_only() {
    let app = include_str!("../../../../agent-ui/web/src/App.tsx");
    assert!(
        app.contains("if (payload.key !== \"C-a\") return;"),
        "App.tsx's literal_key handling changed: re-decide what every_pane_takes accepts"
    );
}

#[test]
fn split_window() {
    let got = import("bind \\\\ split-window -h -c \"#{pane_current_path}\"\nbind - split-window -v\nbind _ splitw\n");
    assert_eq!(got.bound("\\"), Some((Action::Split(Axis::Row), false)));
    assert_eq!(got.bound("-"), Some((Action::Split(Axis::Column), false)));
    assert_eq!(got.bound("_"), Some((Action::Split(Axis::Column), false)));
    let got = import("bind | split-window -h -b -d -f -l 30% -p 20\n");
    assert_eq!(got.bound("|"), Some((Action::Split(Axis::Row), false)));
}

#[test]
fn select_pane_directions() {
    let got = import("bind h select-pane -L\nbind j select-pane -D\nbind k selectp -U\nbind L select-pane -R\n");
    assert_eq!(got.bound("h"), Some((Action::Select(Direction::Left), false)));
    assert_eq!(got.bound("j"), Some((Action::Select(Direction::Down), false)));
    assert_eq!(got.bound("k"), Some((Action::Select(Direction::Up), false)));
    assert_eq!(got.bound("L"), Some((Action::Select(Direction::Right), false)));
}

#[test]
fn select_pane_last_and_last_pane() {
    let got = import("bind b select-pane -l\nbind B last-pane\n");
    assert_eq!(got.bound("b"), Some((Action::SelectLast, false)));
    assert_eq!(got.bound("B"), Some((Action::SelectLast, false)));
}

#[test]
fn select_pane_next() {
    assert_eq!(
        import("bind N select-pane -t :.+\n").bound("N"),
        Some((Action::SelectNext, false))
    );
}

#[test]
fn resize_pane_with_and_without_a_count() {
    let got = import("bind -r h resize-pane -L 5\nbind J resize-pane -D\n");
    assert_eq!(
        got.bound("h"),
        Some((
            Action::Resize {
                dir: Direction::Left,
                cells: 5
            },
            true
        ))
    );
    assert_eq!(
        got.bound("J"),
        Some((
            Action::Resize {
                dir: Direction::Down,
                cells: 1
            },
            false
        ))
    );
}

#[test]
fn resize_pane_z_zooms() {
    assert_eq!(
        import("bind -r m resize-pane -Z\n").bound("m"),
        Some((Action::Zoom, true))
    );
}

#[test]
fn swap_pane_up_and_down() {
    let got = import("bind < swap-pane -U\nbind > swap-pane -D\n");
    assert_eq!(got.bound("<"), Some((Action::Swap(SwapTarget::Prev), false)));
    assert_eq!(got.bound(">"), Some((Action::Swap(SwapTarget::Next), false)));
}

#[test]
fn swap_pane_toward_a_neighbour() {
    let got = import(
        "bind H swap-pane -t '{left-of}'\nbind J swap-pane -t '{down-of}'\nbind K swap-pane -t '{up-of}'\nbind L swap-pane -t '{right-of}'\n",
    );
    assert_eq!(
        got.bound("H"),
        Some((Action::Swap(SwapTarget::Toward(Direction::Left)), false))
    );
    assert_eq!(
        got.bound("J"),
        Some((Action::Swap(SwapTarget::Toward(Direction::Down)), false))
    );
    assert_eq!(
        got.bound("K"),
        Some((Action::Swap(SwapTarget::Toward(Direction::Up)), false))
    );
    assert_eq!(
        got.bound("L"),
        Some((Action::Swap(SwapTarget::Toward(Direction::Right)), false))
    );
}

#[test]
fn select_layout_even() {
    let got = import("bind | select-layout even-horizontal\nbind _ selectl even-vertical\n");
    assert_eq!(got.bound("|"), Some((Action::Even(Axis::Row), false)));
    assert_eq!(got.bound("_"), Some((Action::Even(Axis::Column), false)));
}

#[test]
fn kill_pane_plain_and_inside_confirm_before() {
    let got = import(
        "bind X kill-pane\nbind Y confirm-before -p \"kill-pane #P? (y/n)\" kill-pane\nbind Z \"confirm kill-pane\"\n",
    );
    for key in ["X", "Y", "Z"] {
        assert_eq!(got.bound(key), Some((Action::ModuleKill, false)), "{key}");
    }
}

#[test]
fn kill_window_plain_and_inside_confirm_before() {
    let got = import("bind q kill-window\nbind Q confirm-before -y { kill-window }\n");
    assert_eq!(got.bound("q"), Some((Action::Tab(TabAction::Close), false)));
    assert_eq!(got.bound("Q"), Some((Action::Tab(TabAction::Close), false)));
}

#[test]
fn new_window() {
    let got = import("bind C new-window -c \"#{pane_current_path}\" -n x -a\n");
    assert_eq!(got.bound("C"), Some((Action::Tab(TabAction::New), false)));
}

#[test]
fn next_previous_and_last_window() {
    let got = import("bind N next-window\nbind P prev\nbind L last-window\n");
    assert_eq!(got.bound("N"), Some((Action::Tab(TabAction::Next), false)));
    assert_eq!(got.bound("P"), Some((Action::Tab(TabAction::Prev), false)));
    assert_eq!(got.bound("L"), Some((Action::Tab(TabAction::Last), false)));
}

#[test]
fn command_prompt_with_rename_window() {
    let got = import(
        "bind A command-prompt -I \"#W\" { rename-window -- \"%%\" }\nbind R command-prompt 'rename-window %%'\n",
    );
    assert_eq!(got.bound("A"), Some((Action::Tab(TabAction::Rename), false)));
    assert_eq!(got.bound("R"), Some((Action::Tab(TabAction::Rename), false)));
}

#[test]
fn choose_tree_window_and_session() {
    let got = import("bind W choose-tree\nbind S choose-session\nbind V choose-window\n");
    for key in ["W", "S"] {
        assert_eq!(got.bound(key), Some((Action::Tab(TabAction::Choose), false)), "{key}");
    }
    // `v` is the canvas's; `V` is just a key.
    assert_eq!(got.bound("V"), Some((Action::Tab(TabAction::Choose), false)));
}

#[test]
fn copy_mode_and_copy_mode_up() {
    let got = import("bind Escape copy-mode\nbind y copy-mode\nbind PPage copy-mode -u\n");
    assert_eq!(got.bound("y"), Some((Action::CopyMode { up: false }, false)));
    assert_eq!(got.bound("PPage"), Some((Action::CopyMode { up: true }, false)));
}

#[test]
fn command_prompt_alone_opens_the_command_line() {
    assert_eq!(
        import("bind \\; command-prompt\n").bound(";"),
        Some((Action::PanelCommandLine, false))
    );
}

#[test]
fn bind_r_is_repeatable_and_a_default_key_is_replaced_in_place() {
    let got = import("bind -r l resize-pane -R 5\nbind x kill-pane\n");
    assert_eq!(
        got.bound("l"),
        Some((
            Action::Resize {
                dir: Direction::Right,
                cells: 5
            },
            true
        ))
    );
    assert_eq!(got.source("l"), Some(Source::Tmux));
    assert_eq!(
        got.source("x"),
        Some(Source::Tmux),
        "a tmux bind of a default key is the user's tmux"
    );
    let position = |map: &Keymap, key: &str| map.bindings().iter().position(|b| b.key == k(key));
    assert_eq!(position(&got.map, "l"), position(&Keymap::defaults(), "l"));
}

// --- Every reason a line is skipped. ----------------------------------------------------------

/// `(config, what the reason says)`: each a one-line config that is skipped, and the reason.
const SKIPS: &[(&str, &str)] = &[
    ("bind -n C-h select-pane -L", "a root key"),
    ("bind -T root C-h select-pane -L", "a root key"),
    (
        "bind -T copy-mode-vi v send -X begin-selection",
        "the copy-mode-vi table is not imported",
    ),
    ("bind P paste-buffer", "no Eitri equivalent: paste-buffer"),
    ("bind y run-shell 'x'", "no Eitri equivalent: run-shell"),
    ("bind r source-file ~/.tmux.conf", "no Eitri equivalent: source-file"),
    ("bind k kill-pane -a", "flag -a not imported"),
    ("bind k kill-window -a", "flag -a not imported"),
    ("bind k split-window -h -Z", "flag -Z not imported"),
    ("bind k split-window -h htop", "argument htop not imported"),
    ("bind k send-keys -l abc", "flag -l not imported"),
    ("bind k send-keys -X cancel", "flag -X not imported"),
    ("bind k send-keys Enter", "Eitri cannot send Enter to every pane"),
    ("bind k send-keys C-l", "Eitri cannot send C-l to every pane"),
    ("bind k send-keys Up", "Eitri cannot send Up to every pane"),
    ("bind k send-keys M-x", "Eitri cannot send M-x to every pane"),
    ("bind k send-keys F5", "Eitri cannot send F5 to every pane"),
    ("bind k send-keys a b", "argument b not imported"),
    ("bind k send-prefix -2", "prefix2 is not imported"),
    ("set -g prefix2 C-s", "prefix2 is not imported"),
    ("set prefix C-a", "without -g"),
    ("set -gu prefix", "flag -u not imported"),
    ("bind k select-window -t 1", "select-window"),
    ("bind k select-pane", "no Eitri equivalent: select-pane"),
    ("bind k select-pane -t :.-", "-t :.- not imported"),
    ("bind k swap-pane -t 2", "-t 2 not imported"),
    ("bind k select-layout tiled", "no Eitri equivalent: select-layout tiled"),
    ("bind k resize-pane -L 900", "1 to 500"),
    ("bind k new-window htop", "argument htop not imported"),
    (
        "bind k command-prompt 'split-window %%'",
        "no Eitri equivalent: command-prompt",
    ),
    (
        "bind k confirm-before kill-session",
        "no Eitri equivalent: confirm-before kill-session",
    ),
    ("bind k kill-pane \\; kill-window", "more than one command"),
    ("bind k { kill-pane; kill-window }", "more than one command"),
    ("bind k", "binds the key to no command"),
    ("bind PageUp copy-mode", "is not a tmux key name"),
    ("bind k frobnicate", "tmux refuses it: unknown command frobnicate"),
    ("bind k kill", "tmux refuses it: ambiguous command kill"),
    ("bind -z k kill-pane", "tmux refuses it: unknown flag -z"),
    ("bind k kill-pane -t", "tmux refuses it: -t expects an argument"),
    ("bind '#{k}' kill-pane", "a format"),
    ("bind k '#{cmd}'", "a format"),
    ("run '~/.tmux/plugins/tpm/tpm'", "run-shell runs a shell command"),
    ("if-shell 'true' 'bind k kill-pane'", "if-shell runs a shell command"),
    ("%hidden SECRET=1", "%hidden is not read"),
    ("bind k \"unclosed", "tmux refuses it: a quote is not closed"),
    ("source-file -n ~/.other.conf", "-n only parses"),
    ("source-file -F '#{d}/x.conf'", "-F"),
    ("source-file -", "standard input"),
    ("source-file relative.conf", "a relative path"),
    ("source-file /no/such/file.conf", "no such file"),
    ("unbind -n C-h", "a root key"),
    ("unbind -T copy-mode-vi v", "the copy-mode-vi table is not imported"),
    ("unbind", "tmux refuses it: missing key"),
];

#[test]
fn every_skip_reason() {
    for (conf, reason) in SKIPS {
        let got = import(&format!("{conf}\n"));
        let found = got.only_reason();
        assert!(found.contains(reason), "{conf:?}: {found:?} should say {reason:?}");
        assert_eq!(got.import.skipped[0].origin.line, 1, "{conf}");
        assert_eq!(got.import.skipped[0].origin.text, *conf, "{conf}");
        assert_eq!(
            got.map.bindings(),
            Keymap::defaults().bindings(),
            "{conf}: nothing changed"
        );
    }
}

#[test]
fn options_assignments_and_other_commands_are_not_keys_and_not_listed() {
    let got = import("set -g mouse on\nsetw -g mode-keys vi\nset -as terminal-features ',x:RGB'\nX=1\nset-environment -g A b\nnew-session -d\n");
    assert!(got.import.skipped.is_empty(), "{:?}", got.import.skipped);
    assert!(got.import.bindings.is_empty());
    assert!(!got.import.changes_anything());
}

#[test]
fn every_line_of_a_percent_if_block_is_skipped() {
    let got = import("%if #{==:#{host},box}\nbind a kill-pane\n%else\nbind b kill-pane\n%endif\nbind c kill-pane\n");
    assert_eq!(
        got.bound("a"),
        Some((Action::Module(ModuleId::agent()), false)),
        "untouched default"
    );
    assert_eq!(got.bound("b"), None);
    assert_eq!(
        got.bound("c"),
        Some((Action::ModuleKill, false)),
        "after %endif reads again"
    );
    let lines: Vec<usize> = got.import.skipped.iter().map(|s| s.origin.line).collect();
    assert_eq!(lines, vec![1, 2, 4], "{:?}", got.reasons());
    assert!(got.reasons()[0].contains("%if"));
    assert!(got.reasons()[1].contains("inside a %if block"));
    // Nested.
    let got = import("%if 1\n%if 2\nbind a kill-pane\n%endif\nbind b kill-pane\n%endif\nbind c kill-pane\n");
    assert_eq!(got.bound("b"), None);
    assert_eq!(got.bound("c"), Some((Action::ModuleKill, false)));
}

// --- source-file. -----------------------------------------------------------------------------

#[test]
fn source_file_follows_home_globs_and_quiet_missing_files() {
    let home = ScratchDir::new("eitri-tmux", "source");
    write(
        &home.join(".tmux.conf"),
        "source-file ~/.config/tmux/base.conf\nsource -q ~/missing.conf\nsource-file -q \"~/conf.d/*.conf\"\n",
    );
    write(&home.join(".config/tmux/base.conf"), "bind A kill-pane\n");
    write(&home.join("conf.d/b.conf"), "bind B kill-pane\n");
    write(&home.join("conf.d/a.conf"), "bind B kill-window\nbind C kill-pane\n");
    write(&home.join("conf.d/.hidden.conf"), "bind D kill-pane\n");
    let (map, import) = Keymap::with_tmux(&read(&env_for(&home)), &LuaClaims::default());
    let bound = |key: &str| map.lookup(&k(key)).map(|b| b.action.clone());
    assert_eq!(bound("A"), Some(Action::ModuleKill));
    // Glob matches in name order: a.conf, then b.conf, which wins B.
    assert_eq!(bound("B"), Some(Action::ModuleKill));
    assert_eq!(bound("C"), Some(Action::ModuleKill));
    assert_eq!(bound("D"), None, "a glob's * does not match a leading dot");
    assert!(import.skipped.is_empty(), "{:?}", import.skipped);
    assert_eq!(import.files, vec![home.join(".tmux.conf")]);
}

#[test]
fn a_source_cycle_and_deep_nesting_are_skipped_with_a_reason() {
    let home = ScratchDir::new("eitri-tmux", "cycle");
    write(&home.join(".tmux.conf"), "source-file ~/a.conf\n");
    write(&home.join("a.conf"), "bind A kill-pane\nsource-file ~/.tmux.conf\n");
    let read_cycle = read(&env_for(&home));
    assert_eq!(read_cycle.skipped.len(), 1, "{:?}", read_cycle.skipped);
    assert!(read_cycle.skipped[0].reason.contains("a cycle"));
    assert_eq!(read_cycle.skipped[0].origin.file, home.join("a.conf"));

    let home = ScratchDir::new("eitri-tmux", "deep");
    write(&home.join(".tmux.conf"), "source-file ~/d1.conf\n");
    for n in 1..=12 {
        write(
            &home.join(format!("d{n}.conf")),
            &format!("bind F{n} kill-pane\nsource-file ~/d{}.conf\n", n + 1),
        );
    }
    write(&home.join("d13.conf"), "bind F13 kill-pane\n");
    let got = read(&env_for(&home));
    let (map, _) = Keymap::with_tmux(&got, &LuaClaims::default());
    assert!(map.lookup(&k("F10")).is_some());
    assert!(map.lookup(&k("F11")).is_none() || map.lookup(&k("F11")).unwrap().source == Source::Default);
    assert!(
        got.skipped
            .iter()
            .any(|s| s.reason.contains("nested more than 10 deep")),
        "{:?}",
        got.skipped
    );
}

#[test]
fn the_bounds_stop_reading_and_say_so() {
    let home = ScratchDir::new("eitri-tmux", "bounds");
    let big = "# padding\n".repeat(110_000);
    write(&home.join(".tmux.conf"), "source ~/big.conf\nsource ~/many.conf\n");
    write(&home.join("big.conf"), &big);
    write(&home.join("many.conf"), &"source -q ~/one.conf\n".repeat(70));
    write(&home.join("one.conf"), "bind A kill-pane\n");
    let got = read(&env_for(&home));
    assert!(
        got.skipped.iter().any(|s| s.reason.contains("bigger than 1 MiB")),
        "{:?}",
        got.skipped
    );
    assert!(
        got.skipped.iter().any(|s| s.reason.contains("more than 64 files")),
        "{:?}",
        got.skipped
    );

    let home = ScratchDir::new("eitri-tmux", "lines");
    write(&home.join(".tmux.conf"), &"set -g mouse on\n".repeat(20_001));
    let got = read(&env_for(&home));
    assert!(
        got.skipped.iter().any(|s| s.reason.contains("more than 20000 lines")),
        "{:?}",
        got.skipped
    );
}

// --- Which files, and in what order. ------------------------------------------------------------

#[test]
fn every_config_file_tmux_loads_in_its_order_each_path_once() {
    let home = ScratchDir::new("eitri-tmux", "order");
    let mut env = env_for(&home);
    env.system_file = home.join("etc/tmux.conf");
    write(&env.system_file, "bind A kill-pane\n");
    write(&home.join(".tmux.conf"), "bind A kill-window\nbind B kill-pane\n");
    write(
        &home.join("xdg/tmux/tmux.conf"),
        "bind B kill-window\nbind C kill-pane\n",
    );
    write(&home.join(".config/tmux/tmux.conf"), "bind C kill-window\n");
    env.env
        .insert("XDG_CONFIG_HOME".into(), home.join("xdg").display().to_string());
    assert_eq!(
        env.config_files(),
        vec![
            home.join("etc/tmux.conf"),
            home.join(".tmux.conf"),
            home.join("xdg/tmux/tmux.conf"),
            home.join(".config/tmux/tmux.conf"),
        ]
    );
    let got = read(&env);
    assert_eq!(got.files.len(), 4);
    let (map, _) = Keymap::with_tmux(&got, &LuaClaims::default());
    for key in ["A", "B", "C"] {
        assert_eq!(
            map.lookup(&k(key)).unwrap().action,
            Action::Tab(TabAction::Close),
            "{key}: a later file wins"
        );
    }
    // XDG_CONFIG_HOME spelled the same as ~/.config is one path, read once.
    env.env
        .insert("XDG_CONFIG_HOME".into(), home.join(".config").display().to_string());
    assert_eq!(env.config_files().len(), 3);
    env.env.remove("XDG_CONFIG_HOME");
    assert_eq!(env.config_files().len(), 3, "an unset $XDG_CONFIG_HOME names no file");
}

#[test]
fn no_config_at_all_imports_nothing_and_says_so() {
    let home = ScratchDir::new("eitri-tmux", "none");
    let got = read(&env_for(&home));
    assert!(got.files.is_empty() && got.steps.is_empty() && got.skipped.is_empty());
    let (map, import) = Keymap::with_tmux(&got, &LuaClaims::default());
    assert_eq!(map, Keymap::defaults());
    assert!(!import.changes_anything());
}

// --- The import never fails startup. ------------------------------------------------------------

#[test]
fn a_bad_prefix_keeps_the_last_good_one_and_the_bindings_still_come() {
    for (conf, why) in [
        ("set -g prefix a\nbind m kill-pane\n", "must be a chord"),
        ("set -g prefix C-h\nbind m kill-pane\n", "root key"),
        ("set -g prefix C-=\nbind m kill-pane\n", "root key"),
        ("set -g prefix None\nbind m kill-pane\n", "is not a tmux key name"),
    ] {
        let got = import(conf);
        assert_eq!(got.map.prefix(), &k("C-b"), "{conf}");
        assert_eq!(got.import.prefix, None, "{conf}");
        assert!(got.only_reason().contains(why), "{conf}: {:?}", got.reasons());
        assert_eq!(got.bound("m"), Some((Action::ModuleKill, false)), "{conf}");
    }
    let got = import("set -g prefix C-a\nset -g prefix C-j\n");
    assert_eq!(got.map.prefix(), &k("C-a"), "the earlier good prefix stays");
}

#[test]
fn the_canvas_key_is_never_imported() {
    let got = import("bind v copy-mode\n");
    assert!(got.only_reason().contains("reserved for the canvas"));
    assert_eq!(got.bound("v"), None);
}

#[test]
fn a_key_or_prefix_init_lua_claimed_yields_to_init_lua() {
    let claims = LuaClaims {
        panel_keys: vec![("notes".into(), "g".into())],
        command_keybindings: vec![("notes.open".into(), "<Control>a".into())],
    };
    let got = import_with("set -g prefix C-a\nbind g kill-pane\nbind m kill-pane\n", &claims);
    assert_eq!(got.map.prefix(), &k("C-b"));
    assert_eq!(got.bound("g"), None);
    assert_eq!(got.bound("m"), Some((Action::ModuleKill, false)));
    let reasons = got.reasons();
    assert!(
        reasons[0].contains("taken by init.lua") && reasons[0].contains("notes.open"),
        "{reasons:?}"
    );
    assert!(
        reasons[1].contains("taken by init.lua") && reasons[1].contains("notes"),
        "{reasons:?}"
    );
    // And so the panel and the command still start.
    let lua = [(ModuleId::lua("notes"), Some("g".to_string()))];
    crate::layout::ModuleKeys::build(&lua, &got.map).expect("the panel's key is free");
    crate::keymap::check_command_keybinding("notes.open", "<Control>a", &got.map).expect("the accelerator fires");
}

#[test]
fn a_file_that_cannot_be_read_is_skipped_with_the_reason() {
    use std::os::unix::fs::PermissionsExt;
    let home = ScratchDir::new("eitri-tmux", "unreadable");
    write(
        &home.join(".tmux.conf"),
        "source-file ~/locked.conf\nbind m kill-pane\n",
    );
    write(&home.join("locked.conf"), "bind A kill-pane\n");
    std::fs::set_permissions(home.join("locked.conf"), std::fs::Permissions::from_mode(0o000)).unwrap();
    // Root reads a 000 file anyway; the test means nothing there.
    if std::fs::read(home.join("locked.conf")).is_ok() {
        return;
    }
    let got = read(&env_for(&home));
    assert!(
        got.skipped.iter().any(|s| s.reason.contains("cannot be read")),
        "{:?}",
        got.skipped
    );
    let (map, _) = Keymap::with_tmux(&got, &LuaClaims::default());
    assert_eq!(map.lookup(&k("m")).unwrap().action, Action::ModuleKill);
    let utf = ScratchDir::new("eitri-tmux", "binary");
    std::fs::write(utf.join(".tmux.conf"), [0xff, 0xfe, b'\n']).unwrap();
    let got = read(&env_for(&utf));
    assert!(
        got.skipped.iter().any(|s| s.reason.contains("cannot be read")),
        "{:?}",
        got.skipped
    );
}

// --- Layering with init.lua. ----------------------------------------------------------------------

fn set(key: &str, action: &str) -> KeymapOp {
    KeymapOp::Set {
        table: "prefix".into(),
        key: key.into(),
        action: action.into(),
        opts: vec![],
    }
}

fn del(key: &str) -> KeymapOp {
    KeymapOp::Del {
        table: "prefix".into(),
        key: key.into(),
    }
}

#[test]
fn init_lua_overwrites_and_deletes_imported_keys_without_error() {
    let got = import("unbind C-b\nbind m kill-pane\n");
    let map = got
        .map
        .clone()
        .then_user(&[set("m", "zoom"), del("C-b"), del("m"), set("m", "zoom")], &[])
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(map.lookup(&k("m")).unwrap().action, Action::Zoom);
    assert_eq!(map.lookup(&k("m")).unwrap().source, Source::User);
    // A key nobody bound is still init.lua's mistake, and a default still needs its del.
    assert!(got.map.clone().then_user(&[del("q")], &[]).is_err());
    assert!(got
        .map
        .clone()
        .then_user(&[set("z", "hint")], &[])
        .unwrap_err()
        .to_string()
        .contains("(default)"));
    // init.lua's prefix wins over tmux's.
    let got = import("set -g prefix C-a\n");
    let map = got
        .map
        .then_user(&[KeymapOp::Prefix { key: "C-s".into() }], &[])
        .unwrap();
    assert_eq!(map.prefix(), &k("C-s"));
}

// --- What the overlay shows. ---------------------------------------------------------------------

#[test]
fn the_overlay_marks_imported_rows_and_lists_the_skipped_lines() {
    let got = import("bind m kill-pane\nbind P paste-buffer\n");
    let rows = got.map.help(&crate::layout::ModuleKeys::built_in());
    let row = rows.iter().find(|r| r.keys == "Ctrl+b m").unwrap();
    assert!(row.what.ends_with(" (tmux)"), "{row:?}");
    let x = rows.iter().find(|r| r.keys == "Ctrl+b x").unwrap();
    assert!(!x.what.contains("(tmux)"), "{x:?}");
    let skipped = notice::skipped_rows(&got.import, Some(&got.home));
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0].keys, "~/.tmux.conf:2");
    assert_eq!(
        skipped[0].what,
        "bind P paste-buffer \u{2014} no Eitri equivalent: paste-buffer"
    );
}

// --- The setting, and startup. ---------------------------------------------------------------------

#[test]
fn the_setting_is_on_by_default_off_imports_nothing_and_a_bad_value_names_the_key() {
    assert_eq!(enabled(None), Ok(true));
    assert_eq!(enabled(Some("on")), Ok(true));
    assert_eq!(enabled(Some("off")), Ok(false));
    let err = enabled(Some("yes")).unwrap_err();
    assert!(err.contains("keymap.from_tmux") && err.contains("\"yes\""), "{err}");

    let home = ScratchDir::new("eitri-tmux", "startup");
    write(&home.join(".tmux.conf"), "set -g prefix C-a\nbind m kill-pane\n");
    let env = env_for(&home);
    let (map, import) = Keymap::for_startup(
        &[],
        &[],
        Some("off"),
        || panic!("the environment is not read with the import off"),
        &LuaClaims::default(),
    )
    .unwrap();
    assert_eq!(map, Keymap::defaults());
    assert!(import.is_none());
    let (map, import) = Keymap::for_startup(&[], &[], None, || env.clone(), &LuaClaims::default()).unwrap();
    assert_eq!(map.prefix(), &k("C-a"));
    assert!(import.unwrap().changes_anything());
    assert!(Keymap::for_startup(&[], &[], Some("1"), || env.clone(), &LuaClaims::default()).is_err());
    // init.lua's own mistakes still fail startup, import or not.
    assert!(Keymap::for_startup(&[del("q")], &[], None, || env.clone(), &LuaClaims::default()).is_err());
}

// --- The fingerprint behind the one-time notice. ------------------------------------------------------

#[test]
fn the_fingerprint_is_stable_changes_with_the_result_and_ignores_comments() {
    let a = import("set -g prefix C-a\nbind m kill-pane\nunbind %\n");
    let again = import("set -g prefix C-a\nbind m kill-pane\nunbind %\n");
    let commented = import("# mine\nset -g prefix C-a   # comfy\n\nbind m kill-pane\nunbind %\n");
    let reordered = import("unbind %\nbind m kill-pane\nset -g prefix C-a\n");
    let other = import("set -g prefix C-a\nbind m kill-window\nunbind %\n");
    let fp = |i: &Imported| notice::fingerprint(&i.import);
    assert_eq!(fp(&a), fp(&again));
    assert_eq!(fp(&a), fp(&commented));
    assert_eq!(fp(&a), fp(&reordered), "the same result, however it was written");
    assert_ne!(fp(&a), fp(&other));
    assert_ne!(fp(&a), fp(&import("set -g prefix C-a\nbind m kill-pane\n")));
    assert_eq!(fp(&a).len(), 64);

    let state = ScratchDir::new("eitri-tmux", "state");
    let file = notice::state_file(Some(state.as_os_str()), None).unwrap();
    assert_eq!(file, state.join("eitri/keymap-import.json"));
    assert!(notice::needs_notice(&file, &fp(&a)));
    notice::remember(&file, &fp(&a)).unwrap();
    assert!(!notice::needs_notice(&file, &fp(&a)));
    assert!(notice::needs_notice(&file, &fp(&other)));
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
    let text = std::fs::read_to_string(&file).unwrap();
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(json["fingerprint"], fp(&a));
}

#[test]
fn the_notice_and_the_log_line_say_what_was_imported() {
    let got = import("set -g prefix C-a\nunbind C-b\nbind C-a send-prefix\nbind m kill-pane\nbind P paste-buffer\n");
    let text = notice::toast_text(&got.import, &got.map, Some(&got.home)).expect("it changed something");
    assert_eq!(
        text,
        "Using your tmux keys from ~/.tmux.conf: prefix Ctrl+a, 2 keys (1 skipped). Ctrl+a ? lists them; \
         keymap.from_tmux = \"off\" turns this off."
    );
    assert_eq!(
        notice::log_line(Some(&got.import), Some(&got.home)),
        "[keymap] tmux: ~/.tmux.conf: prefix C-a, 2 imported, 1 skipped"
    );
    assert_eq!(notice::log_line(None, None), "[keymap] tmux: off");
    let none = import("set -g mouse on\n");
    assert_eq!(notice::toast_text(&none.import, &none.map, Some(&none.home)), None);
    let empty = TmuxImport::default();
    assert_eq!(notice::log_line(Some(&empty), None), "[keymap] tmux: no config found");
}

// --- Whatever the files hold, the import neither hangs nor grows without bound. -------------------

#[test]
fn a_fifo_or_a_device_in_place_of_a_config_is_skipped_without_reading_it() {
    let home = ScratchDir::new("eitri-tmux", "fifo");
    let fifo = std::ffi::CString::new(home.join(".tmux.conf").as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: a valid NUL-terminated path; mkfifo touches nothing else.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    std::os::unix::fs::symlink("/dev/zero", home.join("zero.conf")).unwrap();
    write(
        &home.join(".config/tmux/tmux.conf"),
        "source-file ~/zero.conf\nbind m kill-pane\n",
    );
    let got = read(&env_for(&home));
    let reasons: Vec<&str> = got.skipped.iter().map(|s| s.reason.as_str()).collect();
    assert_eq!(
        reasons.iter().filter(|r| r.contains("not a regular file")).count(),
        2,
        "{reasons:?}"
    );
    let (map, _) = Keymap::with_tmux(&got, &LuaClaims::default());
    assert_eq!(map.lookup(&k("m")).unwrap().action, Action::ModuleKill);
}

#[test]
fn a_read_stops_one_byte_past_its_limit() {
    // Straight at /dev/zero, past the regular-file check: the read itself is bounded.
    let err = read_limited(Path::new("/dev/zero"), 4096).unwrap_err();
    assert!(err.contains("bigger than"), "{err}");
}

#[test]
fn the_number_of_steps_and_skips_is_bounded() {
    let home = ScratchDir::new("eitri-tmux", "entries");
    write(&home.join(".tmux.conf"), &"unbind q\n".repeat(MAX_ENTRIES + 10));
    let got = read(&env_for(&home));
    assert!(got.steps.len() + got.skipped.len() <= MAX_ENTRIES + 1);
    let limit = format!("more than {MAX_ENTRIES}");
    assert!(
        got.skipped.iter().any(|s| s.reason.contains(&limit)),
        "{:?}",
        got.skipped.last()
    );
}

#[test]
fn a_binding_that_reads_a_variable_a_percent_if_may_have_set_is_skipped() {
    let got =
        import("ACTION=new-window\n%if #{==:#{host},x}\nACTION=kill-pane\n%endif\nbind m $ACTION\nbind n kill-pane\n");
    assert_eq!(got.bound("m"), None, "neither guessed as new-window nor as kill-pane");
    let reason = got
        .import
        .skipped
        .iter()
        .find(|s| s.origin.line == 5)
        .unwrap()
        .reason
        .clone();
    assert!(reason.contains("$ACTION") && reason.contains("%if"), "{reason}");
    assert_eq!(got.bound("n"), Some((Action::ModuleKill, false)));
}

#[test]
fn a_glob_stops_at_its_entry_cap_however_much_matching_budget_is_left() {
    let home = ScratchDir::new("eitri-tmux", "globbudget");
    for n in 0..200 {
        write(&home.join(format!("d/f{n:03}.conf")), "");
    }
    let pattern = format!("{}/d/*.conf", home.display());
    let mut budget = GlobBudget {
        entries: 50,
        work: usize::MAX,
    };
    let (found, exhausted) = glob(&pattern, &mut budget);
    assert!(exhausted);
    assert!(found.is_empty());
    assert_eq!(budget.entries, 0);
    let mut budget = GlobBudget {
        entries: MAX_GLOB_ENTRIES,
        work: MAX_GLOB_WORK,
    };
    let (found, exhausted) = glob(&pattern, &mut budget);
    assert!(!exhausted);
    assert_eq!(found.len(), 200);
    let literal = pattern.split('/').filter(|p| !p.is_empty()).count() - 1;
    assert_eq!(
        budget.entries,
        MAX_GLOB_ENTRIES - 200 - literal,
        "200 entries and each literal component"
    );
    assert_eq!(MAX_GLOB_ENTRIES, 10_000);
}

/// The matcher's own steps come out of their own budget: a long bracket class that a `*` makes it
/// rescan for every position cannot run past it, however many entries are left.
#[test]
fn a_glob_whose_matching_is_costly_stops_at_its_work_budget() {
    let home = ScratchDir::new("eitri-tmux", "globwork");
    for n in 0..2 {
        write(&home.join(format!("d/{n}{}", "b".repeat(100))), "");
    }
    let pattern = format!("{}/d/*[{}]", home.display(), "a".repeat(65_000));
    let started = std::time::Instant::now();
    let mut budget = GlobBudget {
        entries: MAX_GLOB_ENTRIES,
        work: 1_000_000,
    };
    let (found, exhausted) = glob(&pattern, &mut budget);
    assert!(exhausted, "{found:?}");
    assert_eq!(budget.work, 0);
    assert!(budget.entries > 0, "the entries were not what ran out");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
}

/// What each glob bound costs at its worst on this machine, for the record: 10,000 entries of a
/// warm directory, and 20,000,000 matcher steps. `cargo test -p eitri-core --lib measure_glob_bounds
/// -- --ignored --nocapture`.
#[test]
#[ignore]
fn measure_glob_bounds() {
    let home = ScratchDir::new("eitri-tmux", "measure");
    for n in 0..MAX_GLOB_ENTRIES + 100 {
        write(&home.join(format!("d/f{n:05}")), "");
    }
    let pattern = format!("{}/d/*.conf", home.display());
    let _warm = glob(
        &pattern,
        &mut GlobBudget {
            entries: usize::MAX,
            work: usize::MAX,
        },
    );
    let started = std::time::Instant::now();
    let mut budget = GlobBudget {
        entries: MAX_GLOB_ENTRIES,
        work: usize::MAX,
    };
    let (_, exhausted) = glob(&pattern, &mut budget);
    let entries = started.elapsed();
    assert!(exhausted);
    let class = format!("*[{}]", "a".repeat(65_000));
    let name = "b".repeat(255);
    let started = std::time::Instant::now();
    let mut work = MAX_GLOB_WORK;
    while fnmatch_within(&class, &name, &mut work).is_some() {}
    let steps = started.elapsed();
    println!("MEASURE: {MAX_GLOB_ENTRIES} warm entries: {entries:?}; {MAX_GLOB_WORK} matcher steps: {steps:?}");
}

#[test]
fn the_notice_state_file_is_read_bounded_and_only_when_regular() {
    let state = ScratchDir::new("eitri-tmux", "noticefifo");
    let file = notice::state_file(Some(state.as_os_str()), None).unwrap();
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    let fifo = std::ffi::CString::new(file.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: a valid NUL-terminated path; mkfifo touches nothing else.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    assert!(
        notice::needs_notice(&file, "x"),
        "a FIFO is no record, and reading it does not block"
    );
    std::fs::remove_file(&file).unwrap();
    std::os::unix::fs::symlink("/dev/zero", &file).unwrap();
    assert!(
        notice::needs_notice(&file, "x"),
        "a device is no record, and is not read to the end"
    );
    // Writing over a symlink replaces the link; what it pointed at is left alone.
    let elsewhere = state.join("elsewhere.json");
    std::fs::write(&elsewhere, "keep").unwrap();
    std::fs::remove_file(&file).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &file).unwrap();
    notice::remember(&file, "fp").unwrap();
    assert_eq!(std::fs::read_to_string(&elsewhere).unwrap(), "keep");
    assert!(!std::fs::symlink_metadata(&file).unwrap().file_type().is_symlink());
    assert!(!notice::needs_notice(&file, "fp"));
}

#[test]
fn a_config_tilde_follows_home_and_an_uncertain_home_skips_the_line() {
    let got = import("%if #{==:#{host},x}\nHOME=/elsewhere\n%endif\nsource-file ~/keys.conf\nbind n kill-pane\n");
    let reason = got
        .import
        .skipped
        .iter()
        .find(|s| s.origin.line == 4)
        .unwrap()
        .reason
        .clone();
    assert!(reason.contains("$HOME"), "{reason}");
    assert_eq!(got.bound("n"), Some((Action::ModuleKill, false)));
}

#[test]
fn uncertainty_passes_through_an_assignment() {
    let got = import(
        "ACTION=kill-pane\n%if 1\nACTION=new-window\n%endif\nCOPY=$ACTION\nAGAIN=$COPY\nbind m $AGAIN\nbind n kill-pane\n",
    );
    assert_eq!(got.bound("m"), None);
    let reason = got
        .import
        .skipped
        .iter()
        .find(|s| s.origin.line == 7)
        .unwrap()
        .reason
        .clone();
    assert!(reason.contains("$AGAIN"), "{reason}");
    // In one statement too: the assignment's dependency is the command's.
    let got = import("%if 1\nA=x\n%endif\nB=$A bind m kill-pane\n");
    assert_eq!(got.bound("m"), None);
}

#[test]
fn a_block_refused_for_its_depth_is_skipped_whole_however_deep() {
    let mut conf = String::new();
    for _ in 0..crate::keymap::tmux::lex::MAX_NESTING + 1 {
        conf.push_str("if-shell false {\n");
    }
    // Inside the outermost block only: a recovery one brace short would surface it.
    for _ in 0..crate::keymap::tmux::lex::MAX_NESTING {
        conf.push_str("}\n");
    }
    conf.push_str("bind m kill-pane\n}\nbind n kill-pane\n");
    let got = import(&conf);
    assert_eq!(got.bound("m"), None, "{:?}", got.import.skipped);
    assert_eq!(got.bound("n"), Some((Action::ModuleKill, false)));
}

#[test]
fn the_environment_skips_what_is_not_utf8_rather_than_panicking() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    let env = TmuxEnv::from_vars(
        Some(OsString::from("/home/u")),
        vec![
            (OsString::from("XDG_CONFIG_HOME"), OsString::from("/x")),
            (OsString::from("BAD"), OsString::from_vec(vec![0xff, 0xfe])),
            (OsString::from_vec(vec![0xff]), OsString::from("v")),
        ],
    );
    assert_eq!(env.env.get("XDG_CONFIG_HOME").map(String::as_str), Some("/x"));
    assert!(!env.env.contains_key("BAD"));
    assert_eq!(env.home, Some(PathBuf::from("/home/u")));
    assert_eq!(
        TmuxEnv::from_vars(Some(OsString::from("relative")), Vec::new()).home,
        None
    );
}

#[test]
fn the_strip_and_the_overlay_follow_a_tmux_binding_over_a_module_key() {
    use crate::layout::{strip_direct, Layout};
    let got = import("bind a send-prefix\n");
    let layout = Layout::initial(&[]).unwrap();
    let strip = strip_direct(&crate::layout::ModuleKeys::built_in(), &got.map, &layout);
    assert!(!strip.iter().any(|e| e.module == ModuleId::agent()), "{strip:?}");
    assert!(strip.iter().any(|e| e.module == ModuleId::editor() && e.key == "e"));
    let rows = got.map.help(&crate::layout::ModuleKeys::built_in());
    let a = rows.iter().find(|r| r.keys == "Ctrl+b a").unwrap();
    assert!(a.what.contains("(tmux)") && a.what.contains("Send"), "{a:?}");
}
