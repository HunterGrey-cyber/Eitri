//! What the helper reads of a JSON-RPC message: whether it is a request, a notification or a response, its id and its
//! method. Ids are kept as their canonical JSON text (`1`, `"a"`), which is also what goes back into a message the
//! helper writes for that id.

use serde::Deserialize;
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    Request { id: String, method: String },
    Notification { method: String },
    Response { id: String },
}

#[derive(Deserialize)]
struct Envelope {
    #[serde(default)]
    id: Option<Value>,
    #[serde(default)]
    method: Option<String>,
}

/// The message's kind, or `None` when it is not a JSON-RPC object the helper understands (it is then passed on as it
/// is while a bridge is attached).
pub fn classify(bytes: &[u8]) -> Option<Kind> {
    let envelope: Envelope = serde_json::from_slice(bytes).ok()?;
    let id = envelope.id.as_ref().and_then(id_key);
    match (id, envelope.method) {
        (Some(id), Some(method)) => Some(Kind::Request { id, method }),
        (None, Some(method)) => Some(Kind::Notification { method }),
        (Some(id), None) => Some(Kind::Response { id }),
        (None, None) => None,
    }
}

/// The longest id the helper passes on, as canonical JSON text (the scanner keeps no longer value either). A request
/// with a longer id is answered with an error and passed neither way, so the ids the helper keeps stay small; such an
/// id is still an id, so the request is not taken for a notification.
pub const ID_MAX: usize = 1024;

/// An id's canonical text: a number or a string, as JSON. Other values are not ids.
pub fn id_key(id: &Value) -> Option<String> {
    match id {
        Value::Number(_) | Value::String(_) => serde_json::to_string(id).ok(),
        _ => None,
    }
}

/// A value's canonical JSON: object keys sorted (serde_json's map is ordered by key in this build: no
/// `preserve_order`), no insignificant whitespace.
pub fn canonical(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

/// The `params` of a message, if it parses and has them.
pub fn params(bytes: &[u8]) -> Option<Value> {
    #[derive(Deserialize)]
    struct Params {
        #[serde(default)]
        params: Option<Value>,
    }
    serde_json::from_slice::<Params>(bytes).ok()?.params
}

/// The `result` of a response, if it parses and has one (`null` included).
pub fn result(bytes: &[u8]) -> Option<Value> {
    let value: Value = serde_json::from_slice(bytes).ok()?;
    match value {
        Value::Object(mut map) => map.remove("result"),
        _ => None,
    }
}

/// A response to `id` carrying `result` (JSON text).
pub fn response(id: &str, result: &str) -> Vec<u8> {
    format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{result}}}"#).into_bytes()
}

