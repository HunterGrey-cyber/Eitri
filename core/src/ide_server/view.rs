//! The proposal view's constant chunks and their typed calls.
//!
//! A view is two windows in the person's nvim: the file as the CLI read it, and what it proposes to write. The chunks
//! are constants and every value a msgpack argument, as for all Lua sent to nvim, and a large text is moved, never
//! copied: these calls are made on the GTK thread.

use std::os::unix::ffi::OsStringExt;

use rmpv::Value;

use super::types::DiffRequest;
use crate::editor_context::compose::{IDE_FILE_SCHEME, IDE_PROPOSAL_SCHEME};
use crate::editor_rpc::EditorRpc;
use crate::editor_tools::stamp::Stamp;
use crate::nvim_rpc::Pending;

/// Opens a view. Its stamp's rule holds: a chunk that runs late shows nothing.
pub const IDE_DIFF_OPEN: &str = crate::editor_rpc::takeover_chunk!(
    concat!(
        include_str!("../editor_tools/prelude.lua"),
        include_str!("ide_view.lua")
    ),
    r#"
local ide = rawget(_G, '__eitri_ide')
if ide then __takeover_adopt('ide', function()
  if rawget(_G, '__eitri_ide') ~= ide then return end
  local ids = {}
  for id in pairs(ide.views) do ids[#ids+1] = id end
  for _, id in ipairs(ids) do pcall(ide.close, id) end
  _G.__eitri_ide = nil
end) end
"#
);

/// No prelude: a close that runs late still closes. Argument: the view id.
pub const IDE_DIFF_CLOSE: &str = r#"
local view = ...
local ide = rawget(_G, '__eitri_ide')
if ide then ide.close(view) end
"#;

/// Hands over the queued events, oldest first: at least one, and at most 8 MiB of them, keeping the rest, so a take
/// costs nvim and the GTK thread a bounded answer however much is queued.
pub const IDE_TAKE_EVENTS: &str = r#"
local ide = rawget(_G, '__eitri_ide')
if not ide then return {} end
local LIMIT = 8 * 1024 * 1024
local LINE_HEADER = 5
local taken, bytes = {}, 0
for _, event in ipairs(ide.events) do
  local size = 64
  for _, line in ipairs(event.lines or {}) do
    size = size + #line + LINE_HEADER
  end
  if #taken > 0 and bytes + size > LIMIT then break end
  taken[#taken + 1] = event
  bytes = bytes + size
end
local kept = {}
for index = #taken + 1, #ide.events do
  kept[#kept + 1] = ide.events[index]
end
ide.events = kept
return taken
"#;

/// The names of a view's two buffers. The real path would clash with the person's own buffer of that file, and the
/// schemes keep both out of what the editor feed reports as the focused file. The view's id keeps two views of one
/// file apart.
fn buffer_names(view: u64, rel: &str) -> (String, String) {
    let tag = format!("{:06x}/{rel}", view & 0xff_ffff);
    (format!("{IDE_FILE_SCHEME}{tag}"), format!("{IDE_PROPOSAL_SCHEME}{tag}"))
}

/// Moves the prebuilt lines into the call's arguments.
pub fn send_open(rpc: &dyn EditorRpc, stamp: &Stamp, view: u64, diff: DiffRequest) -> Pending {
    let DiffRequest {
        tab_name: _,
        real_path,
        rel,
        file_text,
        file_lines,
        proposal_lines,
        shape,
    } = diff;
    let (file_name, proposal_name) = buffer_names(view, &rel);
    let fields = vec![
        (Value::from("view"), Value::from(view)),
        (Value::from("file_name"), Value::Binary(file_name.into_bytes())),
        (Value::from("proposal_name"), Value::Binary(proposal_name.into_bytes())),
        (Value::from("rel"), Value::Binary(rel.into_bytes())),
        (
            Value::from("real"),
            Value::Binary(real_path.into_os_string().into_vec()),
        ),
        (Value::from("file_lines"), Value::Array(file_lines)),
        (Value::from("proposal_lines"), Value::Array(proposal_lines)),
        (Value::from("file_text"), Value::Binary(file_text)),
        (Value::from("crlf"), Value::from(shape.crlf)),
        (Value::from("eol"), Value::from(shape.eol)),
    ];
    rpc.exec_lua(IDE_DIFF_OPEN, vec![stamp.to_value(), Value::Map(fields)])
}

/// What an open came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Opened {
    Opened,
    Refused(String),
    Expired,
}

/// The field `name` of a map.
fn field<'a>(value: &'a Value, name: &str) -> Option<&'a Value> {
    let fields = value.as_map()?;
    fields
        .iter()
        .find(|(key, _)| key.as_str() == Some(name))
        .map(|(_, value)| value)
}

