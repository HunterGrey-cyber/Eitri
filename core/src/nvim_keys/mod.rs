//! The nvim report (spec `2026-09-26-panel-round2-design.md` §3): `mapleader`, `timeoutlen`/
//! `timeout`, and every global Normal-mode mapping, read off a fourth nvim->shell push socket
//! (Task 4 builds the socket itself; this module only shapes and classifies the JSON line it
//! carries). `v: 1` is the only wire version this reads (spec §3.2); anything else -- a `v` this
//! crate does not know, or JSON it cannot parse at all -- is dropped whole, and the caller keeps
//! its last good report (spec §3.8).

pub mod classify;
pub mod feed;

pub use classify::{classify, translate_lhs};

use serde::Deserialize;

/// One nvim Normal-mode mapping (`vim.api.nvim_get_keymap("n")`, spec §3.1): `lhs` already through
/// `vim.fn.keytrans`, `rhs` absent for a Lua callback, `desc` optional, and `callback` true exactly
/// when there is no `rhs` because a Lua function runs it instead.
#[derive(Debug, Clone, Deserialize)]
pub struct NvimMap {
    pub lhs: String,
    pub rhs: Option<String>,
    pub desc: Option<String>,
    #[serde(default)]
    pub callback: bool,
}

/// One report from the `nvim_keys` feed (spec §3.1-3.2). `mapleader` is already `keytrans`'d
/// (`"<Space>"`, never a raw `" "`) and absent when nvim's own `g:mapleader` is unset or empty.
#[derive(Debug, Clone, Deserialize)]
pub struct NvimReport {
    pub v: u32,
    pub mapleader: Option<String>,
    pub timeoutlen: u32,
    pub timeout: bool,
    pub maps: Vec<NvimMap>,
}

/// The report's leader as the classifier reads it: `mapleader` when nvim set one, or `\` -- vim's
/// own default -- when it did not (`nvim: map.txt` `*mapleader*`).
pub fn nvim_leader_token(report: &NvimReport) -> &str {
    report.mapleader.as_deref().unwrap_or("\\")
}

/// `line` parses only at wire version 1 (spec §3.2). Unparsable JSON, or a `v` this crate does not
/// know, is `None` -- never a partially-applied report -- so the caller keeps its last good one
/// (spec §3.8).
pub fn parse_report(line: &[u8]) -> Option<NvimReport> {
    serde_json::from_slice::<NvimReport>(line)
        .ok()
        .filter(|report| report.v == 1)
}