/// An error response to `id`.
pub fn error_response(id: &str, code: i64, message: &str) -> Vec<u8> {
    let message = serde_json::to_string(message).unwrap_or_else(|_| "\"\"".to_owned());
    format!(r#"{{"jsonrpc":"2.0","id":{id},"error":{{"code":{code},"message":{message}}}}}"#).into_bytes()
}

/// `$/cancelRequest` for `id`.
pub fn cancel(id: &str) -> Vec<u8> {
    format!(r#"{{"jsonrpc":"2.0","method":"$/cancelRequest","params":{{"id":{id}}}}}"#).into_bytes()
}

/// The longest key the scanner compares and the longest scalar value it keeps; anything longer is not an id or method.
const KEY_MAX: usize = 16;
const VALUE_MAX: usize = 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Expect {
    #[default]
    Object,
    Key,
    Colon,
    Value,
    Comma,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    Id,
    Method,
    Other,
}

/// Finds the top-level `id` and `method` of a message too large to keep, from its bytes as they stream past: nested
/// values and strings are skipped, only short top-level scalars are held, so the memory it uses does not depend on
/// the message's size.
#[derive(Debug, Default)]
pub struct TopLevelScan {
    depth: u32,
    in_string: bool,
    escape: bool,
    expect: Expect,
    key: Vec<u8>,
    field: Option<Field>,
    value: Vec<u8>,
    /// A top-level scalar (string or bare word) is being kept in `value`.
    capturing: bool,
    capturing_string: bool,
    broken: bool,
    pub id: Option<String>,
    pub method: Option<String>,
}

impl TopLevelScan {
    pub fn feed(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if self.broken {
                return;
            }
            self.step(byte);
        }
    }

    pub fn kind(&self) -> Option<Kind> {
        match (self.id.clone(), self.method.clone()) {
            (Some(id), Some(method)) => Some(Kind::Request { id, method }),
            (None, Some(method)) => Some(Kind::Notification { method }),
            (Some(id), None) => Some(Kind::Response { id }),
            (None, None) => None,
        }
    }

    fn keep(&mut self, byte: u8) {
        if self.value.len() < VALUE_MAX + 1 {
            self.value.push(byte);
        }
    }

    fn step(&mut self, byte: u8) {
        if self.in_string {
            let top_key = self.depth == 1 && self.expect == Expect::Key;
            if top_key && self.key.len() <= KEY_MAX {
                self.key.push(byte);
            }
            if self.capturing_string {
                self.keep(byte);
            }
            if self.escape {
                self.escape = false;
            } else if byte == b'\\' {
                self.escape = true;
            } else if byte == b'"' {
                self.in_string = false;
                if top_key {
                    self.key.pop();
                    self.field = Some(match self.key.as_slice() {
                        b"id" => Field::Id,
                        b"method" => Field::Method,
                        _ => Field::Other,
                    });
                    self.key.clear();
                    self.expect = Expect::Colon;
                } else if self.capturing_string {
                    self.finish_value();
                }
            }
            return;
        }
        if self.capturing && !self.capturing_string {
            if matches!(byte, b',' | b'}' | b']') || byte.is_ascii_whitespace() {
                self.finish_value();
            } else {
                self.keep(byte);
                return;
            }
        }
        match byte {
            b' ' | b'\t' | b'\r' | b'\n' => {}
            b'"' => {
                self.in_string = true;
                if self.depth == 1 && self.expect == Expect::Value {
                    self.capturing = true;
                    self.capturing_string = true;
                    self.value.clear();
                    self.value.push(b'"');
                }
            }
            b'{' | b'[' => {
                if self.depth == 0 {
                    if byte != b'{' || self.expect != Expect::Object {
                        self.broken = true;
                        return;
                    }
                    self.expect = Expect::Key;
                } else if self.depth == 1 {
                    // A nested value: not an id or a method.
                    self.expect = Expect::Comma;
                }
                self.depth += 1;
            }
            b'}' | b']' => {
                if self.depth == 0 {
                    self.broken = true;
                    return;
                }
                self.depth -= 1;
                if self.depth == 1 {
                    self.expect = Expect::Comma;
                }
            }
            b':' if self.depth == 1 => {
                if self.expect == Expect::Colon {
                    self.expect = Expect::Value;
                }
            }
            b',' if self.depth == 1 => self.expect = Expect::Key,
            _ if self.depth == 1 && self.expect == Expect::Value => {
                self.capturing = true;
                self.capturing_string = false;
                self.value.clear();
                self.value.push(byte);
            }
            _ => {}
        }
    }

    fn finish_value(&mut self) {
        self.capturing = false;
        self.capturing_string = false;
        self.expect = Expect::Comma;
        if self.value.len() > VALUE_MAX {
            return;
        }
        let parsed: Option<Value> = serde_json::from_slice(&self.value).ok();
        match (self.field, parsed) {
            (Some(Field::Id), Some(value)) => self.id = id_key(&value),
            (Some(Field::Method), Some(Value::String(method))) => self.method = Some(method),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(text: &str, piece: usize) -> TopLevelScan {
        let mut scan = TopLevelScan::default();
        for part in text.as_bytes().chunks(piece) {
            scan.feed(part);
        }
        scan
    }

    #[test]
    fn the_scanner_finds_top_level_ids_and_methods_in_any_order_and_any_pieces() {
        for piece in [1, 3, 1000] {
            let request = scan(
                r#"{"jsonrpc":"2.0","params":{"id":9,"method":"x","s":"\"}"},"id" : 12,"method":"a/b"}"#,
                piece,
            );
            assert_eq!(
                request.kind(),
                Some(Kind::Request {
                    id: "12".into(),
                    method: "a/b".into()
                })
            );
            let response = scan(r#"{"result":["id",{"id":1}],"id":"q\"7"}"#, piece);
            assert_eq!(response.kind(), Some(Kind::Response { id: r#""q\"7""#.into() }));
            let notification = scan(r#"{"method":"n","params":[1,2,{"a":[]}]}"#, piece);
            assert_eq!(notification.kind(), Some(Kind::Notification { method: "n".into() }));
        }
        assert_eq!(scan(r#"[{"id":1}]"#, 4).kind(), None);
        assert_eq!(scan(r#"{"id":null,"result":1}"#, 4).kind(), None);
    }

    #[test]
    fn classify_reads_the_kind_and_canonical_ids() {
        assert_eq!(
            classify(br#"{"method":"m","id":"x","params":{}}"#),
            Some(Kind::Request {
                id: "\"x\"".into(),
                method: "m".into()
            })
        );
        assert_eq!(
            classify(br#"{"id":3,"result":null}"#),
            Some(Kind::Response { id: "3".into() })
        );
        assert_eq!(classify(b"not json"), None);
        let a: Value = serde_json::from_str(r#"{"b":1,"a":{"d":2,"c":3}}"#).unwrap();
        let b: Value = serde_json::from_str(r#"{"a":{"c":3,"d":2},"b":1}"#).unwrap();
        assert_eq!(canonical(&a), canonical(&b));
    }
}
