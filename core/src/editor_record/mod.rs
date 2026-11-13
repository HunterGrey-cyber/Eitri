//! The editor's record: which files the embedded nvim had open, in which splits and tab pages, and where each
//! cursor stood, so the next start can open them again. Only names and positions: never buffer text, options,
//! mappings or commands. File names travel as base64 of their bytes (standard alphabet, padded), since JSON strings
//! must be UTF-8 and file names need not be.
//!
//! Every record that comes in -- read from the state file, answered by a capture, or left by nvim at its exit -- goes
//! through [`parse`], which holds every bound; [`Record::to_json`] writes what `parse` reads back, and nothing is
//! written past [`RECORD_MAX`], so nothing is written that its reader refuses.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use base64::Engine as _;

mod file;
mod parse;
#[cfg(test)]
mod tests;
mod writer;

pub use file::{write_input, RecordFile, StartRead, INPUT_FILE_NAME};
pub use parse::{parse, parse_handoff, RecordError};
pub use writer::{RecordWriter, WriterAnswer};

/// The largest record read or written, in bytes of JSON.
pub const RECORD_MAX: usize = 256 * 1024;

/// The record's format version; a record of another version is read as nothing.
pub const VERSION: u64 = 1;

/// At most this many tab pages.
pub const MAX_TABS: usize = 16;
/// At most this many windows in one tab page.
pub const MAX_LEAVES: usize = 32;
/// A window tree at most this many levels deep; a tab page that is one window is one level.
pub const MAX_DEPTH: usize = 16;
/// At most this many listed buffers outside the windows.
pub const MAX_BUFS: usize = 256;
/// A file name is 1 to this many bytes.
pub const MAX_NAME: usize = 4096;
/// The largest line, column or top line a view may name: 2^31, which holds nvim's `MAXCOL` (2^31 - 1) for a
/// cursor that wants the end of the line.
pub const MAX_POSITION: u32 = 1 << 31;
/// The largest window or editor size, in cells.
pub const MAX_CELLS: u32 = 10_000;

/// One record: the project root it belongs to, its tab pages and the listed buffers no window showed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// The project root's bytes.
    pub root: Vec<u8>,
    /// The current tab page, counted from 1 among `tabs`; `None` when the current one was not kept.
    pub tab: Option<u32>,
    pub tabs: Vec<Tab>,
    /// Absolute names of listed buffers shown in no kept window.
    pub bufs: Vec<Vec<u8>>,
}

/// One tab page: the editor's text area when it was captured, in cells, and its window tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tab {
    pub cols: u32,
    pub lines: u32,
    pub tree: Node,
}

/// A window tree as nvim's `winlayout()` gives it: a window, or windows side by side (`Row`) or stacked (`Col`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    Leaf(Leaf),
    Row(Vec<Node>),
    Col(Vec<Node>),
}

/// One window: the file it shows, its view (`winsaveview`'s `lnum`, `col`, `curswant`, `topline`, `leftcol`) and
/// its size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leaf {
    /// The absolute file name's bytes.
    pub file: Vec<u8>,
    pub lnum: u32,
    pub col: u32,
    pub want: u32,
    pub top: u32,
    pub left: u32,
    pub w: u32,
    pub h: u32,
    /// The tab page's current window.
    pub cur: bool,
}

pub(crate) fn encode_name(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

impl Record {
    /// A record for `root` with nothing in it.
    pub fn empty(root: &Path) -> Record {
        Record {
            root: root.as_os_str().as_bytes().to_vec(),
            tab: None,
            tabs: Vec::new(),
            bufs: Vec::new(),
        }
    }

    /// The record as JSON, in the shape [`parse`] reads: `{"v", "root", "tab", "tabs", "bufs"}`, `tab` left out
    /// when there is none.
    pub fn to_json(&self) -> String {
        let mut out = String::with_capacity(256);
        let _ = write!(out, "{{\"v\":{VERSION},\"root\":\"{}\"", encode_name(&self.root));
        if let Some(tab) = self.tab {
            let _ = write!(out, ",\"tab\":{tab}");
        }
        out.push_str(",\"tabs\":[");
        for (i, tab) in self.tabs.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            write_tab(&mut out, tab);
        }
        out.push_str("],\"bufs\":[");
        for (i, buf) in self.bufs.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let _ = write!(out, "\"{}\"", encode_name(buf));
        }
        out.push_str("]}");
        out
    }

    /// Makes the JSON at most `max` bytes by dropping listed buffers from the end, then whole tabs from the end.
    /// The first tab is never dropped: with every count bound held, one tab is well under [`RECORD_MAX`], so a
    /// record trimmed to that always fits. A current tab that goes leaves `tab` empty. Returns one description
    /// per omitted entry; these descriptions contain private names and must not be logged.
    pub fn fit_budget(&mut self, max: usize) -> Vec<String> {
        let mut dropped = Vec::new();
        let mut len = self.to_json().len();
        while len > max {
            let Some(buf) = self.bufs.pop() else { break };
            // `"<base64>"`, and the comma before it unless it was the only one.
            len -= encode_name(&buf).len() + 2 + usize::from(!self.bufs.is_empty());
            dropped.push(format!("the listed buffer {}", String::from_utf8_lossy(&buf)));
        }
        while self.to_json().len() > max && self.tabs.len() > 1 {
            let gone = self.tabs.pop().map_or(0, |t| leaves(&t.tree));
            let number = self.tabs.len() + 1;
            if self.tab.is_some_and(|t| t as usize > self.tabs.len()) {
                self.tab = None;
            }
            dropped.push(format!("tab page {number} ({gone} windows)"));
        }
        dropped
    }

    /// How many different files the record names, in windows and as listed buffers.
    pub fn file_count(&self) -> usize {
        let mut names: BTreeSet<&[u8]> = self.bufs.iter().map(Vec::as_slice).collect();
        for tab in &self.tabs {
            collect_names(&tab.tree, &mut names);
        }
        names.len()
    }
}

fn write_tab(out: &mut String, tab: &Tab) {
    let _ = write!(out, "{{\"cols\":{},\"lines\":{},\"tree\":", tab.cols, tab.lines);
    write_node(out, &tab.tree);
    out.push('}');
}

fn write_node(out: &mut String, node: &Node) {
    match node {
        Node::Leaf(l) => {
            let _ = write!(
                out,
                "{{\"leaf\":{{\"f\":\"{}\",\"lnum\":{},\"col\":{},\"want\":{},\"top\":{},\"left\":{},\"w\":{},\
                 \"h\":{},\"cur\":{}}}}}",
                encode_name(&l.file),
                l.lnum,
                l.col,
                l.want,
                l.top,
                l.left,
                l.w,
                l.h,
                l.cur
            );
        }
        Node::Row(children) | Node::Col(children) => {
            out.push_str(if matches!(node, Node::Row(_)) {
                "{\"row\":["
            } else {
                "{\"col\":["
            });
            for (i, child) in children.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_node(out, child);
            }
            out.push_str("]}");
        }
    }
}

fn leaves(node: &Node) -> usize {
    match node {
        Node::Leaf(_) => 1,
        Node::Row(children) | Node::Col(children) => children.iter().map(leaves).sum(),
    }
}

fn collect_names<'a>(node: &'a Node, names: &mut BTreeSet<&'a [u8]>) {
    match node {
        Node::Leaf(l) => {
            names.insert(&l.file);
        }
        Node::Row(children) | Node::Col(children) => children.iter().for_each(|c| collect_names(c, names)),
    }
}
