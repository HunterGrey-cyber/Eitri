//! The nvim report: `mapleader`, `timeoutlen`/
//! `timeout`, and the global Normal-mode mappings but `<Plug>` and `<SNR>` ones (at most 2,000),
//! read off an nvim->host push socket
//! (`feed` builds the socket itself; this module only shapes and classifies the JSON line it
//! carries). `v: 1` is the only wire version this reads; anything else -- a `v` this
//! crate does not know, or JSON it cannot parse at all -- is dropped whole, and the caller keeps
//! its last good report.

pub mod classify;
pub mod feed;
pub mod findings;
pub mod rescan;

pub use classify::{classify, translate_lhs};

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize};

/// The chords the probe reads in every probed mode: each is a candidate the key presets can pick for
/// a prefix. `nvim_keys.lua` carries the same list.
pub const PROBED_CHORDS: [&str; 7] = ["<C-b>", "<C-a>", "<C-Space>", "<C-h>", "<C-j>", "<C-k>", "<C-l>"];

/// The modes the probe reads each chord in: Normal, Visual (`x`), Insert and Terminal. `nvim_keys.lua`
/// carries the same list.
pub const PROBED_MODES: [&str; 4] = ["n", "x", "i", "t"];

/// The plugins and distributions the probe reports on, by the name their own project uses. The list is
/// closed: a name outside it in a report is dropped, so nothing a plugin says becomes a key here.
/// `nvim_keys.lua` carries the same list.
pub const KNOWN_PLUGINS: [&str; 11] = [
    "vim-tmux-navigator",
    "smart-splits.nvim",
    "Navigator.nvim",
    "which-key.nvim",
    "blink.cmp",
    "nvim-cmp",
    "lazy.nvim",
    "LazyVim",
    "NvChad",
    "AstroNvim",
    "LunarVim",
];

/// One nvim Normal-mode mapping (`vim.api.nvim_get_keymap("n")`): `lhs` already through
/// `vim.fn.keytrans`, `rhs` absent for a Lua callback, `desc` optional, and `callback` true exactly
/// when there is no `rhs` because a Lua function runs it instead.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NvimMap {
    pub lhs: String,
    pub rhs: Option<String>,
    pub desc: Option<String>,
    #[serde(default)]
    pub callback: bool,
}

/// One report from the `nvim_keys` feed. `mapleader` is already `keytrans`'d
/// (`"<Space>"`, never a raw `" "`) and absent when nvim's own `g:mapleader` is unset or empty.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NvimReport {
    pub v: u32,
    pub mapleader: Option<String>,
    pub timeoutlen: u32,
    pub timeout: bool,
    pub maps: Vec<NvimMap>,
    /// What the probe found, when the sender has one. A `probe` this reader cannot make sense of is
    /// `None`, and the rest of the report still applies.
    #[serde(default, deserialize_with = "lenient_probe")]
    pub probe: Option<NvimProbe>,
}

/// Where a probed mapping lives: `nvim_get_keymap` (global) or the current buffer's map.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Global,
    Buffer,
}

/// Whether a known plugin's own module is already in memory (`Loaded`) or only listed by lazy.nvim as
/// one it will load later (`Declared`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginState {
    Loaded,
    Declared,
}

/// One mapping of a probed chord. `mode` is the probed mode, `lhs` as nvim spells it (`keytrans`'d,
/// so `<C-B>`), and `rhs`, `desc` and `origin` are text from the user's plugins: at most 200
/// characters, control characters and all, so whoever shows them escapes them. `origin` is a plugin's
/// directory name or `config`, never a path.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProbeKey {
    pub mode: String,
    pub lhs: String,
    pub scope: Scope,
    pub rhs: Option<String>,
    pub desc: Option<String>,
    #[serde(default)]
    pub callback: bool,
    pub origin: Option<String>,
}

/// The probe: every mapping of a probed chord in a probed mode, and which known plugins are present.
/// `plugins` holds only [`KNOWN_PLUGINS`] names.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct NvimProbe {
    #[serde(default)]
    pub keys: Vec<ProbeKey>,
    #[serde(default, deserialize_with = "known_plugins_only")]
    pub plugins: BTreeMap<String, PluginState>,
}

/// `probe` as the report's reader takes it: anything that is not a probe (an unknown scope or plugin
/// state, a wrong type) is no probe, never a reason to drop the leader and maps beside it.
fn lenient_probe<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<NvimProbe>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value::<NvimProbe>(value).ok())
}

fn known_plugins_only<'de, D: Deserializer<'de>>(deserializer: D) -> Result<BTreeMap<String, PluginState>, D::Error> {
    let mut plugins = BTreeMap::<String, PluginState>::deserialize(deserializer)?;
    plugins.retain(|name, _| KNOWN_PLUGINS.contains(&name.as_str()));
    Ok(plugins)
}

/// The report's leader as the classifier reads it: `mapleader` when nvim set one, or `\` -- vim's
/// own default -- when it did not (`nvim: map.txt` `*mapleader*`).
pub fn nvim_leader_token(report: &NvimReport) -> &str {
    report.mapleader.as_deref().unwrap_or("\\")
}

