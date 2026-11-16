//! `eitri.keymap.prefix(key)`, `.set(table, key, action, opts)`, `.del(table, key)`, `.use(name)` (a named
//! set of those calls, recorded in place). The tables
//! are `"prefix"`, `"root"` (keys that work without the prefix), `"panel"` and `"plugin:<id>"` (a plugin
//! page's own keys, in vim's notation). Each call is **recorded, never applied or checked here**: `shell` hands the recording to
//! `eitri_core::keymap::Keymap::apply_user` once `init.lua` has run, so every mistake -- a bad
//! key, an unknown action, a collision -- is a startup failure naming the call. A Lua error raised
//! from here would only be logged (`Kernel::load_init_file`), and the window would open with a
//! keymap the user did not write. That includes an argument of the wrong type: it is recorded as
//! [`KeymapOp::Invalid`].

use std::cell::RefCell;
use std::rc::Rc;

use mlua::{FromLua, Lua, Table, Value};

use crate::keymap::{bundles, KeymapOp, OptValue};

/// The calls `init.lua` made, in order.
#[derive(Default)]
pub struct KeymapStore {
    ops: Vec<KeymapOp>,
    /// Where each call was made, `file:line`, one entry for each of `ops` (`None` when it was not made from a file).
    origins: Vec<Option<String>>,
}

impl KeymapStore {
    pub fn ops(&self) -> &[KeymapOp] {
        &self.ops
    }

    /// Where each of [`KeymapStore::ops`] was called from, as `file:line`.
    pub fn origins(&self) -> &[Option<String>] {
        &self.origins
    }

    fn record(&mut self, lua: &Lua, ops: impl IntoIterator<Item = KeymapOp>) {
        let origin = super::kernel::caller_origin(lua);
        for op in ops {
            self.ops.push(op);
            self.origins.push(origin.clone());
        }
    }
}

fn text(lua: &Lua, value: &Value) -> Option<String> {
    match value {
        Value::String(_) => String::from_lua(value.clone(), lua).ok(),
        _ => None,
    }
}

pub(crate) fn opts(lua: &Lua, value: &Value) -> Result<Vec<(String, OptValue)>, String> {
    let table = match value {
        Value::Nil => return Ok(Vec::new()),
        Value::Table(table) => table,
        other => return Err(format!("the options must be a table, not a {}", other.type_name())),
    };
    let mut out = Vec::new();
    for pair in table.clone().pairs::<Value, Value>() {
        let (name, value) = pair.map_err(|e| e.to_string())?;
        let name = text(lua, &name).ok_or_else(|| "every option has a name, as { cells = 5 }".to_string())?;
        let value = match value {
            Value::Integer(i) => OptValue::Int(i),
            Value::Number(n) if n.fract() == 0.0 && n.abs() < 1e15 => OptValue::Int(n as i64),
            Value::Boolean(b) => OptValue::Bool(b),
            Value::String(_) => OptValue::Str(text(lua, &value).unwrap_or_default()),
            other => OptValue::Other(other.type_name()),
        };
        out.push((name, value));
    }
    Ok(out)
}

