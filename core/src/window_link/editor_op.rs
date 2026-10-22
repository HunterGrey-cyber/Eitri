//! The editor operations window 2 may ask window 1 to carry out. A closed set, each with typed fields: window 1
//! chooses the Lua constant for each and builds every argument itself after its own checks, so nothing window 2
//! sends can become code in the editor. Adding an operation means adding it here and to the dispatcher's table.

use serde::{Deserialize, Serialize};

use crate::editor_lines::{BufferLine, Eol};
use crate::review_editor::{ShowHunk, ShowMeta};
use crate::turn_review::Scope;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum EditorOp {
    /// Draw a turn's hunks over a file the editor has loaded. `path` is relative to the project root.
    ReviewShow {
        path: String,
        meta: WireMeta,
        hunks: Vec<WireHunk>,
    },
    /// Open the file at `line`, then draw.
    ReviewOpenAndShow {
        path: String,
        line: Option<u32>,
        meta: WireMeta,
        hunks: Option<Vec<WireHunk>>,
    },
    /// Clear one file window 2 drew.
    ReviewClear {
        path: String,
    },
    /// Clear every file window 2 drew, and nothing window 1 drew. A struct variant with no fields, not a unit
    /// variant: serde ignores extra fields on an internally tagged unit variant, and an op must refuse them.
    ReviewClearMine {},
    /// Whether a file (or a range of its lines, start and count) has unsaved changes in the editor.
    BufferState {
        path: String,
        lines: Option<(u32, u32)>,
    },
    /// A plugin page asks the editor to open a file of the plugin's work tree.
    PluginOpen {
        plugin: String,
        path: String,
        line: Option<u32>,
        col: Option<u32>,
    },
    PluginAssistApply {
        plugin: String,
        diagnostics: bool,
        inlay_hints: bool,
    },
    PluginAssistClear {
        plugin: String,
    },
}

