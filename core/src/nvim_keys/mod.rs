//! Reads `mapleader`, `timeoutlen`, `timeout` and global Normal-mode mappings from nvim.
//! This module shapes and classifies the JSON line delivered by the nvim-to-shell push socket.
//! Only wire version `v: 1` is accepted. Unknown versions and malformed JSON are dropped whole,
//! so the caller can keep its last good report.

pub mod classify;
pub mod feed;

pub use classify::{classify, translate_lhs};

use serde::Deserialize;

/// One nvim Normal-mode mapping: `lhs` already through
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

/// One report from the `nvim_keys` feed. `mapleader` is already `keytrans`'d
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

/// `line` parses only at wire version 1. Unparsable JSON, or a `v` this crate does not
/// know, is `None` -- never a partially-applied report -- so the caller keeps its last good one
///.
pub fn parse_report(line: &[u8]) -> Option<NvimReport> {
    serde_json::from_slice::<NvimReport>(line)
        .ok()
        .filter(|report| report.v == 1)
}
