//! Existing default keys keep their meanings; new keys may be added. Checked-in fixtures pin
//! the two Rust default key tables, and changing or removing a pinned (context, key) mapping
//! fails. A new key absent from the fixture needs no fixture update.
//!
//! Two tables:
//! - The prefix (tmux-style) table: both `Keymap::defaults().prefix()` -- the trigger chord that
//!   *enters* prefix mode (default `Ctrl+b`) -- and `Keymap::defaults().bindings()`
//!   (the within-prefix-mode key table, looked up only once that chord has already fired). These are
//!   two different things that happen to share a spelling by design: `bindings()` also has its own
//!   row keyed `"C-b"` (`send-prefix`, stock tmux's `prefix C-b` -> send the literal prefix to the
//!   pane), but changing the *trigger* in `prefix()` would not touch that row's key at all, so pinning
//!   `bindings()` alone cannot catch a changed trigger. Rust/shell-only, fixture at
//!   `tests/fixtures/v1-frozen-prefix-keys.json`.
//! - The panel (leader/which-key) table, `eitri_core::keymap::default_bindings()` -- shared with
//!   `agent-ui/web`'s `keymap.v1Freeze.test.ts` via one fixture, `docs/keymap/v1-frozen-panel-keys.json`,
//!   because `PanelSeq::wire()` already produces the exact `keys: string[]` shape the frontend's
//!   `PanelBinding` uses on the wire (`serialize_keymap_for_js`), so one JSON file is the freeze for
//!   both languages instead of two fixtures that could quietly drift apart.
//!
//! Each action is pinned by its full shape (`Action::name()` plus every `Action::options()` value,
//! sorted), not just its name, so a binding that keeps its name but changes an option (`cells: 1` ->
//! `cells: 5`, `up: false` -> `up: true`) still counts as a changed meaning.
//!
//! Regenerate only for a deliberate addition, never to silence a regression:
//! `cargo test -p eitri-core --test keymap_v1_freeze -- --ignored regenerate_fixtures`.
//! Inspect the diff before accepting it: this test detects a change but cannot tell whether
//! it was intended.

use std::collections::BTreeMap;
use std::path::PathBuf;

use eitri_core::keymap::{self, Action, Keymap, OptValue};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
struct PrefixFixtureRow {
    key: String,
    repeatable: bool,
    action: String,
}

/// The frozen prefix shape includes both its trigger chord and every within-prefix binding.
/// An object wrapper, rather than a bare array, lets the fixture pin the trigger independently.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
struct PrefixFixture {
    prefix: String,
    bindings: Vec<PrefixFixtureRow>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
struct PanelFixtureRow {
    keys: Vec<String>,
    action: String,
}

fn opt_value_repr(v: &OptValue) -> String {
    match v {
        OptValue::Int(i) => i.to_string(),
        OptValue::Bool(b) => b.to_string(),
        OptValue::Str(s) => s.clone(),
        OptValue::Other(name) => format!("<{name}>"),
    }
}

/// The action's full shape: its name plus every option `Action::options()` reports, sorted by option
/// name so the signature is stable regardless of `options()`'s own emission order.
fn action_signature(action: &Action) -> String {
    let mut opts = action.options();
    opts.sort_by(|a, b| a.0.cmp(&b.0));
    let opts = opts
        .iter()
        .map(|(k, v)| format!("{k}={}", opt_value_repr(v)))
        .collect::<Vec<_>>()
        .join(",");
    format!("{}({opts})", action.name())
}

fn prefix_fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v1-frozen-prefix-keys.json")
}

fn panel_fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("core/ has a parent directory (the workspace root)")
        .join("docs/keymap/v1-frozen-panel-keys.json")
}

fn current_prefix_fixture() -> PrefixFixture {
    let defaults = Keymap::defaults();
    PrefixFixture {
        prefix: defaults.prefix().to_string(),
        bindings: defaults
            .bindings()
            .iter()
            .map(|b| PrefixFixtureRow {
                key: b.key.to_string(),
                repeatable: b.repeatable,
                action: action_signature(&b.action),
            })
            .collect(),
    }
}

fn current_panel_rows() -> Vec<PanelFixtureRow> {
    keymap::default_bindings()
        .iter()
        .map(|b| PanelFixtureRow {
            keys: b.seq.wire(),
            action: b.action.name().to_string(),
        })
        .collect()
}

fn read_fixture<T: serde::de::DeserializeOwned>(path: &PathBuf) -> Vec<T> {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| {
        panic!(
            "{}: {e} (generate it first: cargo test -p eitri-core --test keymap_v1_freeze -- \
             --ignored regenerate_fixtures)",
            path.display()
        )
    });
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