impl EditorOp {
    /// The name the dispatcher's table and its log lines use.
    pub fn name(&self) -> &'static str {
        match self {
            EditorOp::ReviewShow { .. } => "review_show",
            EditorOp::ReviewOpenAndShow { .. } => "review_open_and_show",
            EditorOp::ReviewClear { .. } => "review_clear",
            EditorOp::ReviewClearMine {} => "review_clear_mine",
            EditorOp::BufferState { .. } => "buffer_state",
            EditorOp::PluginOpen { .. } => "plugin_open",
            EditorOp::PluginAssistApply { .. } => "plugin_assist_apply",
            EditorOp::PluginAssistClear { .. } => "plugin_assist_clear",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireScope {
    Turn,
    Session,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireMeta {
    pub tab: u64,
    pub session: String,
    pub turn: u32,
    pub scope: WireScope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireEol {
    Lf,
    CrLf,
    Missing,
}

/// A buffer line with its bytes as they are (invalid UTF-8 included) and how it ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireLine {
    pub text: Vec<u8>,
    pub eol: WireEol,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireHunk {
    pub id: u32,
    pub old_start: u32,
    pub old_len: u32,
    pub new_start: u32,
    pub new_len: u32,
    pub old_lines: Vec<WireLine>,
    pub new_lines: Vec<WireLine>,
}

impl From<&ShowMeta> for WireMeta {
    fn from(meta: &ShowMeta) -> Self {
        WireMeta {
            tab: meta.tab,
            session: meta.session.clone(),
            turn: meta.turn,
            scope: match meta.scope {
                Scope::Turn => WireScope::Turn,
                Scope::Session => WireScope::Session,
            },
        }
    }
}

impl From<WireMeta> for ShowMeta {
    fn from(meta: WireMeta) -> Self {
        ShowMeta {
            tab: meta.tab,
            session: meta.session,
            turn: meta.turn,
            scope: match meta.scope {
                WireScope::Turn => Scope::Turn,
                WireScope::Session => Scope::Session,
            },
        }
    }
}

impl From<&BufferLine> for WireLine {
    fn from(line: &BufferLine) -> Self {
        WireLine {
            text: line.text.clone(),
            eol: match line.eol {
                Eol::Lf => WireEol::Lf,
                Eol::CrLf => WireEol::CrLf,
                Eol::Missing => WireEol::Missing,
            },
        }
    }
}

impl From<WireLine> for BufferLine {
    fn from(line: WireLine) -> Self {
        BufferLine {
            text: line.text,
            eol: match line.eol {
                WireEol::Lf => Eol::Lf,
                WireEol::CrLf => Eol::CrLf,
                WireEol::Missing => Eol::Missing,
            },
        }
    }
}

impl From<&ShowHunk> for WireHunk {
    fn from(h: &ShowHunk) -> Self {
        WireHunk {
            id: h.id,
            old_start: h.old_start,
            old_len: h.old_len,
            new_start: h.new_start,
            new_len: h.new_len,
            old_lines: h.old_lines.iter().map(WireLine::from).collect(),
            new_lines: h.new_lines.iter().map(WireLine::from).collect(),
        }
    }
}

impl From<WireHunk> for ShowHunk {
    fn from(h: WireHunk) -> Self {
        ShowHunk {
            id: h.id,
            old_start: h.old_start,
            old_len: h.old_len,
            new_start: h.new_start,
            new_len: h.new_len,
            old_lines: h.old_lines.into_iter().map(BufferLine::from).collect(),
            new_lines: h.new_lines.into_iter().map(BufferLine::from).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> WireMeta {
        WireMeta {
            tab: 4,
            session: "s-1".into(),
            turn: 2,
            scope: WireScope::Turn,
        }
    }

    fn hunk() -> WireHunk {
        WireHunk {
            id: 1,
            old_start: 3,
            old_len: 1,
            new_start: 3,
            new_len: 2,
            old_lines: vec![WireLine {
                text: b"a\xff".to_vec(),
                eol: WireEol::CrLf,
            }],
            new_lines: vec![
                WireLine {
                    text: b"b".to_vec(),
                    eol: WireEol::Lf,
                },
                WireLine {
                    text: vec![],
                    eol: WireEol::Missing,
                },
            ],
        }
    }

    #[test]
    fn every_op_round_trips_through_json_under_its_name() {
        let ops = vec![
            EditorOp::ReviewShow {
                path: "src/a.rs".into(),
                meta: meta(),
                hunks: vec![hunk()],
            },
            EditorOp::ReviewOpenAndShow {
                path: "src/a.rs".into(),
                line: Some(3),
                meta: meta(),
                hunks: None,
            },
            EditorOp::ReviewClear {
                path: "src/a.rs".into(),
            },
            EditorOp::ReviewClearMine {},
            EditorOp::BufferState {
                path: "src/a.rs".into(),
                lines: Some((3, 2)),
            },
            EditorOp::PluginOpen {
                plugin: "snotra".into(),
                path: "ch01/main.rs".into(),
                line: Some(1),
                col: None,
            },
            EditorOp::PluginAssistApply {
                plugin: "snotra".into(),
                diagnostics: true,
                inlay_hints: false,
            },
            EditorOp::PluginAssistClear {
                plugin: "snotra".into(),
            },
        ];
        for op in ops {
            let json = serde_json::to_value(&op).unwrap();
            assert_eq!(json["op"], op.name(), "{json}");
            let back: EditorOp = serde_json::from_value(json).unwrap();
            assert_eq!(back, op);
        }
    }

    #[test]
    fn an_op_outside_the_set_or_with_an_extra_field_is_refused() {
        for text in [
            r#"{"op":"exec_lua","code":"os.exit()"}"#,
            r#"{"op":"review_clear","path":"a","code":"x"}"#,
            r#"{"op":"review_clear_mine","lua":"x"}"#,
            r#"{"path":"a"}"#,
        ] {
            assert!(serde_json::from_str::<EditorOp>(text).is_err(), "{text}");
        }
    }

    #[test]
    fn review_meta_and_hunks_convert_both_ways_unchanged() {
        let m: ShowMeta = meta().into();
        assert_eq!(WireMeta::from(&m), meta());
        let h: ShowHunk = hunk().into();
        assert_eq!(WireHunk::from(&h), hunk());
        let session = WireMeta {
            scope: WireScope::Session,
            ..meta()
        };
        let m: ShowMeta = session.clone().into();
        assert_eq!(m.scope, Scope::Session);
        assert_eq!(WireMeta::from(&m), session);
    }
}
