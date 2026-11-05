//! Compact JSON at the process boundary. Foreign strings receive the same display cut as local ones.

use super::row::{Round, Selection};
use super::{
    Change, ProcessFile, ProjectRef, Row, View, WindowRef, LABEL_MAX, MAX_FILE_BYTES, MAX_ROWS_PER_FILE, SCHEMA_V,
};
use serde::Serialize;

pub fn cut(s: &str, max: usize) -> String {
    crate::onboarding::display::display_text_cut(s, max)
}

fn text(s: &mut String) {
    *s = cut(s, LABEL_MAX);
}

fn project(p: &mut ProjectRef) {
    text(&mut p.key);
    text(&mut p.name);
}

fn window(w: &mut WindowRef) {
    text(&mut w.instance);
    text(&mut w.name);
}

pub(crate) fn recut_row(r: &mut Row) {
    text(&mut r.id);
    text(&mut r.key);
    text(&mut r.agent);
    project(&mut r.project);
    window(&mut r.window);
    text(&mut r.label);
    text(&mut r.state);
    if let Some(tool) = &mut r.tool {
        text(tool);
    }
    text(&mut r.mode);
}

fn selection(s: &mut Selection) {
    text(&mut s.key);
    text(&mut s.by);
}

fn round(r: &mut Round) {
    text(&mut r.from.instance);
    text(&mut r.from.module);
    if let Some(key) = &mut r.current {
        text(key);
    }
    for key in &mut r.visited {
        text(key);
    }
}

fn recut_view(v: &mut View) {
    for p in &mut v.projects {
        project(p);
    }
    for r in &mut v.rows {
        recut_row(r);
    }
    if let Some(s) = &mut v.selection {
        selection(s);
    }
    if let Some(r) = &mut v.round {
        round(r);
    }
}

#[derive(Serialize)]
struct VersionedView<'a> {
    v: u32,
    #[serde(flatten)]
    view: &'a View,
}

/// One JSON line without a newline. The caller chooses the supported schema it advertised.
pub fn view_json(v: &View, schema: u32) -> String {
    let mut view = v.clone();
    recut_view(&mut view);
    serde_json::to_string(&VersionedView { v: schema, view: &view }).expect("agent views contain only JSON values")
}

pub fn change_json(c: &Change, schema: u32) -> String {
    #[derive(Serialize)]
    struct VersionedChange<'a> {
        v: u32,
        #[serde(flatten)]
        change: Body<'a>,
    }
    #[derive(Serialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum Body<'a> {
        Row {
            row: &'a Row,
        },
        Gone {
            id: String,
            key: String,
            why: super::GoneWhy,
        },
        Reset {
            view: VersionedView<'a>,
        },
    }
    let mut c = c.clone();
    let change = match &mut c {
        Change::Row { row } => {
            recut_row(row);
            Body::Row { row }
        }
        Change::Gone { id, key, why } => Body::Gone {
            id: cut(id, LABEL_MAX),
            key: cut(key, LABEL_MAX),
            why: *why,
        },
        Change::Reset { view } => {
            recut_view(view);
            Body::Reset {
                view: VersionedView { v: schema, view },
            }
        }
    };
    serde_json::to_string(&VersionedChange { v: schema, change }).expect("agent changes contain only JSON values")
}

fn recut_file(f: &mut ProcessFile) {
    text(&mut f.instance);
    project(&mut f.project);
    window(&mut f.window);
    for row in &mut f.rows {
        recut_row(row);
    }
    for departed in &mut f.departed {
        text(&mut departed.id);
        text(&mut departed.key);
    }
}

pub fn process_file_json(f: &ProcessFile) -> Vec<u8> {
    let mut f = f.clone();
    recut_file(&mut f);
    serde_json::to_vec(&f).expect("agent process files contain only JSON values")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireError {
    Json(String),
    NewerMajor(u32),
    TooLarge,
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Json(why) => write!(f, "invalid agent process file: {why}"),
            Self::NewerMajor(v) => write!(f, "agent schema {v} is newer than {SCHEMA_V}"),
            Self::TooLarge => write!(f, "agent process file exceeds its size limit"),
        }
    }
}

impl std::error::Error for WireError {}

pub fn parse_process_file(bytes: &[u8]) -> Result<ProcessFile, WireError> {
    if bytes.len() > MAX_FILE_BYTES {
        return Err(WireError::TooLarge);
    }
    // Read the version before the rest: a newer schema may have changed every other field's type.
    #[derive(serde::Deserialize)]
    struct Version {
        #[serde(default)]
        v: u32,
    }
    let version: Version = serde_json::from_slice(bytes).map_err(|e| WireError::Json(e.to_string()))?;
    if version.v > SCHEMA_V {
        return Err(WireError::NewerMajor(version.v));
    }
    let mut f: ProcessFile = serde_json::from_slice(bytes).map_err(|e| WireError::Json(e.to_string()))?;
    // A window past the row limit is shown with its first rows and the note, not dropped.
    if f.rows.len() > MAX_ROWS_PER_FILE {
        f.rows.truncate(MAX_ROWS_PER_FILE);
        f.truncated = true;
    }
    recut_file(&mut f);
    Ok(f)
}