#[test]
fn prefix_table_keeps_every_pinned_binding() {
    let fixture: PrefixFixture = {
        let text = std::fs::read_to_string(prefix_fixture_path()).unwrap_or_else(|e| {
            panic!(
                "{}: {e} (generate it first: cargo test -p eitri-core --test keymap_v1_freeze -- \
                 --ignored regenerate_fixtures)",
                prefix_fixture_path().display()
            )
        });
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", prefix_fixture_path().display()))
    };
    let current = current_prefix_fixture();
    assert_eq!(
        fixture.prefix,
        current.prefix,
        "the v1 prefix TRIGGER chord (default prefix Ctrl+b) was changed: was {:?}, now {:?}. This is \
         `Keymap::defaults().prefix()` -- the key that *enters* prefix mode -- and is distinct from any \
         `bindings()` row keyed \"C-b\" (see this file's header). After v1, keys are only ever added, \
         never changed in meaning: if this change is deliberate, regenerate {} (see this file's own \
         header) and record why.",
        fixture.prefix,
        current.prefix,
        prefix_fixture_path().display(),
    );
    let current_by_key: BTreeMap<String, PrefixFixtureRow> =
        current.bindings.into_iter().map(|r| (r.key.clone(), r)).collect();
    let mut removed = Vec::new();
    let mut changed = Vec::new();
    for row in &fixture.bindings {
        match current_by_key.get(&row.key) {
            None => removed.push(row.key.clone()),
            Some(now) => {
                if now.repeatable != row.repeatable || now.action != row.action {
                    changed.push(format!(
                        "{}: was {} (repeatable={}), now {} (repeatable={})",
                        row.key, row.action, row.repeatable, now.action, now.repeatable
                    ));
                }
            }
        }
    }
    assert!(
        removed.is_empty() && changed.is_empty(),
        "the v1 prefix-keymap freeze (after v1, keys are only ever added, never changed in meaning) was \
         broken.\nremoved: {removed:?}\nchanged: {changed:?}\nIf this is a deliberate change, regenerate {} \
         (see this file's own header) and record why.",
        prefix_fixture_path().display(),
    );
}

#[test]
fn panel_table_keeps_every_pinned_binding() {
    let fixture: Vec<PanelFixtureRow> = read_fixture(&panel_fixture_path());
    let current: BTreeMap<String, PanelFixtureRow> = current_panel_rows()
        .into_iter()
        .map(|r| (r.keys.join(" "), r))
        .collect();
    let mut removed = Vec::new();
    let mut changed = Vec::new();
    for row in &fixture {
        let human = row.keys.join(" ");
        match current.get(&human) {
            None => removed.push(human),
            Some(now) => {
                if now.action != row.action {
                    changed.push(format!("{human}: was {}, now {}", row.action, now.action));
                }
            }
        }
    }
    assert!(
        removed.is_empty() && changed.is_empty(),
        "the v1 panel-keymap freeze (after v1, keys are only ever added, never changed in meaning) was \
         broken.\nremoved: {removed:?}\nchanged: {changed:?}\nIf this is a deliberate change, regenerate {} \
         (see this file's own header) and record why -- and note that agent-ui/web's \
         keymap.v1Freeze.test.ts reads the very same file.",
        panel_fixture_path().display(),
    );
}

/// The default panel rows added since the v1 fixture was taken, each with why it is there. A new default key is a
/// decision: it is named here when it is made, so a row in neither the fixture nor this list fails.
const PANEL_ROWS_ADDED_AFTER_V1: &[(&str, &str, &str)] = &[
    (
        "<leader> M",
        "model.pick",
        "eitri-only: capital M, since lowercase m stays the mode key",
    ),
    (
        "<leader> E",
        "effort.pick",
        "eitri-only: capital E, since lowercase e stays the following key",
    ),
    (
        "<leader> k",
        "tab.stop-background",
        "eitri-only: k as in tmux's kill-* verbs; it asks y/n first",
    ),
    (
        "<leader> r d",
        "remote.devices",
        "eitri-only: the remote-control device list, r as in remote and d as in devices; LazyVim binds no <leader>r \
         group, and the page offers it only while remote control is on",
    ),
];

#[test]
fn panel_rows_added_after_v1_are_named_with_their_reason() {
    let fixture: Vec<PanelFixtureRow> = read_fixture(&panel_fixture_path());
    let pinned: Vec<String> = fixture.iter().map(|r| r.keys.join(" ")).collect();
    let unnamed: Vec<String> = current_panel_rows()
        .into_iter()
        .map(|r| (r.keys.join(" "), r.action))
        .filter(|(keys, action)| {
            !pinned.contains(keys)
                && !PANEL_ROWS_ADDED_AFTER_V1
                    .iter()
                    .any(|(k, a, why)| k == keys && a == action && !why.is_empty())
        })
        .map(|(keys, action)| format!("{keys} -> {action}"))
        .collect();
    assert!(
        unnamed.is_empty(),
        "default panel rows that are neither pinned by {} nor named in PANEL_ROWS_ADDED_AFTER_V1 with a reason: \
         {unnamed:?}",
        panel_fixture_path().display(),
    );
}

/// `--ignored`, by design: this writes today's tables over the checked-in fixtures. Run it only after
/// a deliberate, approved change to the default bindings, then review the diff by hand before committing --
/// this test has no way to tell an intended change from a regression it should have caught.
#[test]
#[ignore]
fn regenerate_fixtures() {
    std::fs::write(
        prefix_fixture_path(),
        serde_json::to_string_pretty(&current_prefix_fixture()).expect("serializes") + "\n",
    )
    .expect("writes the prefix fixture");
    std::fs::write(
        panel_fixture_path(),
        serde_json::to_string_pretty(&current_panel_rows()).expect("serializes") + "\n",
    )
    .expect("writes the panel fixture");
}
