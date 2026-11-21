//! What a language server says while no bridge is attached, kept for the next one: its notifications in order, and
//! the registrations the helper accepted for it.
//!
//! Only the newest diagnostics per document and the newest progress per token matter to an editor that comes back,
//! so an older one is dropped when a newer one arrives (the newer keeps its own place in the order); log messages
//! past a thousand are dropped. The bytes held are counted so the caller can stop a server whose queue passes its
//! bound.

use std::collections::{HashMap, VecDeque};

use eitri_server_proto::{LspRegistration, LSP_REGISTRATIONS_MAX, LSP_REGISTRATIONS_PARAMS_MAX};
use serde::Deserialize;
use serde_json::Value;

use super::message;

pub const LOG_MESSAGES_MAX: usize = 1000;

/// How a notification collapses in the queue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Collapse {
    /// `textDocument/publishDiagnostics` for this uri.
    Diagnostics(String),
    /// `$/progress` for this token (canonical JSON).
    Progress(String),
    /// `window/logMessage`.
    Log,
    Plain,
}

impl Collapse {
    pub fn of(method: &str, bytes: &[u8]) -> Collapse {
        #[derive(Deserialize)]
        struct Diagnostics {
            params: DiagnosticsParams,
        }
        #[derive(Deserialize)]
        struct DiagnosticsParams {
            uri: String,
        }
        #[derive(Deserialize)]
        struct Progress {
            params: ProgressParams,
        }
        #[derive(Deserialize)]
        struct ProgressParams {
            token: Value,
        }
        match method {
            "textDocument/publishDiagnostics" => serde_json::from_slice::<Diagnostics>(bytes)
                .map(|parsed| Collapse::Diagnostics(parsed.params.uri))
                .unwrap_or(Collapse::Plain),
            "$/progress" => serde_json::from_slice::<Progress>(bytes)
                .ok()
                .and_then(|parsed| message::id_key(&parsed.params.token))
                .map(Collapse::Progress)
                .unwrap_or(Collapse::Plain),
            "window/logMessage" => Collapse::Log,
            _ => Collapse::Plain,
        }
    }

    fn key(&self) -> Option<String> {
        match self {
            Collapse::Diagnostics(uri) => Some(format!("d{uri}")),
            Collapse::Progress(token) => Some(format!("p{token}")),
            Collapse::Log | Collapse::Plain => None,
        }
    }
}

struct Entry {
    bytes: Vec<u8>,
    key: Option<String>,
}

#[derive(Default)]
pub struct Queue {
    /// In arrival order; a dropped entry leaves a hole until the next compaction.
    entries: VecDeque<Option<Entry>>,
    /// The sequence number of `entries[0]`.
    base: u64,
    by_key: HashMap<String, u64>,
    bytes: usize,
    live: usize,
    logs: usize,
    dropped: u64,
}

impl Queue {
    pub fn push(&mut self, bytes: Vec<u8>, collapse: Collapse) {
        let log = collapse == Collapse::Log;
        if log && self.logs >= LOG_MESSAGES_MAX {
            self.dropped += 1;
            return;
        }
        let key = collapse.key();
        if let Some(older) = key.as_ref().and_then(|key| self.by_key.remove(key)) {
            let slot = (older - self.base) as usize;
            if let Some(entry) = self.entries.get_mut(slot).and_then(Option::take) {
                self.bytes -= entry.bytes.len();
                self.live -= 1;
                self.dropped += 1;
            }
        }
        let sequence = self.base + self.entries.len() as u64;
        if let Some(key) = &key {
            self.by_key.insert(key.clone(), sequence);
        }
        self.bytes += bytes.len();
        self.live += 1;
        self.logs += log as usize;
        self.entries.push_back(Some(Entry { bytes, key }));
        self.compact();
    }

