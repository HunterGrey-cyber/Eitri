//! How the helper answers a language server's request when no editor can: the bridge is away, or it went away with
//! the request unanswered. An edit or a document shown for text the editor may since have changed is refused, never
//! queued; what a server needs to keep working (its configuration, its registrations, progress tokens) is answered as
//! the editor last answered it or accepted.

use std::collections::VecDeque;

use super::message;

/// JSON-RPC `RequestFailed`.
pub const REQUEST_FAILED: i64 = -32803;
pub const NOT_CONNECTED: &str = "the editor is not connected";
/// Why the oldest of the server's requests is answered by the helper while an editor is attached.
pub const TOO_MANY_UNANSWERED: &str = "too many unanswered requests";
/// Configuration answers the helper keeps, and their total bytes (keys and answers).
pub const CONFIG_ANSWERS_MAX: usize = 256;
pub const CONFIG_BYTES_MAX: usize = 4 * 1024 * 1024;
/// The largest `workspace/workspaceFolders` answer kept.
pub const FOLDERS_MAX: usize = 1024 * 1024;
/// The largest `workspace/configuration` params kept as a key.
pub const CONFIG_KEY_MAX: usize = 64 * 1024;

pub const CONFIGURATION: &str = "workspace/configuration";
pub const WORKSPACE_FOLDERS: &str = "workspace/workspaceFolders";
pub const REGISTER: &str = "client/registerCapability";
pub const UNREGISTER: &str = "client/unregisterCapability";

const REFRESHES: &[&str] = &[
    "workspace/semanticTokens/refresh",
    "workspace/inlayHint/refresh",
    "workspace/codeLens/refresh",
    "workspace/diagnostic/refresh",
    "workspace/inlineValue/refresh",
    "workspace/foldingRange/refresh",
];

/// What the editor last answered to `workspace/configuration`, keyed by the request's params as canonical JSON. The
/// oldest answer goes first when either bound is reached.
#[derive(Default)]
pub struct ConfigAnswers {
    entries: VecDeque<(String, Vec<u8>)>,
    bytes: usize,
}

impl ConfigAnswers {
    pub fn insert(&mut self, key: String, answer: Vec<u8>) {
        let size = key.len() + answer.len();
        if let Some(at) = self.entries.iter().position(|(known, _)| *known == key) {
            let (known, old) = self.entries.remove(at).expect("present");
            self.bytes -= known.len() + old.len();
        }
        if size > CONFIG_BYTES_MAX {
            return;
        }
        while self.entries.len() >= CONFIG_ANSWERS_MAX || self.bytes + size > CONFIG_BYTES_MAX {
            let Some((known, old)) = self.entries.pop_front() else {
                break;
            };
            self.bytes -= known.len() + old.len();
        }
        self.bytes += size;
        self.entries.push_back((key, answer));
    }

    pub fn get(&self, key: &str) -> Option<&[u8]> {
        self.entries
            .iter()
            .find(|(known, _)| known == key)
            .map(|(_, answer)| answer.as_slice())
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// What the editor answered last, for the requests the table answers from it.
#[derive(Default)]
pub struct Remembered {
    pub config: ConfigAnswers,
    pub folders: Option<Vec<u8>>,
}

pub fn is_registration(method: &str) -> bool {
    method == REGISTER || method == UNREGISTER
}

/// The response the helper writes to the server for request `id` (canonical JSON) of `method`. `config_key` is the
/// canonical JSON of a `workspace/configuration` request's params; `why` is the text of a refusal.
pub fn answer(id: &str, method: &str, config_key: Option<&str>, remembered: &Remembered, why: &str) -> Vec<u8> {
    let result = |text: &str| message::response(id, text);
    let failed = || message::error_response(id, REQUEST_FAILED, why);
    match method {
        CONFIGURATION => match config_key.and_then(|key| remembered.config.get(key)) {
            Some(answer) => result(&String::from_utf8_lossy(answer)),
            None => failed(),
        },
        WORKSPACE_FOLDERS => match &remembered.folders {
            Some(answer) => result(&String::from_utf8_lossy(answer)),
            None => result("null"),
        },
        REGISTER | UNREGISTER | "window/workDoneProgress/create" | "window/showMessageRequest" => result("null"),
        "workspace/applyEdit" => result(&serde_json::json!({"applied": false, "failureReason": why}).to_string()),
        "window/showDocument" => result(r#"{"success":false}"#),
        refresh if REFRESHES.contains(&refresh) => result("null"),
        _ => failed(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_answers_keep_the_newest_within_their_bounds() {
        let mut answers = ConfigAnswers::default();
        for n in 0..CONFIG_ANSWERS_MAX + 10 {
            answers.insert(format!("k{n}"), b"[1]".to_vec());
        }
        assert_eq!(answers.len(), CONFIG_ANSWERS_MAX);
        assert!(answers.get("k0").is_none());
        assert_eq!(answers.get(&format!("k{}", CONFIG_ANSWERS_MAX + 9)), Some(&b"[1]"[..]));
        answers.insert("big".into(), vec![b'x'; CONFIG_BYTES_MAX / 2]);
        answers.insert("bigger".into(), vec![b'x'; CONFIG_BYTES_MAX / 2]);
        assert!(answers.get("big").is_none());
        assert!(answers.get("bigger").is_some());
        answers.insert("huge".into(), vec![b'x'; CONFIG_BYTES_MAX]);
        assert!(answers.get("huge").is_none());
    }
}