/// `line` parses only at wire version 1. Unparsable JSON, or a `v` this crate does not
/// know, is `None` -- never a partially-applied report -- so the caller keeps its last good one.
pub fn parse_report(line: &[u8]) -> Option<NvimReport> {
    serde_json::from_slice::<NvimReport>(line)
        .ok()
        .filter(|report| report.v == 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The quoted strings of `local <name> = { ... }` in the Lua source.
    fn lua_list(name: &str) -> Vec<String> {
        let lua = feed::NVIM_KEYS_LUA;
        let start = lua
            .find(&format!("local {name} = {{"))
            .unwrap_or_else(|| panic!("the snippet has no list {name}"));
        let body = &lua[start..];
        let body = &body[..body.find('}').unwrap()];
        body.split('"').skip(1).step_by(2).map(str::to_string).collect()
    }

    fn report_json(extra: &str) -> String {
        format!(
            r#"{{"v":1,"mapleader":null,"timeoutlen":300,"timeout":true,"maps":[{{"lhs":"H","rhs":":bnext<CR>"}}]{extra}}}"#
        )
    }

    #[test]
    fn the_snippet_probes_the_chords_modes_and_plugins_the_reader_names() {
        assert_eq!(lua_list("PROBED_CHORDS"), PROBED_CHORDS);
        assert_eq!(lua_list("PROBED_MODES"), PROBED_MODES);
        assert_eq!(lua_list("KNOWN_PLUGINS"), KNOWN_PLUGINS);
    }

    #[test]
    fn a_report_parses_with_and_without_a_probe() {
        let without = parse_report(report_json("").as_bytes()).unwrap();
        assert!(without.probe.is_none());
        let with = parse_report(
            report_json(
                r#","probe":{"keys":[{"mode":"i","lhs":"<C-B>","scope":"buffer","rhs":null,"desc":"complete","callback":true,"origin":"blink.cmp"}],"plugins":{"blink.cmp":"loaded","nvim-cmp":"declared"}}"#,
            )
            .as_bytes(),
        )
        .unwrap();
        let probe = with.probe.unwrap();
        assert_eq!(probe.keys.len(), 1);
        let key = &probe.keys[0];
        assert_eq!(
            (key.mode.as_str(), key.lhs.as_str(), key.scope),
            ("i", "<C-B>", Scope::Buffer)
        );
        assert_eq!((key.rhs.as_deref(), key.desc.as_deref()), (None, Some("complete")));
        assert!(key.callback);
        assert_eq!(key.origin.as_deref(), Some("blink.cmp"));
        assert_eq!(probe.plugins["blink.cmp"], PluginState::Loaded);
        assert_eq!(probe.plugins["nvim-cmp"], PluginState::Declared);
    }

    #[test]
    fn an_unknown_extra_field_still_parses_and_a_newer_version_is_dropped() {
        let extra = r#","future":{"a":1},"probe":{"keys":[],"plugins":{},"more":true}"#;
        assert!(parse_report(report_json(extra).as_bytes()).unwrap().probe.is_some());
        let v2 = report_json("").replace(r#""v":1"#, r#""v":2"#);
        assert!(parse_report(v2.as_bytes()).is_none());
    }

    #[test]
    fn a_long_desc_with_control_characters_is_kept_verbatim() {
        let desc = format!(r"a\u001b[31m\u0007{}\u202e", "x".repeat(192));
        let json = report_json(&format!(
            r#","probe":{{"keys":[{{"mode":"n","lhs":"<C-A>","scope":"global","desc":"{desc}","origin":"a\nb"}}],"plugins":{{}}}}"#
        ));
        let probe = parse_report(json.as_bytes()).unwrap().probe.unwrap();
        let key = &probe.keys[0];
        let kept = key.desc.as_deref().unwrap();
        assert!(kept.starts_with("a\u{1b}[31m\u{7}x"));
        assert!(kept.ends_with('\u{202e}'));
        assert_eq!(kept.chars().count(), 200);
        assert_eq!(key.origin.as_deref(), Some("a\nb"));
        assert!(key.rhs.is_none() && !key.callback);
    }

    #[test]
    fn a_plugin_outside_the_closed_list_is_dropped() {
        let json = report_json(r#","probe":{"keys":[],"plugins":{"evil.nvim":"loaded","which-key.nvim":"loaded"}}"#);
        let probe = parse_report(json.as_bytes()).unwrap().probe.unwrap();
        assert_eq!(
            probe.plugins.keys().map(String::as_str).collect::<Vec<_>>(),
            ["which-key.nvim"]
        );
    }

    #[test]
    fn a_broken_probe_keeps_the_leader_and_maps() {
        for probe in [
            r#"{"keys":[{"mode":"i","lhs":"<C-b>","scope":"window"}],"plugins":{}}"#,
            r#"{"keys":[],"plugins":{"blink.cmp":"sleeping"}}"#,
            r#"{"keys":"none","plugins":{}}"#,
            r#"{"keys":[],"plugins":[]}"#,
            r#""probe""#,
            "7",
            "null",
        ] {
            let json = format!(
                r#"{{"v":1,"mapleader":" ","timeoutlen":300,"timeout":true,"maps":[{{"lhs":"H","rhs":":bnext<CR>"}}],"probe":{probe}}}"#
            );
            let report = parse_report(json.as_bytes()).unwrap_or_else(|| panic!("the report must survive {probe}"));
            assert!(report.probe.is_none(), "{probe}");
            assert_eq!(report.mapleader.as_deref(), Some(" "));
            assert_eq!(report.maps.len(), 1);
        }
    }
}