/// `pub`: `Kernel::new` calls this.
pub fn install(lua: &Lua, eitri: &Table, store: Rc<RefCell<KeymapStore>>) -> mlua::Result<()> {
    let keymap = lua.create_table()?;

    let s = store.clone();
    keymap.set(
        "prefix",
        lua.create_function(move |lua, key: Value| {
            let op = match text(lua, &key) {
                Some(key) => KeymapOp::Prefix { key },
                None => KeymapOp::Invalid(format!(
                    "eitri.keymap.prefix(…): the key must be a string, not a {}",
                    key.type_name()
                )),
            };
            s.borrow_mut().record(lua, [op]);
            Ok(())
        })?,
    )?;

    let s = store.clone();
    keymap.set(
        "set",
        lua.create_function(
            move |lua, (table, key, action, options): (Value, Value, Value, Value)| {
                let op = match (text(lua, &table), text(lua, &key), text(lua, &action)) {
                    (Some(table), Some(key), Some(action)) => match opts(lua, &options) {
                        Ok(opts) => KeymapOp::Set {
                            table,
                            key,
                            action,
                            opts,
                        },
                        Err(why) => {
                            KeymapOp::Invalid(format!("eitri.keymap.set({table:?}, {key:?}, {action:?}, …): {why}"))
                        }
                    },
                    _ => KeymapOp::Invalid(
                        "eitri.keymap.set(table, key, action, opts): table, key and action must be strings".to_string(),
                    ),
                };
                s.borrow_mut().record(lua, [op]);
                Ok(())
            },
        )?,
    )?;

    let s = store.clone();
    keymap.set(
        "del",
        lua.create_function(move |lua, (table, key): (Value, Value)| {
            let op = match (text(lua, &table), text(lua, &key)) {
                (Some(table), Some(key)) => KeymapOp::Del { table, key },
                _ => KeymapOp::Invalid("eitri.keymap.del(table, key): table and key must be strings".to_string()),
            };
            s.borrow_mut().record(lua, [op]);
            Ok(())
        })?,
    )?;

    // A named set of calls (`crate::keymap::bundles`), recorded in place: a `del` after it sees its rows.
    let s = store;
    keymap.set(
        "use",
        lua.create_function(move |lua, name: Value| {
            let mut store = s.borrow_mut();
            match text(lua, &name) {
                Some(name) => match bundles::bundle(&name) {
                    Some(ops) => store.record(lua, ops),
                    None => store.record(
                        lua,
                        [KeymapOp::Invalid(format!(
                            "eitri.keymap.use({name:?}): there is no bundle of that name; the names are {}",
                            bundles::BUNDLES.join(", ")
                        ))],
                    ),
                },
                None => store.record(
                    lua,
                    [KeymapOp::Invalid(format!(
                        "eitri.keymap.use(…): the name must be a string, not a {}",
                        name.type_name()
                    ))],
                ),
            }
            Ok(())
        })?,
    )?;

    eitri.set("keymap", keymap)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::{Action, KeySpec, Keymap, KeymapError, SwapTarget, TabAction};
    use crate::layout::{Axis, Direction};

    fn run(source: &str) -> Vec<KeymapOp> {
        let lua = Lua::new();
        let eitri = lua.create_table().unwrap();
        let store = Rc::new(RefCell::new(KeymapStore::default()));
        install(&lua, &eitri, store.clone()).unwrap();
        lua.globals().set("eitri", eitri).unwrap();
        lua.load(source).exec().unwrap();
        let ops = store.borrow().ops().to_vec();
        ops
    }

    const SNIPPET: &str = include_str!("../../../docs/keymap/tmux-ctrl-a.lua");

    #[test]
    fn calls_are_recorded_in_order_with_their_options() {
        let ops = run(r#"
            eitri.keymap.prefix("C-a")
            eitri.keymap.del("prefix", "C-b")
            eitri.keymap.set("prefix", "h", "resize.left", { cells = 5, repeatable = true })
        "#);
        assert_eq!(ops[0], KeymapOp::Prefix { key: "C-a".into() });
        assert_eq!(
            ops[1],
            KeymapOp::Del {
                table: "prefix".into(),
                key: "C-b".into()
            }
        );
        let KeymapOp::Set {
            table,
            key,
            action,
            opts,
        } = &ops[2]
        else {
            panic!("{:?}", ops[2])
        };
        assert_eq!(
            (table.as_str(), key.as_str(), action.as_str()),
            ("prefix", "h", "resize.left")
        );
        let mut opts = opts.clone();
        opts.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            opts,
            vec![
                ("cells".into(), OptValue::Int(5)),
                ("repeatable".into(), OptValue::Bool(true))
            ]
        );
    }

    /// A wrong argument type is not a Lua error (which `shell` only logs): it is recorded, and
    /// becomes the startup failure.
    #[test]
    fn a_call_with_a_wrong_argument_is_recorded_as_invalid_naming_the_call() {
        for source in [
            "eitri.keymap.prefix(3)",
            "eitri.keymap.set('prefix', 'g')",
            "eitri.keymap.set('prefix', 'g', 'zoom', 'fast')",
            "eitri.keymap.set('prefix', 'g', 'zoom', { [1] = true })",
            "eitri.keymap.del('prefix')",
        ] {
            let ops = run(source);
            let [KeymapOp::Invalid(message)] = ops.as_slice() else {
                panic!("{source}: {ops:?}")
            };
            assert!(message.starts_with("eitri.keymap."), "{source}: {message}");
            assert!(Keymap::apply_user(&ops, &[]).is_err());
        }
        let ops = run("eitri.keymap.set('prefix', 'g', 'zoom', { cells = {} })");
        let KeymapOp::Set { opts, .. } = &ops[0] else { panic!() };
        assert_eq!(
            opts[0].1,
            OptValue::Other("table"),
            "a value of a type no option takes is kept for the error"
        );
    }

    /// `eitri.keymap.set("panel", ...)` is recorded like the prefix table and accepted by
    /// `Keymap::apply_user`.
    #[test]
    fn a_panel_table_set_is_recorded_and_accepted() {
        let ops = run(r#"eitri.keymap.set("panel", "<leader>tn", "tab.new")"#);
        let KeymapOp::Set {
            table,
            key,
            action,
            opts,
        } = &ops[0]
        else {
            panic!("{:?}", ops[0])
        };
        assert_eq!(
            (table.as_str(), key.as_str(), action.as_str(), opts.as_slice()),
            ("panel", "<leader>tn", "tab.new", [].as_slice())
        );
        let map = Keymap::apply_user(&ops, &[]).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(map.panel_user().sets.len(), 1);
    }

    /// A plugin page's table is named `plugin:<id>`; it is recorded like the others, and a key Eitri keeps
    /// fails startup rather than reaching the page.
    #[test]
    fn a_plugin_table_set_is_recorded_and_a_kept_key_fails_startup() {
        let ops = run(r#"
            eitri.keymap.set("plugin:snotra", "<C-n>", "item.next")
            eitri.keymap.del("plugin:snotra", "]q")
        "#);
        let map = Keymap::apply_user(&ops, &[]).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(map.plugin_user("snotra").len(), 2);

        let ops = run(r#"eitri.keymap.set("plugin:snotra", "<C-h>", "x")"#);
        let err = Keymap::apply_user(&ops, &[]).unwrap_err();
        assert!(matches!(err, KeymapError::PluginKey { .. }), "{err:?}");
        assert!(err.to_string().contains("\"<C-h>\""), "{err}");

        let ops = run(r#"eitri.keymap.set("plugin:Bad_Id", "j", "x")"#);
        let err = Keymap::apply_user(&ops, &[]).unwrap_err();
        assert!(matches!(err, KeymapError::UnknownTable { .. }), "{err:?}");
    }

    /// The shipped keymap snippet must load through real Lua and apply its advertised bindings.
    #[test]
    fn the_sample_snippet_applies_cleanly_and_reproduces_the_sample_tmux_config() {
        let map = Keymap::apply_user(&run(SNIPPET), &[]).unwrap_or_else(|e| panic!("{e}"));
        reproduces_the_sample_tmux(&map);
    }

    /// What the fixture user's `~/.config/tmux/base.conf` binds, as Eitri has it: the snippet's result,
    /// and since the import, also what reading that tmux config itself gives.
    fn reproduces_the_sample_tmux(map: &Keymap) {
        let k = |s: &str| KeySpec::parse(s).unwrap();
        let action = |s: &str| map.lookup(&k(s)).map(|b| (b.action.clone(), b.repeatable));
        assert_eq!(map.prefix(), &k("C-a"));
        assert_eq!(action("C-a"), Some((Action::SendPrefix, false)));
        assert_eq!(action("C-b"), None);
        assert_eq!(action("%"), None);
        assert_eq!(action("\""), None);
        assert_eq!(
            action("\\"),
            Some((
                Action::Split {
                    axis: Axis::Row,
                    cwd: crate::keymap::action::SplitCwd::Pane
                },
                false
            ))
        );
        assert_eq!(
            action("-"),
            Some((
                Action::Split {
                    axis: Axis::Column,
                    cwd: crate::keymap::action::SplitCwd::Pane
                },
                false
            ))
        );
        assert_eq!(action("|"), Some((Action::Even(Axis::Row), false)));
        assert_eq!(action("_"), Some((Action::Even(Axis::Column), false)));
        assert_eq!(
            action("h"),
            Some((
                Action::Resize {
                    dir: Direction::Left,
                    cells: 5
                },
                true
            ))
        );
        assert_eq!(
            action("l"),
            Some((
                Action::Resize {
                    dir: Direction::Right,
                    cells: 5
                },
                true
            ))
        );
        assert_eq!(action("m"), Some((Action::Zoom, true)));
        assert_eq!(
            action("L"),
            Some((Action::Swap(SwapTarget::Toward(Direction::Right)), false))
        );
        for (key, dir) in [("H", Direction::Left), ("J", Direction::Down), ("K", Direction::Up)] {
            assert_eq!(
                action(key),
                Some((Action::Swap(SwapTarget::Toward(dir)), false)),
                "{key}: the tmux swap, not the far-edge default"
            );
        }
        assert_eq!(action("q"), Some((Action::Tab(TabAction::Close), false)));
        assert_eq!(
            action("x"),
            Some((Action::ModuleKill, false)),
            "base.conf:69 (`bind x kill-pane`) is a default"
        );
        assert_eq!(
            action("C-l"),
            Some((Action::SendKeys(k("C-l")), false)),
            "base.conf:73 is a default"
        );
    }

    /// A scratch home holding copies of a real user's two tmux files where that user keeps them
    /// (`~/.config/tmux/tmux.conf`, which sources `~/.config/tmux/base.conf`). The copies in
    /// `core/tests/fixtures/tmux/` are those files with every comment emptied and one script name changed,
    /// line for line, so the snippet's `base.conf:NN` references still point at the right lines.
    fn sample_tmux_home() -> crate::test_scratch_dir::ScratchDir {
        let home = crate::test_scratch_dir::ScratchDir::new("eitri-tmux", "sample");
        let dir = home.join(".config/tmux");
        std::fs::create_dir_all(&dir).unwrap();
        let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tmux");
        for name in ["tmux.conf", "base.conf"] {
            std::fs::copy(fixtures.join(name), dir.join(name)).unwrap();
        }
        home
    }

    fn import_sample_tmux(home: &std::path::Path) -> (Keymap, crate::keymap::TmuxImport) {
        let env = crate::keymap::tmux::tests::env_for(home);
        let read = crate::keymap::tmux::read(&env);
        Keymap::with_tmux(&read, &crate::keymap::LuaClaims::default())
    }

    /// The bindings as `(key, action, repeatable)`, sorted: what two keymaps are compared by when
    /// their bindings' sources and order may differ.
    fn effect(map: &Keymap) -> Vec<(String, String, bool)> {
        let mut rows: Vec<_> = map
            .bindings()
            .iter()
            .map(|b| (b.key.to_string(), format!("{:?}", b.action), b.repeatable))
            .collect();
        rows.sort();
        rows
    }

    #[test]
    fn importing_the_sample_tmux_config_gives_what_the_snippet_gives() {
        let home = sample_tmux_home();
        let (map, import) = import_sample_tmux(&home);
        reproduces_the_sample_tmux(&map);
        let snippet = Keymap::apply_user(&run(SNIPPET), &[]).unwrap();
        assert_eq!(map.prefix(), snippet.prefix());
        assert_eq!(
            effect(&map),
            effect(&snippet),
            "every row, not only the ones asserted above"
        );
        assert_eq!(import.files, vec![home.join(".config/tmux/tmux.conf")]);
        // What did not come across, and why: its run/if-shell lines, its copy-mode and root keys,
        // the commands Eitri has no equivalent of.
        let at = |file: &str, line: usize| {
            import
                .skipped
                .iter()
                .find(|s| s.origin.file.ends_with(file) && s.origin.line == line)
                .unwrap_or_else(|| panic!("{file}:{line} not skipped: {:?}", import.skipped))
                .reason
                .clone()
        };
        assert!(at("tmux.conf", 7).contains("if-shell"));
        assert!(at("tmux.conf", 11).contains("run-shell"));
        assert!(at("tmux.conf", 16).contains("no Eitri equivalent: source-file"));
        assert!(at("base.conf", 82).contains("paste-buffer"));
        assert!(at("base.conf", 84).contains("copy-mode-vi"));
        assert!(at("base.conf", 104).contains("more than one command"));
        assert!(at("base.conf", 117).contains("root key"));
        assert!(at("base.conf", 143).contains("run-shell"));
        // The panel would drop it, so it is not taken from tmux; Eitri's own default binds the same.
        assert!(at("base.conf", 73).contains("cannot send C-l"));
        assert_eq!(import.skipped.len(), 27, "{:#?}", import.skipped);
        assert_eq!(import.bindings.len(), 16);
        assert_eq!(
            crate::keymap::tmux::notice::toast_text(&import, &map, Some(&home)).as_deref(),
            Some(
                "Using your tmux keys from ~/.config/tmux/tmux.conf: prefix Ctrl+a, 16 keys (27 skipped). Ctrl+a ? \
                 lists them; keymap.from_tmux = \"off\" turns this off."
            )
        );
    }

    /// A user's `init.lua` may already carry the snippet; with the import on, it applies on top of the
    /// import of the same file without a single error, and the keymap is the same.
    #[test]
    fn the_snippet_on_top_of_the_import_starts_and_gives_the_same_map() {
        let home = sample_tmux_home();
        let (imported, _) = import_sample_tmux(&home);
        let layered = imported.then_user(&run(SNIPPET), &[]).unwrap_or_else(|e| panic!("{e}"));
        reproduces_the_sample_tmux(&layered);
        let snippet = Keymap::apply_user(&run(SNIPPET), &[]).unwrap();
        assert_eq!(layered.prefix(), snippet.prefix());
        assert_eq!(effect(&layered), effect(&snippet));
    }

    #[test]
    fn use_i3_alt_keeps_the_default_scratchpad_key_without_an_override() {
        let map = Keymap::apply_user(&run(r#"eitri.keymap.use("i3-alt")"#), &[]).unwrap();
        let key = |text: &str| KeySpec::parse(text).unwrap();
        assert!(map.lookup(&key("M-_")).is_none());
        let binding = map.lookup(&key("_")).unwrap();
        assert_eq!(binding.action, Action::ScratchpadMove);
        assert!(!binding.repeatable);
        assert_eq!(binding.source, crate::keymap::Source::Default);
        assert!(map.user_overrides().is_empty(), "{:?}", map.user_overrides());
    }

    #[test]
    fn a_user_can_take_w_and_gets_the_default_override_notice() {
        let map = Keymap::apply_user(&run(r#"local k = eitri.keymap; k.set("prefix", "W", "zoom")"#), &[]).unwrap();
        let binding = map.lookup(&KeySpec::parse("W").unwrap()).unwrap();
        assert_eq!(binding.action, Action::Zoom);
        assert_eq!(binding.source, crate::keymap::Source::User);
        assert_eq!(map.user_overrides(), ["prefix W (was window.new)"]);
    }

    #[test]
    fn use_records_the_bundles_ops_in_place() {
        let ops = run(r#"
            eitri.keymap.use("i3-alt")
            eitri.keymap.del("prefix", "C-M-w")
            eitri.keymap.set("prefix", "C-M-w", "container.stacked")
        "#);
        let bundle = bundles::bundle("i3-alt").unwrap();
        assert_eq!(&ops[..bundle.len()], bundle.as_slice());
        assert_eq!(ops.len(), bundle.len() + 2);
        assert_eq!(
            ops[bundle.len()],
            KeymapOp::Del {
                table: "prefix".into(),
                key: "C-M-w".into()
            }
        );
        // The bundle's rows are bound when the later `del` runs: startup accepts the whole sequence.
        Keymap::apply_user(&ops, &[]).unwrap_or_else(|e| panic!("{e}"));
        // And it gives the same table as the sample file's calls.
        let by_use = Keymap::apply_user(&run(r#"eitri.keymap.use("i3-alt")"#), &[]).unwrap();
        let by_sample = Keymap::apply_user(&run(I3_ALT_MOD), &[]).unwrap();
        let sorted = |map: &Keymap| {
            let mut rows: Vec<String> = map.bindings().iter().map(|b| format!("{b:?}")).collect();
            rows.sort();
            rows
        };
        assert_eq!(sorted(&by_use), sorted(&by_sample));
    }

    #[test]
    fn use_of_an_unknown_name_is_a_startup_failure_naming_the_call() {
        for source in ["eitri.keymap.use('nope')", "eitri.keymap.use(3)", "eitri.keymap.use()"] {
            let ops = run(source);
            let [KeymapOp::Invalid(message)] = ops.as_slice() else {
                panic!("{source}: {ops:?}")
            };
            assert!(message.starts_with("eitri.keymap.use("), "{message}");
            assert!(Keymap::apply_user(&ops, &[]).is_err());
        }
        let ops = run("eitri.keymap.use('nope')");
        let KeymapOp::Invalid(message) = &ops[0] else { panic!() };
        assert!(message.contains("eitri.keymap.use(\"nope\")"), "{message}");
    }

    const I3_ALT_MOD: &str = include_str!("../../../docs/keymap/i3-alt-mod.lua");

    /// The Alt chords a stock i3 (`/etc/i3/config`) or sway (`/etc/sway/config`) grabs when `$mod` is Alt.
    const GRABBED_BY_ALT_MOD: &[&str] = &[
        "M-H",
        "M-J",
        "M-K",
        "M-L",
        "M-w",
        "M-s",
        "M-a",
        "M-d",
        "M-Enter",
        "M-S-Space",
        "M--",
        "M-_",
        "M-Up",
        "M-Down",
        "M-Left",
        "M-Right",
        "M-1",
        "M-2",
        "M-3",
        "M-4",
        "M-5",
        "M-e",
    ];

    #[test]
    fn the_i3_alt_mod_sample_applies_on_the_stock_keymap_without_a_collision() {
        let stock = Keymap::defaults();
        let map = Keymap::apply_user(&run(I3_ALT_MOD), &[]).unwrap_or_else(|e| panic!("{e}"));
        let key = |s: &str| KeySpec::parse(s).unwrap();
        let grabbed: Vec<KeySpec> = GRABBED_BY_ALT_MOD.iter().map(|s| key(s)).collect();
        let tmux: Vec<KeySpec> = crate::keymap::stock::STOCK_TMUX_PREFIX
            .iter()
            .map(|(k, _, _)| key(k))
            .collect();

        // Every grabbed row is gone; the scratchpad keeps its plain default and the others move to new chords.
        for old in &grabbed {
            let was = stock
                .lookup(old)
                .unwrap_or_else(|| panic!("{old:?} is not a stock row"));
            assert!(map.lookup(old).is_none(), "{old:?} is still bound");
            if *old == key("M-_") {
                assert_eq!(map.lookup(&key("_")), stock.lookup(&key("_")));
                continue;
            }
            let moved_to: Vec<&KeySpec> = map
                .bindings()
                .iter()
                .filter(|b| b.action == was.action && b.repeatable == was.repeatable && stock.lookup(&b.key).is_none())
                .map(|b| &b.key)
                .collect();
            assert_eq!(moved_to.len(), 1, "{old:?} ({:?}) moved to {moved_to:?}", was.action);
        }

        // Where the rows went: new keys only, none of them a chord the config grabs or a key stock tmux binds.
        let added: Vec<&KeySpec> = map
            .bindings()
            .iter()
            .map(|b| &b.key)
            .filter(|k| stock.lookup(k).is_none())
            .collect();
        assert_eq!(
            added.len(),
            grabbed.len() - 1,
            "the scratchpad keeps its default: {added:?}"
        );
        for new in &added {
            assert!(!grabbed.contains(new), "{new:?} is a chord the config grabs");
            assert!(!tmux.contains(new), "{new:?} is bound in stock tmux's prefix table");
        }

        // Nothing else changed: the prefix and every row that was not grabbed.
        assert_eq!(map.prefix(), stock.prefix());
        for b in stock.bindings().iter().filter(|b| !grabbed.contains(&b.key)) {
            let now = map.lookup(&b.key).unwrap_or_else(|| panic!("{:?} was unbound", b.key));
            assert_eq!((&now.action, now.repeatable), (&b.action, b.repeatable), "{:?}", b.key);
        }
    }

    /// The snippet's `pairs` loops run in an order Lua does not specify.
    #[test]
    fn the_snippet_applies_the_same_whatever_order_pairs_runs_in() {
        let ops = run(SNIPPET);
        let forwards = Keymap::apply_user(&ops, &[]).unwrap();
        // Reverse only the eight `set`s the two loops produce, which sit together in the recording.
        let mut shuffled = ops.clone();
        let loop_start = ops
            .iter()
            .position(|op| matches!(op, KeymapOp::Set { action, .. } if action.starts_with("resize.")))
            .unwrap();
        shuffled[loop_start..loop_start + 4].reverse();
        let swaps = ops
            .iter()
            .position(|op| matches!(op, KeymapOp::Set { action, .. } if action.starts_with("swap.")))
            .unwrap();
        shuffled[swaps..swaps + 4].reverse();
        let backwards = Keymap::apply_user(&shuffled, &[]).unwrap();
        for binding in forwards.bindings() {
            assert_eq!(
                backwards.lookup(&binding.key).map(|b| &b.action),
                Some(&binding.action),
                "{}",
                binding.key
            );
        }
        assert_eq!(forwards.bindings().len(), backwards.bindings().len());
    }
}
