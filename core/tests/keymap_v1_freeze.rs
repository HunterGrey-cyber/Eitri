//! Pins the two default key tables to checked-in fixtures. Existing (context, key)
//! mappings must retain their actions; new keys are allowed without fixture updates.
//!
//! The prefix table pins both `Keymap::defaults().prefix()`, the `Ctrl+b` trigger,
//! and `Keymap::defaults().bindings()`, the bindings used after the trigger fires.
//! The `C-b` binding sends the literal prefix to the pane. Changing the trigger
//! does not change that binding, so pinning bindings alone would miss the change.
//! The Rust prefix fixture is `tests/fixtures/v1-frozen-prefix-keys.json`.
//!
//! The panel table comes from `keymap::default_bindings()`. It shares
//! `docs/keymap/v1-frozen-panel-keys.json` with the frontend's `keymap.v1Freeze.test.ts`.
//! `PanelSeq::wire()` produces the `keys: string[]` shape used by `PanelBinding`
//! through `serialize_keymap_for_js`, so both languages can use the same fixture.
//!
//! Each action includes `Action::name()` and all sorted `Action::options()` values.
//! Changing an option changes the binding's meaning even if its action name stays the same.
//!
//! Run the ignored `regenerate_fixtures` test only for deliberate additions, then
//! inspect the diff before committing. Regeneration must not hide changed or removed
//! mappings; the test detects changes but cannot tell an intended change from a bug.

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

/// The prefix fixture includes the trigger chord and every binding within prefix mode. An
/// object holds both `prefix` and the bindings array so the trigger itself can be checked.
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
        "the v1 prefix TRIGGER chord (\"default \
         prefix Ctrl+b\") was changed: was {:?}, now {:?}. This is `Keymap::defaults().prefix()` -- the \
         key that *enters* prefix mode -- and is distinct from any `bindings()` row keyed \"C-b\" (see \
         this file's header). If this is a deliberate change, regenerate {} (see this \
         file's own header).",
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
        "the v1 prefix-keymap freeze (\"After v1, \
         keys are only ever added, never changed in meaning\") was broken.\nremoved: {removed:?}\n\
         changed: {changed:?}\nIf this is a deliberate change, regenerate {} (see this \
         file's own header).",
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
        "the v1 panel-keymap freeze (\"After v1, \
         keys are only ever added, never changed in meaning\") was broken.\nremoved: {removed:?}\n\
         changed: {changed:?}\nIf this is a deliberate change, regenerate {} (see this \
         file's own header) -- and note that agent-ui/web's \
         keymap.v1Freeze.test.ts reads the very same file.",
        panel_fixture_path().display(),
    );
}

/// Ignored because it overwrites the checked-in fixtures with the current binding tables. Run
/// after an intentional binding change and inspect the diff before committing: the test cannot
/// distinguish an intended change from a regression.
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