/// `Refused` carries nvim's own message, which can quote a file name of any bytes.
pub fn parse_open(value: &Value) -> Result<Opened, String> {
    if let Some(refused) = field(value, "refused") {
        return match refused {
            Value::String(why) => Ok(Opened::Refused(String::from_utf8_lossy(why.as_bytes()).into_owned())),
            _ => Err("a refused open that says nothing".to_owned()),
        };
    }
    if field(value, "expired").and_then(Value::as_bool) == Some(true) {
        return Ok(Opened::Expired);
    }
    if field(value, "opened").and_then(Value::as_bool) == Some(true) {
        return Ok(Opened::Opened);
    }
    Err("an answer the open chunk does not give".to_owned())
}

pub fn send_close(rpc: &dyn EditorRpc, view: u64) -> Pending {
    rpc.exec_lua(IDE_DIFF_CLOSE, vec![Value::from(view)])
}

pub fn send_take(rpc: &dyn EditorRpc) -> Pending {
    rpc.exec_lua(IDE_TAKE_EVENTS, vec![])
}

/// What a person did in a view.
#[derive(Debug, Clone, PartialEq)]
pub enum ViewEvent {
    Accept { view: u64, lines: Vec<Value> },
    Reject { view: u64 },
    Closed { view: u64 },
}

/// One event. Takes the answer by value, for the same reason [`parse_events`] does.
fn parse_event(event: Value) -> Result<ViewEvent, String> {
    let Value::Map(fields) = event else {
        return Err("a view event that is not a table".to_owned());
    };
    let (mut view, mut kind, mut lines) = (None, None, None);
    for (name, value) in fields {
        match name.as_str() {
            Some("view") => view = value.as_u64(),
            Some("kind") => kind = value.as_str().map(str::to_owned),
            Some("lines") => lines = Some(value),
            _ => {}
        }
    }
    let view = view.ok_or("a view event that names no view")?;
    match kind.as_deref() {
        Some("accept") => match lines {
            Some(Value::Array(lines)) => Ok(ViewEvent::Accept { view, lines }),
            _ => Err("an accept with no lines".to_owned()),
        },
        Some("reject") => Ok(ViewEvent::Reject { view }),
        Some("closed") => Ok(ViewEvent::Closed { view }),
        other => Err(format!("a view event of an unknown kind: {other:?}")),
    }
}

