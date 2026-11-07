//! Named sets of `eitri.keymap` calls that `eitri.keymap.use(name)` records in place, so a generated
//! `init.lua` can say "the i3-alt rows" in one line and a user can read what it means in the sample file
//! the bundle is built from.

use super::{KeymapOp, OptValue};

/// The names `eitri.keymap.use` accepts.
pub const BUNDLES: [&str; 1] = ["i3-alt"];

/// The ops of the bundle called `name`, in the order `eitri.keymap.use` records them; `None` for a name
/// that is not in [`BUNDLES`].
pub fn bundle(name: &str) -> Option<Vec<KeymapOp>> {
    match name {
        "i3-alt" => Some(i3_alt()),
        _ => None,
    }
}

fn del(key: &str) -> KeymapOp {
    KeymapOp::Del {
        table: "prefix".into(),
        key: key.into(),
    }
}

fn set(key: &str, action: &str, opts: Vec<(&str, OptValue)>) -> KeymapOp {
    KeymapOp::Set {
        table: "prefix".into(),
        key: key.into(),
        action: action.into(),
        opts: opts
            .into_iter()
            .map(|(name, value)| (name.to_string(), value))
            .collect(),
    }
}

/// A moved row: the Alt chord the stock i3 and sway configs grab goes away, and its action takes the key
/// that follows. The `del` comes first, as `Keymap::apply_user` requires of a key that is already bound.
fn moved(ops: &mut Vec<KeymapOp>, from: &str, to: &str, action: &str, opts: Vec<(&str, OptValue)>) {
    ops.push(del(from));
    ops.push(set(to, action, opts));
}

/// For an i3 or sway whose `$mod` is Alt: every Alt row of the stock prefix table that those configs grab
/// moves to the same key with Ctrl added, but for what Eitri cannot read as a Ctrl chord (Ctrl+Shift with
/// a character): the four window moves become Ctrl+Alt+h/j/k/l. The scratchpad move already has the plain
/// default `_`, so its Alt chord is only deleted.
fn i3_alt() -> Vec<KeymapOp> {
    let repeat = || vec![("repeatable", OptValue::Bool(true))];
    let mut ops = Vec::new();
    for (dir, key) in [("left", 'h'), ("down", 'j'), ("up", 'k'), ("right", 'l')] {
        moved(
            &mut ops,
            &format!("M-{}", key.to_ascii_uppercase()),
            &format!("C-M-{key}"),
            &format!("move.{dir}"),
            repeat(),
        );
    }
    moved(&mut ops, "M-w", "C-M-w", "container.tabbed", vec![]);
    moved(&mut ops, "M-s", "C-M-s", "container.stacked", vec![]);
    moved(&mut ops, "M-a", "C-M-a", "focus.parent", vec![]);
    moved(&mut ops, "M-d", "C-M-d", "focus.child", vec![]);
    moved(
        &mut ops,
        "M-Enter",
        "C-M-Enter",
        "instance.new",
        vec![("kind", OptValue::Str("terminal".into()))],
    );
    moved(&mut ops, "M-S-Space", "C-M-Space", "float.toggle", vec![]);
    moved(&mut ops, "M--", "C-M--", "scratchpad.show", vec![]);
    ops.push(del("M-_"));
    for (dir, key) in [("left", "Left"), ("down", "Down"), ("up", "Up"), ("right", "Right")] {
        moved(
            &mut ops,
            &format!("M-{key}"),
            &format!("C-M-{key}"),
            &format!("resize.{dir}"),
            vec![("cells", OptValue::Int(5)), ("repeatable", OptValue::Bool(true))],
        );
    }
    for (n, layout) in [
        (1, "even-horizontal"),
        (2, "even-vertical"),
        (3, "main-horizontal"),
        (4, "main-vertical"),
        (5, "tiled"),
    ] {
        moved(
            &mut ops,
            &format!("M-{n}"),
            &format!("C-M-{n}"),
            &format!("layout.{layout}"),
            vec![],
        );
    }
    moved(&mut ops, "M-e", "C-M-e", "container.toggle-split", vec![]);
    ops
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lua::kernel::{refuse_panels, Kernel};
    use std::path::Path;

    fn normalized(mut ops: Vec<KeymapOp>) -> Vec<KeymapOp> {
        for op in &mut ops {
            if let KeymapOp::Set { opts, .. } = op {
                opts.sort_by(|a, b| a.0.cmp(&b.0));
            }
        }
        ops.sort_by_key(|op| format!("{op:?}"));
        ops
    }

    fn key_of(op: &KeymapOp) -> &str {
        match op {
            KeymapOp::Set { key, .. } | KeymapOp::Del { key, .. } => key,
            other => panic!("{other:?}"),
        }
    }

    /// The sample file, run through real Lua, is what `use("i3-alt")` records. The sample's `pairs()` order
    /// differs from one Lua state to the next, so the two are compared as multisets; what must hold in any
    /// order is that a moved row's `del` comes before its `set`; `M-_` is deleted by itself because `_`
    /// already reaches the scratchpad by default.
    #[test]
    fn the_i3_alt_bundle_is_the_sample_file() {
        let sample = Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs/keymap/i3-alt-mod.lua");
        let kernel = Kernel::new(sample.parent().unwrap().to_path_buf(), refuse_panels).unwrap();
        kernel.run_and_check_init_file(&sample).unwrap();
        let from_sample = kernel.keymap.borrow().ops().to_vec();
        let moved_rows: Vec<_> = from_sample.iter().filter(|op| **op != del("M-_")).collect();
        assert_eq!(from_sample.iter().filter(|op| **op == del("M-_")).count(), 1);
        assert!(!from_sample
            .iter()
            .any(|op| matches!(op, KeymapOp::Set { key, .. } if key == "_")));
        for pair in moved_rows.chunks(2) {
            assert!(
                matches!(pair, [KeymapOp::Del { .. }, KeymapOp::Set { .. }]),
                "a row's del comes right before its set: {pair:?}"
            );
        }
        let ours = bundle("i3-alt").unwrap();
        assert_eq!(normalized(ours.clone()), normalized(from_sample));

        // Each moved row deletes before setting its replacement; the scratchpad chord needs only deletion.
        let positions = |op_is_del: bool| -> Vec<(usize, String)> {
            ours.iter()
                .enumerate()
                .filter(|(_, op)| matches!(op, KeymapOp::Del { .. }) == op_is_del)
                .map(|(i, op)| (i, key_of(op).to_string()))
                .collect()
        };
        let (dels, sets) = (positions(true), positions(false));
        assert_eq!(dels.len(), sets.len() + 1);
        for ((del_at, _), (set_at, _)) in dels.iter().filter(|(_, key)| key != "M-_").zip(&sets) {
            assert!(del_at < set_at);
        }
        for (_, key) in &dels {
            let stock = crate::keymap::Keymap::defaults();
            let spec = crate::keymap::KeySpec::parse(key).unwrap();
            assert!(stock.lookup(&spec).is_some(), "{key} is not a stock row");
        }
    }

    #[test]
    fn an_unknown_bundle_is_none_and_every_name_resolves() {
        assert!(bundle("nope").is_none());
        for name in BUNDLES {
            assert!(bundle(name).is_some(), "{name}");
        }
    }
}
