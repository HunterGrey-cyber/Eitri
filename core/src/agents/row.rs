use super::{Class, EndedHow, SCHEMA_V};
use serde::{Deserialize, Deserializer, Serialize};

/// A value this reader has no word for reads as the type's default instead of failing the whole file: a newer
/// writer of the same schema may add a word to a closed set, and only the frozen `class` has to be understood.
fn lenient<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: serde::de::DeserializeOwned + Default,
{
    let value = serde_json::Value::deserialize(d)?;
    Ok(T::deserialize(value).unwrap_or_default())
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProjectRef {
    pub key: String,
    pub name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowRef {
    pub instance: String,
    pub name: String,
    pub focused: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LabelFrom {
    Rename,
    Title,
    Session,
    #[default]
    New,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Needs {
    pub cards: usize,
    pub questions: usize,
    pub trust: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Background {
    pub calls: u32,
    pub shells: u32,
    pub monitors: u32,
    pub agents: u32,
    pub other: u32,
}

/// Fields stay in wire order. State words remain strings so a newer word retains its frozen class.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Row {
    pub id: String,
    pub key: String,
    pub agent: String,
    pub project: ProjectRef,
    pub window: WindowRef,
    #[serde(deserialize_with = "lenient")]
    pub number: u16,
    pub label: String,
    #[serde(deserialize_with = "lenient")]
    pub label_from: LabelFrom,
    pub shown: bool,
    pub state: String,
    pub class: Class,
    pub since: u64,
    pub needs: Needs,
    pub waiting_since: Option<u64>,
    pub unread: bool,
    pub tool: Option<String>,
    pub background: Background,
    pub queued: usize,
    pub mode: String,
    #[serde(deserialize_with = "lenient")]
    pub ended_how: Option<EndedHow>,
    pub moving: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Selection {
    pub key: String,
    pub by: String,
    #[serde(rename = "at")]
    pub at_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RoundPoint {
    pub instance: String,
    pub module: String,
    pub tab: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Round {
    pub from: RoundPoint,
    pub current: Option<String>,
    pub visited: Vec<String>,
    #[serde(rename = "started_at")]
    pub started_at_ms: u64,
    #[serde(rename = "at")]
    pub at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct View {
    pub at: u64,
    pub enabled: bool,
    pub projects: Vec<ProjectRef>,
    pub rows: Vec<Row>,
    pub selection: Option<Selection>,
    pub round: Option<Round>,
}

impl Default for View {
    fn default() -> Self {
        Self {
            at: 0,
            enabled: crate::agent_prefs::AGENTS_ENABLED_DEFAULT,
            projects: Vec::new(),
            rows: Vec::new(),
            selection: None,
            round: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoneWhy {
    #[default]
    Closed,
    Moved,
    Window,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Change {
    Row {
        row: Row,
    },
    Gone {
        id: String,
        key: String,
        why: GoneWhy,
    },
    Reset {
        view: View,
    },
    /// The shared selection was written or removed.
    Selection {
        selection: Option<Selection>,
    },
    /// The shared round started, moved or ended.
    Round {
        round: Option<Round>,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Departed {
    pub id: String,
    pub key: String,
    #[serde(deserialize_with = "lenient")]
    pub why: GoneWhy,
    pub at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProcessFile {
    pub v: u32,
    pub instance: String,
    pub pid: u32,
    pub pid_start: u64,
    pub project: ProjectRef,
    pub window: WindowRef,
    pub rows: Vec<Row>,
    pub departed: Vec<Departed>,
    pub truncated: bool,
}

impl Default for ProcessFile {
    fn default() -> Self {
        Self {
            v: SCHEMA_V,
            instance: String::new(),
            pid: 0,
            pid_start: 0,
            project: ProjectRef::default(),
            window: WindowRef::default(),
            rows: Vec::new(),
            departed: Vec::new(),
            truncated: false,
        }
    }
}