    /// Holes are removed once they outnumber the entries, so a server that repeats one document's diagnostics keeps
    /// the queue's length bounded by what it holds.
    fn compact(&mut self) {
        if self.entries.len() < 64 || self.entries.len() - self.live <= self.live {
            return;
        }
        let entries: VecDeque<_> = self.entries.drain(..).flatten().map(Some).collect();
        self.base = 0;
        self.by_key.clear();
        for (at, entry) in entries.iter().enumerate() {
            if let Some(key) = entry.as_ref().and_then(|entry| entry.key.clone()) {
                self.by_key.insert(key, at as u64);
            }
        }
        self.entries = entries;
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Counts notifications dropped before they reached the queue.
    pub fn add_dropped(&mut self, dropped: u64) {
        self.dropped += dropped;
    }

    /// The notifications in order and how many were dropped since the last take; the queue is empty after.
    pub fn take(&mut self) -> (Vec<Vec<u8>>, u64) {
        let messages = self.entries.drain(..).flatten().map(|entry| entry.bytes).collect();
        let dropped = self.dropped;
        *self = Queue::default();
        (messages, dropped)
    }
}

/// The registrations accepted while detached, in order, within `LSP_REGISTRATIONS_MAX` entries and
/// `LSP_REGISTRATIONS_PARAMS_MAX` bytes of params.
#[derive(Default)]
pub struct Registrations {
    kept: Vec<LspRegistration>,
    bytes: usize,
}

/// A bound was reached: the server is stopped.
#[derive(Debug)]
pub struct Full;

impl Registrations {
    pub fn push(&mut self, method: &str, params: Vec<u8>) -> Result<(), Full> {
        if self.kept.len() >= LSP_REGISTRATIONS_MAX || self.bytes + params.len() > LSP_REGISTRATIONS_PARAMS_MAX {
            return Err(Full);
        }
        self.bytes += params.len();
        self.kept.push(LspRegistration {
            method: method.to_owned(),
            params,
        });
        Ok(())
    }

    pub fn take(&mut self) -> Vec<LspRegistration> {
        self.bytes = 0;
        std::mem::take(&mut self.kept)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diagnostics(uri: &str, version: u32) -> Vec<u8> {
        format!(r#"{{"method":"textDocument/publishDiagnostics","params":{{"uri":"{uri}","version":{version}}}}}"#)
            .into_bytes()
    }

    #[test]
    fn newer_diagnostics_replace_older_and_keep_their_own_place() {
        let mut queue = Queue::default();
        let plain = b"{\"method\":\"x\"}".to_vec();
        for message in [
            diagnostics("a", 1),
            plain.clone(),
            diagnostics("b", 1),
            diagnostics("a", 2),
        ] {
            let collapse = Collapse::of(
                if message == plain {
                    "x"
                } else {
                    "textDocument/publishDiagnostics"
                },
                &message,
            );
            queue.push(message, collapse);
        }
        let expected = [plain, diagnostics("b", 1), diagnostics("a", 2)];
        assert_eq!(queue.bytes(), expected.iter().map(Vec::len).sum::<usize>());
        let (messages, dropped) = queue.take();
        assert_eq!(messages, expected);
        assert_eq!(dropped, 1);
        assert_eq!(queue.bytes(), 0);
    }

    #[test]
    fn repeated_collapses_keep_the_queue_short_and_logs_stop_at_their_cap() {
        let mut queue = Queue::default();
        for version in 0..10_000 {
            queue.push(diagnostics("a", version), Collapse::Diagnostics("a".into()));
        }
        assert!(queue.entries.len() < 128, "{}", queue.entries.len());
        for _ in 0..LOG_MESSAGES_MAX + 5 {
            queue.push(b"{}".to_vec(), Collapse::Log);
        }
        assert_eq!(queue.logs, LOG_MESSAGES_MAX);
        let (messages, dropped) = queue.take();
        assert_eq!(messages.len(), LOG_MESSAGES_MAX + 1);
        assert_eq!(messages[0], diagnostics("a", 9_999));
        assert_eq!(dropped, 9_999 + 5);
    }

    #[test]
    fn registrations_stop_at_either_bound() {
        let mut kept = Registrations::default();
        for _ in 0..LSP_REGISTRATIONS_MAX {
            kept.push("client/registerCapability", b"{}".to_vec()).unwrap();
        }
        assert!(kept.push("client/registerCapability", b"{}".to_vec()).is_err());
        assert_eq!(kept.take().len(), LSP_REGISTRATIONS_MAX);
        assert!(kept
            .push(
                "client/registerCapability",
                vec![b' '; LSP_REGISTRATIONS_PARAMS_MAX + 1]
            )
            .is_err());
    }
}
