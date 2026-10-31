//! The editor operations window 2 may ask window 1 to carry out. A closed set, each with typed fields: window 1
//! chooses the Lua constant for each and builds every argument itself after its own checks, so nothing window 2
//! sends can become code in the editor. Adding an operation means adding it here and to the dispatcher's table.

use serde::{Deserialize, Serialize};

use crate::editor_lines::{BufferLine, Eol};
use crate::editor_tools::stamp::Stamp;
use crate::editor_tools::tools::diagnostics::Severity;
use crate::editor_tools::tools::lsp::{LspKind, UsagePart};
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
    /// A plugin page asks the editor to open `path` relative to its declared `root`, named by the manifest.
    PluginOpen {
        plugin: String,
        root: String,
        path: String,
        line: Option<u32>,
        col: Option<u32>,
    },
    PluginAssistApply {
        plugin: String,
        diagnostics: bool,
        inlay_hints: bool,
    },
    /// The follow snippet installed for a page window 2 holds, from window 1's own registration of that plugin
    /// (its socket, filetypes and commands); an install already in place is made again with the same values. There
    /// is no teardown op: the snippet and its commands are window 1's, and a page ending in window 2 only turns its
    /// following off ([`EditorOp::PluginFollow`]).
    PluginInstall {
        plugin: String,
    },
    PluginFollow {
        plugin: String,
        on: bool,
    },
    PluginText {
        plugin: String,
        buf: u64,
    },
    PluginReveal {
        plugin: String,
        buf: u64,
        line: u32,
        token: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cursor: Option<u32>,
    },
    PluginJump {
        plugin: String,
        buf: u64,
        line: u32,
        col: u32,
    },
    /// An already resolved Markdown link, checked again against window 1's project before opening.
    PluginOpenLink {
        plugin: String,
        path: String,
        line: Option<u32>,
    },
    PluginAssistClear {
        plugin: String,
    },
    /// Show a tab's file at a line. `path` is relative to the project root. `load` is the answer of window 2's own
    /// gate: the file may be read in if it is not open.
    AgentOpen {
        session: String,
        path: String,
        line: Option<u32>,
        end_line: Option<u32>,
        load: bool,
        stamp: WireStamp,
    },
    /// What the editor holds, as `editor_state` tells it, under window 1's own root.
    AgentEditorState {
        session: String,
        stamp: WireStamp,
    },
    /// The lines of a loaded buffer. `path` is relative to the project root.
    AgentReadBuffer {
        session: String,
        path: String,
        start: Option<u32>,
        end: Option<u32>,
        stamp: WireStamp,
    },
    /// The editor's diagnostics, of one file or of the project. `min_severity` is [`severity_to_wire`]'s number.
    AgentDiagnostics {
        session: String,
        path: Option<String>,
        min_severity: u8,
        limit: u32,
        stamp: WireStamp,
    },
    /// Starts an ask of the language servers, collected under `id`. `path` is relative to the project root and left
    /// out only by a workspace search. `load` is the answer of window 2's own gate, as on [`EditorOp::AgentOpen`].
    AgentLspAsk {
        session: String,
        id: String,
        kind: WireLspKind,
        path: Option<String>,
        line: Option<u32>,
        symbol: Option<String>,
        include: Vec<WireUsagePart>,
        query: Option<String>,
        budget_ms: u32,
        load: bool,
        stamp: WireStamp,
    },
    /// What has arrived for an ask. With `last`, what is there ends it.
    AgentLspCollect {
        session: String,
        id: String,
        last: bool,
        stamp: WireStamp,
    },
    /// The project's diagnostics as a turn begins. Window 1 picks how many, and keeps nothing: the entries are
    /// answered to window 2, whose broker compares a later reading with them.
    AgentBaseline {
        session: String,
        stamp: WireStamp,
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
            EditorOp::PluginInstall { .. } => "plugin_install",
            EditorOp::PluginFollow { .. } => "plugin_follow",
            EditorOp::PluginText { .. } => "plugin_text",
            EditorOp::PluginReveal { .. } => "plugin_reveal",
            EditorOp::PluginJump { .. } => "plugin_jump",
            EditorOp::PluginOpenLink { .. } => "plugin_open_link",
            EditorOp::PluginAssistClear { .. } => "plugin_assist_clear",
            EditorOp::AgentOpen { .. } => "agent_open",
            EditorOp::AgentEditorState { .. } => "agent_editor_state",
            EditorOp::AgentReadBuffer { .. } => "agent_read_buffer",
            EditorOp::AgentDiagnostics { .. } => "agent_diagnostics",
            EditorOp::AgentLspAsk { .. } => "agent_lsp_ask",
            EditorOp::AgentLspCollect { .. } => "agent_lsp_collect",
            EditorOp::AgentBaseline { .. } => "agent_baseline",
        }
    }
}