/// Moves an accept's `lines` out of the answer, unconverted: the bytes are the judge's to join, and a large text of
/// short lines is not to be copied on the GTK thread. So this takes the answer itself, where [`parse_open`] borrows.
pub fn parse_events(value: Value) -> Result<Vec<ViewEvent>, String> {
    match value {
        Value::Array(events) => events.into_iter().map(parse_event).collect(),
        // nvim has one empty table, and it can arrive as either.
        Value::Map(fields) if fields.is_empty() => Ok(Vec::new()),
        _ => Err("view events that are not a list".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::path::PathBuf;

    use super::*;
    use crate::ide_server::lines::{line_values, split_lines, LineShape};

    /// Keeps what it was sent, and answers nothing.
    #[derive(Default)]
    struct Recorder(RefCell<Vec<(&'static str, Vec<Value>)>>);

    impl EditorRpc for Recorder {
        fn exec_lua(&self, code: &'static str, args: Vec<Value>) -> Pending {
            self.0.borrow_mut().push((code, args));
            Pending::pair().1
        }

        fn target(&self) -> Option<u64> {
            Some(1)
        }
    }

    fn stamp() -> Stamp {
        Stamp {
            deadline_ms: 1,
            scope: "ide-1".to_owned(),
            seq: 1,
        }
    }

    fn request() -> DiffRequest {
        let (file_lines, _) = split_lines(b"one\n");
        let (proposal_lines, shape) = split_lines(b"a line long enough to have its own allocation\n");
        DiffRequest {
            tab_name: "tab".to_owned(),
            real_path: PathBuf::from("/project/src/x.rs"),
            rel: "src/x.rs".to_owned(),
            file_text: b"one\n".to_vec(),
            file_lines: line_values(file_lines),
            proposal_lines: line_values(proposal_lines),
            shape,
        }
    }

    fn event(fields: &[(&str, Value)]) -> Value {
        Value::Map(
            fields
                .iter()
                .map(|(name, value)| (Value::from(*name), value.clone()))
                .collect(),
        )
    }

    #[test]
    fn the_names_use_the_schemes_the_feed_leaves_out() {
        let (file, proposal) = buffer_names(0x1234, "src/x.rs");
        assert_eq!(file, format!("{IDE_FILE_SCHEME}001234/src/x.rs"));
        assert_eq!(proposal, format!("{IDE_PROPOSAL_SCHEME}001234/src/x.rs"));
        let (file, _) = buffer_names(0xab_cdef_0123, "x");
        assert_eq!(
            file,
            format!("{IDE_FILE_SCHEME}ef0123/x"),
            "six hex digits, whatever the id"
        );
    }

    #[test]
    fn parse_events_refuses_an_unknown_kind() {
        let wrong = event(&[("view", Value::from(1)), ("kind", Value::from("approve"))]);
        let error = parse_events(Value::Array(vec![wrong])).unwrap_err();
        assert!(error.contains("approve"), "{error}");
        let good = event(&[("view", Value::from(1)), ("kind", Value::from("reject"))]);
        let wrong = event(&[("view", Value::from(2))]);
        assert!(
            parse_events(Value::Array(vec![good, wrong])).is_err(),
            "one bad event refuses the answer"
        );
    }

    #[test]
    fn parse_events_reads_each_kind_and_both_empty_tables() {
        let lines = Value::Array(vec![Value::Binary(b"a".to_vec())]);
        let answer = Value::Array(vec![
            event(&[
                ("view", Value::from(1)),
                ("kind", Value::from("accept")),
                ("lines", lines),
            ]),
            event(&[("view", Value::from(2)), ("kind", Value::from("reject"))]),
            event(&[("view", Value::from(3)), ("kind", Value::from("closed"))]),
        ]);
        assert_eq!(
            parse_events(answer),
            Ok(vec![
                ViewEvent::Accept {
                    view: 1,
                    lines: vec![Value::Binary(b"a".to_vec())]
                },
                ViewEvent::Reject { view: 2 },
                ViewEvent::Closed { view: 3 },
            ])
        );
        assert_eq!(parse_events(Value::Array(vec![])), Ok(vec![]));
        assert_eq!(parse_events(Value::Map(vec![])), Ok(vec![]));
        assert!(parse_events(Value::Nil).is_err());
        assert!(parse_events(Value::Map(vec![(Value::from("a"), Value::from(1))])).is_err());
    }

    #[test]
    fn an_accept_without_lines_is_refused() {
        let answer = Value::Array(vec![event(&[
            ("view", Value::from(1)),
            ("kind", Value::from("accept")),
        ])]);
        assert!(parse_events(answer).is_err());
    }

    #[test]
    fn parse_events_moves_the_lines_it_is_given() {
        let line = b"a line long enough to have its own allocation".to_vec();
        let held = line.as_ptr();
        let answer = Value::Array(vec![Value::Map(vec![
            (Value::from("view"), Value::from(1)),
            (Value::from("kind"), Value::from("accept")),
            (Value::from("lines"), Value::Array(vec![Value::Binary(line)])),
        ])]);
        let events = parse_events(answer).expect("an accept");
        let [ViewEvent::Accept { lines, .. }] = events.as_slice() else {
            panic!("one accept");
        };
        assert!(matches!(&lines[0], Value::Binary(bytes) if bytes.as_ptr() == held));
    }

    #[test]
    fn parse_open_reads_each_answer() {
        assert_eq!(parse_open(&event(&[("opened", Value::from(true))])), Ok(Opened::Opened));
        assert_eq!(
            parse_open(&event(&[("expired", Value::from(true))])),
            Ok(Opened::Expired)
        );
        assert_eq!(
            parse_open(&event(&[("refused", Value::from("a diff is open"))])),
            Ok(Opened::Refused("a diff is open".to_owned()))
        );
        assert!(parse_open(&Value::Nil).is_err());
        assert!(parse_open(&event(&[("opened", Value::from(false))])).is_err());
    }

    #[test]
    fn a_refusal_quoting_bytes_that_are_not_text_still_reads() {
        // A msgpack str of two bytes, as nvim sends any string, whatever it holds.
        let frame = [0x81, 0xa7, b'r', b'e', b'f', b'u', b's', b'e', b'd', 0xa2, b'a', 0xff];
        let answer = rmpv::decode::read_value(&mut frame.as_slice()).expect("a msgpack map");
        assert_eq!(parse_open(&answer), Ok(Opened::Refused("a\u{fffd}".to_owned())));
    }

    #[test]
    fn send_open_moves_the_lines_and_sends_text_as_bytes() {
        let request = request();
        let Value::Binary(line) = &request.proposal_lines[0] else {
            panic!("a line is bytes");
        };
        let held = line.as_ptr();
        let editor = Recorder::default();
        let _pending = send_open(&editor, &stamp(), 0x1234, request);
        let sent = editor.0.borrow();
        let (code, args) = &sent[0];
        assert_eq!(*code, IDE_DIFF_OPEN);
        assert_eq!(args[0], stamp().to_value());
        let lines = field(&args[1], "proposal_lines")
            .and_then(Value::as_array)
            .expect("lines");
        assert!(matches!(&lines[0], Value::Binary(bytes) if bytes.as_ptr() == held));
        for name in ["file_name", "proposal_name", "rel", "real", "file_text"] {
            assert!(matches!(field(&args[1], name), Some(Value::Binary(_))), "{name}");
        }
        assert_eq!(field(&args[1], "view").and_then(Value::as_u64), Some(0x1234));
        assert_eq!(field(&args[1], "crlf"), Some(&Value::from(false)));
        assert_eq!(field(&args[1], "eol"), Some(&Value::from(true)));
    }

    #[test]
    fn close_and_take_are_the_constant_chunks() {
        let editor = Recorder::default();
        let _pending = send_close(&editor, 7);
        let _pending = send_take(&editor);
        let sent = editor.0.borrow();
        assert_eq!(sent[0], (IDE_DIFF_CLOSE, vec![Value::from(7)]));
        assert_eq!(sent[1], (IDE_TAKE_EVENTS, vec![]));
    }

    #[test]
    fn the_shape_named_by_a_request_is_the_proposals() {
        let shape = LineShape { crlf: true, eol: false };
        let mut request = request();
        request.shape = shape;
        let editor = Recorder::default();
        let _pending = send_open(&editor, &stamp(), 1, request);
        let sent = editor.0.borrow();
        assert_eq!(field(&sent[0].1[1], "crlf"), Some(&Value::from(true)));
        assert_eq!(field(&sent[0].1[1], "eol"), Some(&Value::from(false)));
    }
}
