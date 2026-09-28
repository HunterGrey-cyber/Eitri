//! Fitness function F10 (2026-09-27 Codex audit P6, `the private review notes`
//! appendix, verdict SUPPORTED in `the private review notes`):
//! "After v1, keys are only ever added, never changed in meaning"
//! (`docs/superpowers/specs/2026-09-27-v1-decisions.md`, top paragraph). This pins the two Rust-side
//! default key tables as checked-in fixtures and fails, naming that spec, if a pinned
//! (context, key) -> action mapping is changed or removed. A brand-new key not in the fixture is
//! fine and needs no fixture update.
//!
//! Two tables:
//! - The prefix (tmux-style) table: both `Keymap::defaults().prefix()` -- the trigger chord that
//!   *enters* prefix mode (R36: "default prefix `Ctrl+b`") -- and `Keymap::defaults().bindings()`
//!   (the within-prefix-mode key table, looked up only once that chord has already fired). These are
//!   two different things that happen to share a spelling by design: `bindings()` also has its own
//!   row keyed `"C-b"` (`send-prefix`, stock tmux's `prefix C-b` -> send the literal prefix to the
//!   pane), but changing the *trigger* in `prefix()` would not touch that row's key at all, so pinning
//!   `bindings()` alone cannot catch a changed trigger. Rust/shell-only, fixture at
//!   `tests/fixtures/v1-frozen-prefix-keys.json`.
//! - The panel (leader/which-key) table, `neovibe_core::keymap::default_bindings()` -- shared with
//!   `agent-ui/web`'s `keymap.v1Freeze.test.ts` via one fixture, `docs/keymap/v1-frozen-panel-keys.json`,
//!   because `PanelSeq::wire()` already produces the exact `keys: string[]` shape the frontend's
//!   `PanelBinding` uses on the wire (`serialize_keymap_for_js`), so one JSON file is the freeze for
//!   both languages instead of two fixtures that could quietly drift apart.
//!
//! Each action is pinned by its full shape (`Action::name()` plus every `Action::options()` value,
//! sorted), not just its name, so a binding that keeps its name but changes an option (`cells: 1` ->
//! `cells: 5`, `up: false` -> `up: true`) still counts as a changed meaning.
//!
//! Regenerate (only for a deliberate, owner-approved addition -- never to silence a real
//! regression): `cargo test -p neovibe-core --test keymap_v1_freeze -- --ignored regenerate_fixtures`,
//! then read the diff by hand against `docs/superpowers/specs/2026-09-27-v1-decisions.md` before
//! committing -- this test cannot tell an intended change from a bug, only that one happened.

use std::collections::BTreeMap;
use std::path::PathBuf;

use neovibe_core::keymap::{self, Action, Keymap, OptValue};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
struct PrefixFixtureRow {
    key: String,
    repeatable: bool,
    action: String,
}

/// The whole prefix table's frozen shape: the trigger chord that enters prefix mode (R36) plus every
/// within-prefix-mode binding. Wrapping the array in an object (rather than a bare `Vec` as before)
/// is what makes it possible to pin `prefix` at all -- see this file's header.
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
            "{}: {e} (generate it first: cargo test -p neovibe-core --test keymap_v1_freeze -- \
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
                "{}: {e} (generate it first: cargo test -p neovibe-core --test keymap_v1_freeze -- \
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
        "the v1 prefix TRIGGER chord (docs/superpowers/specs/2026-09-27-v1-decisions.md, R36: \"default \
         prefix Ctrl+b\") was changed: was {:?}, now {:?}. This is `Keymap::defaults().prefix()` -- the \
         key that *enters* prefix mode -- and is distinct from any `bindings()` row keyed \"C-b\" (see \
         this file's header). If this is a deliberate, owner-approved change, regenerate {} (see this \
         file's own header) and say so in the dated record.",
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
        "the v1 prefix-keymap freeze (docs/superpowers/specs/2026-09-27-v1-decisions.md: \"After v1, \
         keys are only ever added, never changed in meaning\") was broken.\nremoved: {removed:?}\n\
         changed: {changed:?}\nIf this is a deliberate, owner-approved change, regenerate {} (see this \
         file's own header) and say so in the dated record.",
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
        "the v1 panel-keymap freeze (docs/superpowers/specs/2026-09-27-v1-decisions.md: \"After v1, \
         keys are only ever added, never changed in meaning\") was broken.\nremoved: {removed:?}\n\
         changed: {changed:?}\nIf this is a deliberate, owner-approved change, regenerate {} (see this \
         file's own header) and say so in the dated record -- and note that agent-ui/web's \
         keymap.v1Freeze.test.ts reads the very same file.",
        panel_fixture_path().display(),
    );
}

/// `--ignored`, by design: this writes today's tables over the checked-in fixtures. Run it only after
/// a deliberate, owner-approved binding change, then review the diff by hand before committing --
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
