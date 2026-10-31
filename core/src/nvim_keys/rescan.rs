//! The on-demand rescan: asks the embedded nvim to send its next report now, unchanged or not, and to
//! probe a few more chords in that one report.
//!
//! The feed only sends a report that differs from the last, and nvim has no "mappings changed" event, so a
//! mapping a plugin makes on `InsertEnter` or a buffer-local one can be missing from the last report. A
//! caller that is about to show keys asks again here; the answer arrives on the feed's socket like any other
//! report, and `nvim_keys.lua` keeps the extra chords for that report only.

use rmpv::Value;

use crate::editor_rpc::EditorRpc;
use crate::nvim_rpc::Pending;

/// The call: one table `{lhs = {<chord>, ...}}`. Answers `true` when the feed was running and took the
/// request, `false` when it was not (no feed, or already torn down).
pub const KEYS_RESCAN_LUA: &str = "local o = ... \
    local f = rawget(_G, '__eitri_keys_rescan') \
    if type(f) ~= 'function' then return false end \
    return f(o.lhs)";

/// Asks the editor for a report now, probing `extra` (chords in `<...>` notation, at most 16 bytes each;
/// the editor skips any other spelling) as well for that report only.
pub fn rescan(rpc: &dyn EditorRpc, extra: &[String]) -> Pending {
    rpc.exec_lua(KEYS_RESCAN_LUA, vec![rescan_opts(extra)])
}

/// The call's one argument.
pub fn rescan_opts(extra: &[String]) -> Value {
    Value::Map(vec![(
        Value::from("lhs"),
        Value::Array(extra.iter().map(|lhs| Value::from(lhs.as_str())).collect()),
    )])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_argument_carries_the_chords_in_order() {
        let opts = rescan_opts(&["<C-o>".to_string(), "<M-t>".to_string()]);
        let map = opts.as_map().unwrap();
        assert_eq!(map.len(), 1);
        assert_eq!(map[0].0.as_str(), Some("lhs"));
        let chords: Vec<_> = map[0]
            .1
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(chords, ["<C-o>", "<M-t>"]);
    }

    #[test]
    fn no_extra_chords_is_an_empty_list_not_a_missing_one() {
        let opts = rescan_opts(&[]);
        assert_eq!(opts.as_map().unwrap()[0].1.as_array().map(Vec::len), Some(0));
    }
}
