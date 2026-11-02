//! A chooser's state and requests. The host supplies facts and performs every effect.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::onboarding::Lang;

#[cfg(test)]
mod tests;
mod text;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fact {
    Present,
    Open,
    Gone,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub path: PathBuf,
    pub name: String,
    pub shown: String,
    pub age: String,
    pub fact: Fact,
}

impl Row {
    pub fn new(path: PathBuf, home: Option<&Path>, age: String, fact: Fact) -> Row {
        let name = clean(&path.file_name().unwrap_or(path.as_os_str()).to_string_lossy());
        let shown = match home.and_then(|home| path.strip_prefix(home).ok()) {
            Some(rest) if rest.as_os_str().is_empty() => "~".into(),
            Some(rest) => format!("~/{}", rest.to_string_lossy()),
            None => path.to_string_lossy().into_owned(),
        };
        Row {
            path,
            name,
            shown: clean(&shown),
            age: clean(&age),
            fact,
        }
    }
}

fn clean(text: &str) -> String {
    text.chars().map(|c| if c.is_control() { '?' } else { c }).collect()
}

/// Format elapsed time supplied by a caller; future stamps read as zero minutes.
pub fn age_text(used: SystemTime, now: SystemTime) -> String {
    let seconds = now.duration_since(used).unwrap_or_default().as_secs();
    let (amount, unit) = match seconds {
        0..3600 => (seconds / 60, "m"),
        3600..86400 => (seconds / 3600, "h"),
        86400..604800 => (seconds / 86400, "d"),
        _ => (seconds / 604800, "w"),
    };
    format!("{amount} {unit}")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Window { root: PathBuf },
    Launch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind {
    Dir,
    File,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathAnswer {
    Found {
        path: PathBuf,
        exact: Option<PathKind>,
        folders: Vec<PathBuf>,
        common: String,
    },
    Missing,
    Stuck,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineEv {
    Changed(String),
    Enter,
    Escape,
    Step(i32),
    Tab,
    ChooseFolder,
    KeysLost,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ev {
    Rows { serial: u64, rows: Vec<Row> },
    PathAnswer { serial: u64, answer: PathAnswer },
    Line(LineEv),
    Pick(usize),
    Dismiss,
    Folder(Option<PathBuf>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    List { serial: u64 },
    Path { serial: u64, text: String, base: PathBuf },
    FolderDialog,
    SetLine(String),
    Open(PathBuf),
    Close,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowView {
    pub name: String,
    pub shown: String,
    pub right: String,
    pub dimmed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChooserView {
    pub title: String,
    pub line: String,
    pub placeholder: String,
    pub rows: Vec<RowView>,
    pub selected: Option<usize>,
    pub message: Option<String>,
    pub footer: String,
    pub opening: bool,
}

#[derive(Clone)]
enum Item {
    Project(Row),
    Folder,
    Home(PathBuf),
}

pub struct Chooser {
    mode: Mode,
    home: Option<PathBuf>,
    lang: Lang,
    generation: u32,
    counter: u32,
    list_serial: Option<u64>,
    path_serial: Option<u64>,
    rows: Vec<Row>,
    line: String,
    answer: Option<PathAnswer>,
    selected: Option<usize>,
    held_enter: bool,
    dialog: bool,
    opening: bool,
    closed: bool,
    message: Option<String>,
}

impl Chooser {
    pub fn new(mode: Mode, home: Option<PathBuf>, lang: Lang, generation: u32) -> Chooser {
        Chooser {
            mode,
            home,
            lang,
            generation,
            counter: 0,
            list_serial: None,
            path_serial: None,
            rows: Vec::new(),
            line: String::new(),
            answer: None,
            selected: Some(0),
            held_enter: false,
            dialog: false,
            opening: false,
            closed: false,
            message: None,
        }
    }

    fn serial(&mut self) -> u64 {
        self.counter = self.counter.checked_add(1).expect("chooser request counter exhausted");
        (u64::from(self.generation) << 32) | u64::from(self.counter)
    }

    pub fn shown(&mut self) -> Vec<Effect> {
        let serial = self.serial();
        self.list_serial = Some(serial);
        self.held_enter = false;
        vec![Effect::List { serial }]
    }

    pub fn event(&mut self, ev: Ev) -> Vec<Effect> {
        if self.opening || self.closed {
            return vec![];
        }
        // The native dialog owns input until it answers; worker replies may still update facts.
        if self.dialog && !matches!(ev, Ev::Rows { .. } | Ev::PathAnswer { .. } | Ev::Folder(_)) {
            return vec![];
        }
        match ev {
            Ev::Rows { serial, rows } if self.list_serial == Some(serial) => {
                self.list_serial = None;
                self.rows = rows
                    .into_iter()
                    .filter(|row| !matches!(&self.mode, Mode::Window { root } if root == &row.path))
                    .collect();
                if !is_path(&self.line) {
                    self.selected = Some(0);
                    if std::mem::take(&mut self.held_enter) && matches!(self.items().first(), Some(Item::Project(_))) {
                        return self.activate();
                    }
                }
                vec![]
            }
            Ev::PathAnswer { serial, answer } if self.path_serial == Some(serial) => {
                self.path_serial = None;
                self.answer = Some(answer);
                self.selected = Some(0);
                if std::mem::take(&mut self.held_enter)
                    && !matches!(&self.answer, Some(PathAnswer::Found { exact: None, folders, .. }) if folders.is_empty())
                {
                    return self.activate();
                }
                vec![]
            }
            Ev::Line(LineEv::Changed(text)) => self.changed(text),
            Ev::Line(LineEv::Enter) => {
                if (is_path(&self.line) && self.path_serial.is_some())
                    || (!is_path(&self.line) && self.list_serial.is_some())
                {
                    self.held_enter = true;
                    vec![]
                } else {
                    self.activate()
                }
            }
            Ev::Line(LineEv::Step(step)) => {
                self.held_enter = false;
                let last = self.items().len().saturating_sub(1);
                self.selected =
                    Some((self.selected.unwrap_or(0) as i64 + i64::from(step)).clamp(0, last as i64) as usize);
                vec![]
            }
            Ev::Line(LineEv::Tab) => self.complete(),
            Ev::Line(LineEv::ChooseFolder) => self.folder_dialog(),
            Ev::Pick(index) if index < self.items().len() => {
                self.held_enter = false;
                self.selected = Some(index);
                if matches!(self.items().get(index), Some(Item::Folder)) {
                    self.folder_dialog()
                } else {
                    self.activate()
                }
            }
            Ev::Dismiss | Ev::Line(LineEv::Escape | LineEv::KeysLost) => {
                self.closed = true;
                self.held_enter = false;
                vec![Effect::Close]
            }
            Ev::Folder(path) if self.dialog => {
                self.dialog = false;
                path.map(|path| vec![Effect::Open(path)]).unwrap_or_default()
            }
            _ => vec![],
        }
    }

    fn changed(&mut self, text: String) -> Vec<Effect> {
        if text == self.line {
            return vec![];
        }
        self.line = text;
        self.held_enter = false;
        self.answer = None;
        self.path_serial = None;
        self.message = None;
        self.selected = Some(0);
        if is_path(&self.line) {
            let serial = self.serial();
            self.path_serial = Some(serial);
            let base = match &self.mode {
                Mode::Window { root } => root.clone(),
                Mode::Launch => self.home.clone().unwrap_or_else(|| PathBuf::from("/")),
            };
            vec![Effect::Path {
                serial,
                text: self.line.clone(),
                base,
            }]
        } else {
            vec![]
        }
    }

    fn folder_dialog(&mut self) -> Vec<Effect> {
        self.held_enter = false;
        self.dialog = true;
        vec![Effect::FolderDialog]
    }

    fn activate(&mut self) -> Vec<Effect> {
        if is_path(&self.line) {
            match &self.answer {
                Some(PathAnswer::Missing) => {
                    self.message = Some(text::missing(self.lang, &clean(&self.line)));
                    return vec![];
                }
                Some(PathAnswer::Stuck) | None => {
                    self.message = Some(text::text(self.lang).stuck.into());
                    return vec![];
                }
                _ => {}
            }
        }
        match self.selected.and_then(|i| self.items().get(i).cloned()) {
            Some(Item::Project(row)) if row.fact == Fact::Gone => {
                self.message = Some(text::gone(self.lang, &row.shown));
                vec![]
            }
            Some(Item::Project(row)) => vec![Effect::Open(row.path)],
            Some(Item::Home(path)) => vec![Effect::Open(path)],
            Some(Item::Folder) => self.folder_dialog(),
            None => vec![],
        }
    }

    fn complete(&mut self) -> Vec<Effect> {
        let Some(PathAnswer::Found { folders, common, .. }) = &self.answer else {
            return vec![];
        };
        if common.is_empty() {
            return vec![];
        }
        let mut next = common.clone();
        if folders.len() == 1 && !next.ends_with('/') {
            next.push('/');
        }
        let mut effects = self.changed(next.clone());
        effects.insert(0, Effect::SetLine(next));
        effects
    }

    fn items(&self) -> Vec<Item> {
        let mut items = Vec::new();
        if is_path(&self.line) {
            if let Some(PathAnswer::Found {
                path, exact, folders, ..
            }) = &self.answer
            {
                if exact.is_some() {
                    items.push(Item::Project(Row::new(
                        path.clone(),
                        self.home.as_deref(),
                        String::new(),
                        Fact::Present,
                    )));
                }
                for folder in folders {
                    if exact.is_none() || folder != path {
                        items.push(Item::Project(Row::new(
                            folder.clone(),
                            self.home.as_deref(),
                            String::new(),
                            Fact::Present,
                        )));
                    }
                }
            }
        } else {
            let needle = self.line.trim().to_lowercase();
            items.extend(
                self.rows
                    .iter()
                    .filter(|r| r.name.to_lowercase().contains(&needle) || r.shown.to_lowercase().contains(&needle))
                    .cloned()
                    .map(Item::Project),
            );
            let first_run = self.rows.is_empty() && matches!(self.mode, Mode::Launch) && needle.is_empty();
            if first_run {
                items.push(Item::Folder);
            }
            if matches!(self.mode, Mode::Launch) && needle.is_empty() {
                if let Some(home) = &self.home {
                    if !self.rows.iter().any(|r| &r.path == home) {
                        items.push(Item::Home(home.clone()));
                    }
                }
            }
            if first_run {
                return items;
            }
        }
        items.push(Item::Folder);
        items
    }

    pub fn view(&self) -> ChooserView {
        let text = text::text(self.lang);
        let rows: Vec<_> = self
            .items()
            .into_iter()
            .map(|item| match item {
                Item::Project(row) => RowView {
                    name: clean(&row.name),
                    shown: clean(&row.shown),
                    right: match row.fact {
                        Fact::Open => text.open.into(),
                        Fact::Gone => text.gone.into(),
                        _ => clean(&row.age),
                    },
                    dimmed: row.fact == Fact::Gone,
                },
                Item::Folder => RowView {
                    name: text.folder.into(),
                    shown: String::new(),
                    right: String::new(),
                    dimmed: false,
                },
                Item::Home(path) => RowView {
                    name: text.home.into(),
                    shown: Row::new(path, self.home.as_deref(), String::new(), Fact::Present).shown,
                    right: String::new(),
                    dimmed: false,
                },
            })
            .collect();
        ChooserView {
            title: text.title.into(),
            line: self.line.clone(),
            placeholder: text.placeholder.into(),
            selected: self.selected.filter(|i| *i < rows.len()),
            rows,
            message: self.message.clone().or_else(|| {
                (self.list_serial.is_none() && self.rows.is_empty() && self.line.is_empty()).then(|| text.empty.into())
            }),
            footer: text.footer.into(),
            opening: self.opening,
        }
    }

    pub fn opening(&mut self, name: &str) {
        self.opening = true;
        self.held_enter = false;
        self.message = Some(text::opening(self.lang, &clean(name)));
    }

    pub fn failed(&mut self, message: String) {
        self.opening = false;
        self.held_enter = false;
        self.message = Some(clean(&message));
    }
}

pub fn is_path(text: &str) -> bool {
    ["/", "~", "./", "../"].iter().any(|prefix| text.starts_with(prefix))
}