/// A chunk's stamp as window 2 made it. Window 1 holds it to its own limits before any chunk carries it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireStamp {
    pub deadline_ms: u64,
    pub scope: String,
    pub seq: u64,
}

impl From<&Stamp> for WireStamp {
    fn from(stamp: &Stamp) -> Self {
        WireStamp {
            deadline_ms: stamp.deadline_ms,
            scope: stamp.scope.clone(),
            seq: stamp.seq,
        }
    }
}

impl From<WireStamp> for Stamp {
    fn from(stamp: WireStamp) -> Self {
        Stamp {
            deadline_ms: stamp.deadline_ms,
            scope: stamp.scope,
            seq: stamp.seq,
        }
    }
}

/// The asks that cross the link: reads, each with an answer window 2 tells. An ask that would end in an edit has no
/// name here, so no proposal can be asked for across the link. A set of its own rather than [`LspKind`], which a new
/// kind would widen without anyone deciding that it crosses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireLspKind {
    Usages,
    Hover,
    DocumentSymbols,
    WorkspaceSymbols,
}

impl From<WireLspKind> for LspKind {
    fn from(kind: WireLspKind) -> Self {
        match kind {
            WireLspKind::Usages => LspKind::Usages,
            WireLspKind::Hover => LspKind::Hover,
            WireLspKind::DocumentSymbols => LspKind::DocumentSymbols,
            WireLspKind::WorkspaceSymbols => LspKind::WorkspaceSymbols,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireUsagePart {
    Definitions,
    References,
    Implementations,
}

impl From<UsagePart> for WireUsagePart {
    fn from(part: UsagePart) -> Self {
        match part {
            UsagePart::Definitions => WireUsagePart::Definitions,
            UsagePart::References => WireUsagePart::References,
            UsagePart::Implementations => WireUsagePart::Implementations,
        }
    }
}

impl From<WireUsagePart> for UsagePart {
    fn from(part: WireUsagePart) -> Self {
        match part {
            WireUsagePart::Definitions => UsagePart::Definitions,
            WireUsagePart::References => UsagePart::References,
            WireUsagePart::Implementations => UsagePart::Implementations,
        }
    }
}

/// A severity as it travels: nvim's own numbers, 1 for an error to 4 for a hint.
pub fn severity_to_wire(severity: Severity) -> u8 {
    match severity {
        Severity::Error => 1,
        Severity::Warning => 2,
        Severity::Info => 3,
        Severity::Hint => 4,
    }
}

/// `None` for a number no severity has.
pub fn severity_from_wire(number: u8) -> Option<Severity> {
    match number {
        1 => Some(Severity::Error),
        2 => Some(Severity::Warning),
        3 => Some(Severity::Info),
        4 => Some(Severity::Hint),
        _ => None,
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

    fn stamp() -> WireStamp {
        WireStamp {
            deadline_ms: 1_700_000_000_000,
            scope: "abc-3-1".into(),
            seq: 2,
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
                plugin: "demo".into(),
                root: "work".into(),
                path: "ch01/main.rs".into(),
                line: Some(1),
                col: None,
            },
            EditorOp::PluginAssistApply {
                plugin: "demo".into(),
                diagnostics: true,
                inlay_hints: false,
            },
            EditorOp::PluginAssistClear { plugin: "demo".into() },
            EditorOp::AgentOpen {
                session: "s-1".into(),
                path: "src/a.rs".into(),
                line: Some(3),
                end_line: None,
                load: true,
                stamp: stamp(),
            },
            EditorOp::AgentEditorState {
                session: "s-1".into(),
                stamp: stamp(),
            },
            EditorOp::AgentReadBuffer {
                session: "s-1".into(),
                path: "src/a.rs".into(),
                start: None,
                end: Some(9),
                stamp: stamp(),
            },
            EditorOp::AgentDiagnostics {
                session: "s-1".into(),
                path: None,
                min_severity: 2,
                limit: 500,
                stamp: stamp(),
            },
            EditorOp::AgentLspAsk {
                session: "s-1".into(),
                id: "abc-1-1".into(),
                kind: WireLspKind::Usages,
                path: Some("src/a.rs".into()),
                line: Some(3),
                symbol: Some("frob".into()),
                include: vec![WireUsagePart::Definitions, WireUsagePart::Implementations],
                query: None,
                budget_ms: 15_000,
                load: true,
                stamp: stamp(),
            },
            EditorOp::AgentLspAsk {
                session: "s-1".into(),
                id: "abc-1-2".into(),
                kind: WireLspKind::WorkspaceSymbols,
                path: None,
                line: None,
                symbol: None,
                include: vec![],
                query: Some("frob".into()),
                budget_ms: 15_000,
                load: false,
                stamp: stamp(),
            },
            EditorOp::AgentLspCollect {
                session: "s-1".into(),
                id: "abc-1-1".into(),
                last: true,
                stamp: stamp(),
            },
            EditorOp::AgentBaseline {
                session: "s-1".into(),
                stamp: stamp(),
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
            r#"{"op":"plugin_open","plugin":"demo","path":"a.rs","line":null,"col":null}"#,
            r#"{"op":"agent_open","session":"s","path":"a","line":null,"end_line":null,"load":false,"stamp":{"deadline_ms":1,"scope":"a","seq":1},"lua":"x"}"#,
            r#"{"op":"agent_editor_state","session":"s","stamp":{"deadline_ms":1,"scope":"a","seq":1,"lua":"x"}}"#,
        ] {
            assert!(serde_json::from_str::<EditorOp>(text).is_err(), "{text}");
        }
    }

    const STAMP: &str = r#""stamp":{"deadline_ms":1,"scope":"a","seq":1}"#;

    fn lsp_ask_text(kind: &str, extra: &str) -> String {
        format!(
            r#"{{"op":"agent_lsp_ask","session":"s","id":"a-1","kind":"{kind}","path":"a","line":1,"symbol":"x","include":["references"],"query":null,"budget_ms":1,"load":false,{STAMP}{extra}}}"#
        )
    }

    #[test]
    fn a_kind_that_would_edit_does_not_parse() {
        assert!(serde_json::from_str::<EditorOp>(&lsp_ask_text("hover", "")).is_ok());
        for kind in ["rename", "code_actions", "action_edit", "format", "targets"] {
            let text = lsp_ask_text(kind, "");
            assert!(serde_json::from_str::<EditorOp>(&text).is_err(), "{text}");
        }
    }

    #[test]
    fn an_extra_field_is_refused() {
        for text in [
            lsp_ask_text("hover", r#","lua":"x""#),
            lsp_ask_text("hover", r#","root":"/elsewhere""#),
            format!(r#"{{"op":"agent_lsp_collect","session":"s","id":"a-1","last":false,{STAMP},"lua":"x"}}"#),
            format!(r#"{{"op":"agent_baseline","session":"s",{STAMP},"root":"/elsewhere"}}"#),
            format!(r#"{{"op":"agent_baseline","session":"s",{STAMP},"limit":9}}"#),
        ] {
            assert!(serde_json::from_str::<EditorOp>(&text).is_err(), "{text}");
        }
    }

    #[test]
    fn a_usage_part_outside_the_three_does_not_parse() {
        let text = lsp_ask_text("usages", "").replace("references", "callers");
        assert!(serde_json::from_str::<EditorOp>(&text).is_err(), "{text}");
    }

    #[test]
    fn an_agent_op_with_no_stamp_is_refused() {
        for text in [
            r#"{"op":"agent_open","session":"s","path":"a","line":null,"end_line":null,"load":false}"#,
            r#"{"op":"agent_editor_state","session":"s"}"#,
            r#"{"op":"agent_read_buffer","session":"s","path":"a","start":null,"end":null}"#,
            r#"{"op":"agent_diagnostics","session":"s","path":null,"min_severity":1,"limit":1}"#,
            r#"{"op":"agent_lsp_ask","session":"s","id":"a","kind":"hover","path":"a","line":1,"symbol":"x","include":[],"query":null,"budget_ms":1,"load":false}"#,
            r#"{"op":"agent_lsp_collect","session":"s","id":"a","last":false}"#,
            r#"{"op":"agent_baseline","session":"s"}"#,
        ] {
            assert!(serde_json::from_str::<EditorOp>(text).is_err(), "{text}");
        }
    }

    #[test]
    fn a_stamp_and_a_severity_cross_unchanged() {
        assert_eq!(WireStamp::from(&Stamp::from(stamp())), stamp());
        for severity in [Severity::Error, Severity::Warning, Severity::Info, Severity::Hint] {
            assert_eq!(severity_from_wire(severity_to_wire(severity)), Some(severity));
        }
        for number in [0, 5, u8::MAX] {
            assert_eq!(severity_from_wire(number), None, "{number}");
        }
    }

    #[test]
    fn window_two_has_no_editor_buffers_or_cd_operation() {
        for op in [
            "plugin_preload",
            "plugin_modified",
            "plugin_reload",
            "plugin_diff",
            "plugin_cd",
        ] {
            let value = serde_json::json!({"op": op, "plugin": "demo"});
            assert!(serde_json::from_value::<EditorOp>(value).is_err(), "{op}");
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
