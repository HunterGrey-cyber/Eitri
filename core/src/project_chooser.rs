//! A chooser's state and requests. The host supplies facts and performs every effect.

use std::collections::{BTreeSet, VecDeque};
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use crate::onboarding::{
    display::{display_name, is_hidden},
    Lang,
};

pub mod listing;
pub use listing::*;
pub mod path;

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
        let name = display_name(&path.file_name().unwrap_or(path.as_os_str()).to_string_lossy());
        let shown = match home.and_then(|home| path.strip_prefix(home).ok()) {
            Some(rest) if rest.as_os_str().is_empty() => "~".into(),
            Some(rest) => format!("~/{}", rest.to_string_lossy()),
            None => path.to_string_lossy().into_owned(),
        };
        Row {
            path,
            name,
            shown: display_name(&shown),
            age: display_name(&age),
            fact,
        }
    }
}

/// The log line of a start that failed: the child's exit code, or `unknown` when there is none (the spawn failed, or a
/// signal ended it). It carries no path, as no log line does.
pub fn start_failed_line(code: Option<i32>) -> String {
    match code {
        Some(code) => format!("[project] start failed {code}"),
        None => "[project] start failed unknown".into(),
    }
}

/// What a window says of a project it could not start. A folder that was not there when the start ended is said as a
/// `gone` row is. Any other reason is in the log the started window keeps for its own project, since what it printed
/// went nowhere else.
pub fn start_failed_text(lang: Lang, row: &Row, gone: bool) -> String {
    if gone {
        text::gone(lang, &display_name(&row.shown))
    } else {
        format!("could not open {}: see its window log", display_name(&row.name))
    }
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
    Side(i32),
    Up,
    DeleteSegment,
    Tab,
    ChooseFolder,
    KeysLost,
    /// A plain printable key pressed while the list has the keys (the entry is not taking text).
    Key(char),
    /// Any other key (but a bare modifier) pressed while the list has the keys: it does nothing and ends a waiting `g`.
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ev {
    Rows {
        serial: u64,
        rows: Vec<Row>,
    },
    PathAnswer {
        serial: u64,
        answer: PathAnswer,
    },
    Line(LineEv),
    Listing {
        serial: u64,
        /// The walked folder named by the request, independent of the physical folder in the listing.
        dir: PathBuf,
        listing: Listing,
    },
    Later {
        serial: u64,
        git: Vec<OsString>,
        links: Vec<(OsString, LinkKind)>,
    },
    Slow {
        serial: u64,
    },
    Stuck {
        serial: u64,
    },
    Freed {
        mount: PathBuf,
    },
    Pick {
        index: usize,
        rows_rev: u64,
    },
    PickTwice {
        index: usize,
        rows_rev: u64,
    },
    Dismiss,
    Folder(Option<PathBuf>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    List {
        serial: u64,
    },
    Path {
        serial: u64,
        text: String,
        base: PathBuf,
    },
    Read {
        serial: u64,
        /// The walked folder; the listing's own path is physical.
        dir: PathBuf,
        filter: Option<String>,
        debounce: bool,
        near: Option<OsString>,
    },
    FolderDialog {
        at: Option<PathBuf>,
    },
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
    pub kind: RowKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChooserView {
    pub title: String,
    pub line: String,
    pub placeholder: String,
    pub rows: Arc<[RowView]>,
    pub rows_rev: u64,
    pub browsing: bool,
    pub hidden: bool,
    pub selected: Option<usize>,
    pub message: Option<String>,
    pub footer: String,
    pub opening: bool,
    /// The line has the keys (typing filters or types a path); otherwise the list does.
    pub in_line: bool,
}

#[derive(Clone, PartialEq)]
enum Item {
    Project(Row),
    Browse,
    Home(PathBuf),
    Here,
    Entry(Entry),
}

enum Place {
    Projects,
    Folder(Box<Browse>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Shown {
    Waiting,
    Slow,
    Stuck,
    Done,
}

struct Browse {
    /// Walked path. Display text and the resolver's physical path never replace it.
    dir: PathBuf,
    typed: String,
    filter: String,
    shown: Shown,
    listing: Option<Listing>,
    serial: Option<u64>,
    later_serial: Option<u64>,
    requested_filter: Option<String>,
    needs_filter: bool,
    git: BTreeSet<OsString>,
    left: Option<OsString>,
    held_enters: u8,
}

#[derive(Clone)]
struct FolderShown {
    dir: PathBuf,
    typed: String,
    listing: Listing,
}

pub struct Chooser {
    mode: Mode,
    home: Option<PathBuf>,
    lang: Lang,
    generation: u32,
    counter: u32,
    list_serial: Option<u64>,
    rows: Vec<Row>,
    rows_rev: u64,
    items: Vec<Item>,
    view_rows: Arc<[RowView]>,
    rows_key: Option<PathBuf>,
    line: String,
    place: Place,
    last_shown: Option<FolderShown>,
    recent: VecDeque<(PathBuf, Listing)>,
    hidden: bool,
    selected: Option<usize>,
    /// The line has the keys. The chooser opens with the list, which holds the vim keys.
    in_line: bool,
    /// A lone `g` takes the next key whole, completing `gg` or, in a folder, `gh`.
    pending_g: bool,
    held_enter: bool,
    dialog: bool,
    opening: bool,
    closed: bool,
    message: Option<String>,
}

impl Chooser {
    pub fn new(mode: Mode, home: Option<PathBuf>, lang: Lang, generation: u32) -> Chooser {
        let mut chooser = Chooser {
            mode,
            home,
            lang,
            generation,
            counter: 0,
            list_serial: None,
            rows: Vec::new(),
            rows_rev: u64::from(generation) << 32,
            items: Vec::new(),
            view_rows: Arc::from([]),
            rows_key: None,
            line: String::new(),
            place: Place::Projects,
            last_shown: None,
            recent: VecDeque::new(),
            hidden: false,
            selected: Some(0),
            in_line: false,
            pending_g: false,
            held_enter: false,
            dialog: false,
            opening: false,
            closed: false,
            message: None,
        };
        chooser.rebuild();
        chooser
    }

    fn serial(&mut self) -> u64 {
        self.counter = self.counter.checked_add(1).expect("chooser request counter exhausted");
        (u64::from(self.generation) << 32) | u64::from(self.counter)
    }

    fn rows_changed(&mut self) {
        self.rows_rev = self.rows_rev.checked_add(1).expect("chooser rows revision exhausted");
    }

    pub fn shown(&mut self) -> Vec<Effect> {
        self.rows_changed();
        let serial = self.serial();
        self.list_serial = Some(serial);
        self.drop_held();
        vec![Effect::List { serial }]
    }

    pub fn event(&mut self, ev: Ev) -> Vec<Effect> {
        if self.closed {
            return vec![];
        }
        let reply = matches!(
            ev,
            Ev::Rows { .. }
                | Ev::Listing { .. }
                | Ev::Later { .. }
                | Ev::Slow { .. }
                | Ev::Stuck { .. }
                | Ev::Freed { .. }
        );
        if (self.opening && !reply) || (self.dialog && !reply && !matches!(ev, Ev::Folder(_))) {
            return vec![];
        }
        if matches!(ev, Ev::Line(_)) && std::mem::take(&mut self.pending_g) {
            match ev {
                Ev::Line(LineEv::Key('g')) => {
                    self.drop_held();
                    self.selected = Some(0);
                    return vec![];
                }
                Ev::Line(LineEv::Key('h')) if matches!(self.place, Place::Folder(_)) => return self.go_home(),
                Ev::Line(LineEv::Changed(_) | LineEv::KeysLost) => {}
                _ => return vec![],
            }
        }
        match ev {
            Ev::Rows { serial, rows } if self.list_serial == Some(serial) => {
                self.list_serial = None;
                self.rows = rows
                    .into_iter()
                    .filter(|row| !matches!(&self.mode, Mode::Window { root } if root == &row.path))
                    .collect();
                self.rebuild();
                if matches!(self.place, Place::Projects) {
                    self.selected = Some(0);
                    if std::mem::take(&mut self.held_enter)
                        && !self.opening
                        && !self.dialog
                        && matches!(self.items.first(), Some(Item::Project(_)))
                    {
                        return self.activate();
                    }
                }
                vec![]
            }
            Ev::Listing { serial, dir, listing } => self.listing(serial, dir, listing),
            Ev::Later { serial, git, links } => {
                let Place::Folder(b) = &mut self.place else {
                    return vec![];
                };
                if b.later_serial != Some(serial) {
                    return vec![];
                }
                if git.is_empty() && links.is_empty() {
                    return vec![];
                }
                b.git.extend(git);
                if let Some(listing) = &mut b.listing {
                    update_links(listing, &links);
                }
                self.remember_shown();
                self.rebuild_keep_selection();
                vec![]
            }
            Ev::Slow { serial } | Ev::Stuck { serial } => {
                let stuck = matches!(ev, Ev::Stuck { .. });
                if let Place::Folder(b) = &mut self.place {
                    if b.serial == Some(serial) && (stuck || b.shown == Shown::Waiting) {
                        b.shown = if stuck { Shown::Stuck } else { Shown::Slow };
                        if stuck {
                            b.held_enters = 0;
                        }
                        if !self.opening {
                            self.message = None;
                        }
                    }
                }
                vec![]
            }
            Ev::Freed { .. } if matches!(&self.place, Place::Folder(b) if b.shown == Shown::Stuck || (b.shown == Shown::Done && matches!(b.listing, Some(Listing::Stuck | Listing::TooManyStuck)))) => {
                self.request(false)
            }
            Ev::Line(LineEv::Changed(text)) => {
                self.in_line |= text != self.line;
                self.changed(text, true)
            }
            Ev::Line(LineEv::Key(c)) => self.key(c),
            Ev::Line(LineEv::Enter) if matches!(self.place, Place::Projects) && self.list_serial.is_some() => {
                self.held_enter = true;
                vec![]
            }
            Ev::Line(LineEv::Enter) => self.activate(),
            Ev::Line(LineEv::Step(step)) => self.step(step),
            Ev::Line(LineEv::Side(direction)) if !self.in_line => self.side(direction),
            Ev::Line(LineEv::Up) if !self.in_line && matches!(self.place, Place::Folder(_)) => self.go_up(),
            Ev::Line(LineEv::DeleteSegment) if self.in_line => self.delete_segment(),
            Ev::Line(LineEv::Escape) => self.escape(),
            Ev::Line(LineEv::Tab) if self.in_line => self.complete(),
            Ev::Line(LineEv::ChooseFolder) => self.folder_dialog(),
            Ev::Pick { index, rows_rev } | Ev::PickTwice { index, rows_rev }
                if rows_rev == self.rows_rev && index < self.items.len() =>
            {
                let twice = matches!(ev, Ev::PickTwice { .. });
                // Projects act on the first press; only folder rows have a double-click action.
                if twice && matches!(self.place, Place::Projects) {
                    return vec![];
                }
                self.drop_held();
                self.pending_g = false;
                self.selected = Some(index);
                if twice || matches!(self.place, Place::Projects) {
                    self.activate()
                } else {
                    vec![]
                }
            }
            Ev::Dismiss | Ev::Line(LineEv::KeysLost) => self.close(),
            Ev::Folder(path) if self.dialog => {
                self.dialog = false;
                path.map(|path| vec![Effect::Open(path)]).unwrap_or_default()
            }
            _ => vec![],
        }
    }

    fn drop_held(&mut self) {
        self.held_enter = false;
        if let Place::Folder(b) = &mut self.place {
            b.held_enters = 0;
        }
    }

    fn step(&mut self, step: i32) -> Vec<Effect> {
        self.drop_held();
        let last = self.items.len().saturating_sub(1);
        self.selected = Some((self.selected.unwrap_or(0) as i64 + i64::from(step)).clamp(0, last as i64) as usize);
        vec![]
    }

    /// In the list printable keys are commands. A key delivered to the line is still text, even if the view lagged.
    fn key(&mut self, c: char) -> Vec<Effect> {
        if self.in_line {
            let mut next = self.line.clone();
            next.push(c);
            let mut effects = self.changed(next.clone(), true);
            effects.insert(0, Effect::SetLine(next));
            return effects;
        }
        match c {
            'j' => self.step(1),
            'k' => self.step(-1),
            'g' => {
                self.pending_g = true;
                vec![]
            }
            'G' => {
                self.drop_held();
                self.selected = Some(self.items.len().saturating_sub(1));
                vec![]
            }
            '/' => {
                self.in_line = true;
                vec![]
            }
            '~' => self.go_home(),
            '-' if matches!(self.place, Place::Projects) => self.browse_default(),
            '-' | 'h' if matches!(self.place, Place::Folder(_)) => self.go_up(),
            'h' => self.side(-1),
            'l' => self.side(1),
            '.' if matches!(self.place, Place::Folder(_)) => {
                self.drop_held();
                self.hidden = !self.hidden;
                self.rebuild_keep_selection();
                vec![]
            }
            'q' => self.close(),
            _ => vec![],
        }
    }

    fn close(&mut self) -> Vec<Effect> {
        self.rows_changed();
        self.closed = true;
        self.drop_held();
        vec![Effect::Close]
    }

    fn escape(&mut self) -> Vec<Effect> {
        self.drop_held();
        self.message = None;
        if self.in_line {
            self.in_line = false;
            if matches!(self.place, Place::Folder(_)) {
                if let Some(mut last) = self.last_shown.clone() {
                    last.typed = display_name(&last.typed);
                    let same = matches!(&self.place, Place::Folder(b) if b.dir == last.dir);
                    if same && matches!(&self.place, Place::Folder(b) if b.requested_filter.is_some()) {
                        return self.navigate(last.dir, last.typed, None);
                    }
                    if same {
                        if let Place::Folder(b) = &mut self.place {
                            b.filter.clear();
                            b.typed = display_name(&b.typed);
                            self.line = b.typed.clone();
                        }
                    } else {
                        self.place = Place::Folder(Box::new(Browse {
                            dir: last.dir,
                            typed: last.typed.clone(),
                            filter: String::new(),
                            listing: Some(last.listing.clone()),
                            shown: Shown::Done,
                            serial: None,
                            later_serial: None,
                            requested_filter: None,
                            needs_filter: limited(Some(&last.listing)),
                            git: BTreeSet::new(),
                            left: None,
                            held_enters: 0,
                        }));
                        self.line = last.typed;
                    }
                    self.selected = Some(0);
                    self.rebuild();
                    return vec![];
                }
            }
            self.projects_place();
            return vec![];
        }
        if matches!(self.place, Place::Folder(_)) {
            self.projects_place();
            vec![]
        } else {
            self.close()
        }
    }

    fn projects_place(&mut self) {
        self.place = Place::Projects;
        self.last_shown = None;
        self.line.clear();
        self.selected = Some(0);
        self.rebuild();
    }

    fn base(&self) -> &Path {
        match &self.mode {
            Mode::Window { root } => root,
            Mode::Launch => self.home.as_deref().unwrap_or(Path::new("/")),
        }
    }

    /// Split typed text without interpreting display escapes. Only an unchanged model-written folder prefix retains
    /// its raw path, so editing an escaped or lossy folder name asks for exactly what the person typed.
    fn split_line(&self, text: &str) -> Option<(PathBuf, String, String)> {
        if text == "~" {
            let home = self.home.as_ref()?;
            return Some((
                home.parent().unwrap_or(home).into(),
                "~".into(),
                home.file_name().unwrap_or_default().to_string_lossy().into(),
            ));
        }
        let index = text.rfind('/')?;
        let (head, filter) = text.split_at(index + 1);
        let raw_dir = match &self.place {
            Place::Folder(b) if head == b.typed => Some(b.dir.clone()),
            _ => None,
        };
        let dir = raw_dir.or_else(|| path::expand(head, self.base(), self.home.as_deref()))?;
        if filter == "." || filter == ".." {
            let dir = if filter == ".." { dir.join("..") } else { dir };
            return Some((dir.components().collect(), format!("{text}/"), String::new()));
        }
        Some((dir.components().collect(), head.into(), filter.into()))
    }

    fn changed(&mut self, text: String, debounce: bool) -> Vec<Effect> {
        if text == self.line {
            return vec![];
        }
        self.drop_held();
        self.message = None;
        self.selected = Some(0);
        if !is_path(&text) {
            self.line = text;
            self.place = Place::Projects;
            self.last_shown = None;
            self.rebuild();
            return vec![];
        }
        let split = self.split_line(&text);
        self.line = text;
        let Some((dir, typed, filter)) = split else {
            self.place = Place::Folder(Box::new(Browse {
                dir: self.base().to_path_buf(),
                typed: self.line.clone(),
                filter: String::new(),
                shown: Shown::Done,
                listing: Some(Listing::Missing),
                serial: None,
                later_serial: None,
                requested_filter: None,
                needs_filter: false,
                git: BTreeSet::new(),
                left: None,
                held_enters: 0,
            }));
            self.rebuild();
            return vec![];
        };
        if let Place::Folder(b) = &mut self.place {
            if b.dir == dir {
                let changed_filter = b.filter != filter;
                let changed_folder = b.typed != typed;
                b.typed = typed;
                b.filter = filter;
                b.left = None;
                let reread = changed_folder || (changed_filter && b.needs_filter);
                self.rebuild();
                return if reread { self.request(debounce) } else { vec![] };
            }
        }
        self.set_folder(dir, typed, filter, None);
        self.request(debounce)
    }

    fn set_folder(&mut self, dir: PathBuf, typed: String, filter: String, near: Option<OsString>) {
        let cached = self
            .recent
            .iter()
            .position(|(key, _)| key == &dir)
            .and_then(|i| self.recent.remove(i));
        let listing = cached.as_ref().map(|(_, listing)| listing.clone());
        let needs_filter = limited(listing.as_ref());
        if let Some(cached) = cached {
            self.recent.push_back(cached);
        }
        self.place = Place::Folder(Box::new(Browse {
            dir,
            typed,
            filter,
            shown: Shown::Waiting,
            listing,
            serial: None,
            later_serial: None,
            requested_filter: None,
            needs_filter,
            git: BTreeSet::new(),
            left: near,
            held_enters: 0,
        }));
        self.selected = Some(0);
        self.message = None;
        self.rebuild();
        self.select_left();
        self.remember_shown();
    }

    fn request(&mut self, debounce: bool) -> Vec<Effect> {
        let serial = self.serial();
        let Place::Folder(b) = &mut self.place else {
            return vec![];
        };
        let filter = (!b.filter.is_empty() && b.needs_filter).then(|| b.filter.clone());
        b.requested_filter = filter.clone();
        b.serial = Some(serial);
        b.later_serial = None;
        b.shown = Shown::Waiting;
        if !self.opening {
            self.message = None;
        }
        vec![Effect::Read {
            serial,
            dir: b.dir.clone(),
            filter,
            debounce,
            near: b.left.clone(),
        }]
    }

    fn navigate(&mut self, dir: PathBuf, typed: String, near: Option<OsString>) -> Vec<Effect> {
        self.drop_held();
        let typed = display_name(&typed);
        self.line = typed.clone();
        self.set_folder(dir, typed, String::new(), near);
        let mut effects = self.request(false);
        effects.insert(0, Effect::SetLine(self.line.clone()));
        effects
    }

    fn displayed_path(&self, path: &Path) -> String {
        Row::new(path.into(), self.home.as_deref(), String::new(), Fact::Present).shown
    }

    fn folder_line(&self, dir: &Path) -> String {
        let mut line = self.displayed_path(dir);
        if !line.ends_with('/') {
            line.push('/');
        }
        line
    }

    fn go_home(&mut self) -> Vec<Effect> {
        let Some(home) = self.home.clone() else { return vec![] };
        self.navigate(home.clone(), self.folder_line(&home), None)
    }

    fn browse_default(&mut self) -> Vec<Effect> {
        self.in_line = false;
        match &self.mode {
            Mode::Launch => {
                let dir = self.base().to_path_buf();
                self.navigate(dir.clone(), self.folder_line(&dir), None)
            }
            Mode::Window { root } => {
                let dir = root.parent().unwrap_or(root).to_path_buf();
                let near = root.file_name().map(OsString::from);
                self.navigate(dir.clone(), self.folder_line(&dir), near)
            }
        }
    }

    fn side(&mut self, direction: i32) -> Vec<Effect> {
        self.drop_held();
        if let Place::Folder(_) = self.place {
            return if direction < 0 {
                self.go_up()
            } else {
                match self.selected.and_then(|i| self.items.get(i)) {
                    Some(Item::Entry(e)) if !is_file(e) => self.activate_folder(),
                    _ => vec![],
                }
            };
        }
        match self.selected.and_then(|i| self.items.get(i)).cloned() {
            Some(Item::Project(row)) => self.browse_path(row.path, direction),
            Some(Item::Home(path)) => self.browse_path(path, direction),
            Some(Item::Browse) if direction > 0 => self.browse_default(),
            _ => vec![],
        }
    }

    fn browse_path(&mut self, path: PathBuf, direction: i32) -> Vec<Effect> {
        let (dir, near) = if direction < 0 {
            (
                path.parent().unwrap_or(&path).to_path_buf(),
                path.file_name().map(OsString::from),
            )
        } else {
            (path, None)
        };
        self.navigate(dir.clone(), self.folder_line(&dir), near)
    }

    fn go_up(&mut self) -> Vec<Effect> {
        let Place::Folder(b) = &self.place else { return vec![] };
        // A walked folder ending in `..` goes up by walking one more `..`: a link before it changes what the parent
        // means, and `Path::parent` would instead step back down into the folder the `..` left.
        let (dir, near) = if b.dir.components().next_back() == Some(Component::ParentDir) {
            (b.dir.join(".."), None)
        } else {
            let Some(parent) = b.dir.parent() else { return vec![] };
            (parent.to_path_buf(), b.dir.file_name().map(OsString::from))
        };
        self.navigate(dir.clone(), self.folder_line(&dir), near)
    }

    fn delete_segment(&mut self) -> Vec<Effect> {
        let without_slash = self.line.strip_suffix('/').unwrap_or(&self.line);
        let next = without_slash
            .rfind('/')
            .map(|i| self.line[..=i].to_owned())
            .unwrap_or_default();
        let mut effects = self.changed(next.clone(), true);
        effects.insert(0, Effect::SetLine(next));
        effects
    }

    fn folder_dialog(&mut self) -> Vec<Effect> {
        self.drop_held();
        self.dialog = true;
        let at = match &self.place {
            Place::Folder(b) if b.shown != Shown::Stuck && b.serial.is_none() && readable(b.listing.as_ref()) => {
                Some(b.dir.clone())
            }
            _ => None,
        };
        vec![Effect::FolderDialog { at }]
    }

    fn activate(&mut self) -> Vec<Effect> {
        if matches!(self.place, Place::Folder(_)) {
            return self.activate_folder();
        }
        match self.selected.and_then(|i| self.items.get(i)).cloned() {
            Some(Item::Project(row)) if row.fact == Fact::Gone => {
                self.message = Some(text::gone(self.lang, &display_name(&row.shown)));
                vec![]
            }
            Some(Item::Project(row)) => vec![Effect::Open(row.path)],
            Some(Item::Home(path)) => vec![Effect::Open(path)],
            Some(Item::Browse) => self.browse_default(),
            _ => vec![],
        }
    }

    /// Folders are entered first. Only this folder's own row opens it, once its read has answered.
    fn activate_folder(&mut self) -> Vec<Effect> {
        let Place::Folder(b) = &mut self.place else {
            return vec![];
        };
        if b.shown == Shown::Stuck
            || (b.shown == Shown::Done && matches!(b.listing, Some(Listing::Stuck | Listing::TooManyStuck)))
        {
            b.held_enters = 0;
            let shown = Row::new(b.dir.clone(), self.home.as_deref(), String::new(), Fact::Present).shown;
            self.message = Some(text::cannot_read(self.lang, &shown));
            return vec![];
        }
        if b.serial.is_some() {
            b.held_enters = b.held_enters.saturating_add(1).min(2);
            return vec![];
        }
        if !readable(b.listing.as_ref()) {
            self.message = Some(match b.listing {
                Some(Listing::Missing) => text::missing(self.lang, &display_name(&self.line)),
                Some(Listing::Loop) => text::text(self.lang).looped.into(),
                Some(Listing::NotAFolder) => text::text(self.lang).not_folder.into(),
                _ => text::cannot_read(self.lang, &display_name(&self.line)),
            });
            b.held_enters = 0;
            return vec![];
        }
        match self.selected.and_then(|i| self.items.get(i)).cloned() {
            Some(Item::Here) => vec![Effect::Open(b.dir.clone())],
            Some(Item::Entry(e)) if is_file(&e) => vec![Effect::Open(b.dir.join(&e.raw))],
            Some(Item::Entry(e))
                if matches!(
                    e.kind,
                    EntryKind::Link {
                        kind: LinkKind::Other,
                        ..
                    }
                ) =>
            {
                self.message = Some(text::text(self.lang).not_folder.into());
                vec![]
            }
            Some(Item::Entry(e)) => {
                let dir = b.dir.join(&e.raw);
                let typed = if b.typed == "~" {
                    self.folder_line(&dir)
                } else {
                    format!("{}{}/", b.typed, display_name(&e.raw.to_string_lossy()))
                };
                self.navigate(dir, typed, None)
            }
            _ => vec![],
        }
    }

    fn complete(&mut self) -> Vec<Effect> {
        let Place::Folder(b) = &self.place else { return vec![] };
        let names: Vec<_> = self
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Entry(e)
                    if !is_file(e)
                        && !matches!(
                            e.kind,
                            EntryKind::Link {
                                kind: LinkKind::Other,
                                ..
                            }
                        ) =>
                {
                    Some(e)
                }
                _ => None,
            })
            .collect();
        let Some(first) = names.first() else { return vec![] };
        if names.len() == 1 {
            let dir = b.dir.join(&first.raw);
            let typed = if b.typed == "~" {
                self.folder_line(&dir)
            } else {
                format!("{}{}/", b.typed, display_name(&first.raw.to_string_lossy()))
            };
            return self.navigate(dir, typed, None);
        }
        let mut common = first.raw.to_string_lossy().into_owned();
        for entry in names.iter().skip(1) {
            let name = entry.raw.to_string_lossy();
            let length = common
                .chars()
                .zip(name.chars())
                .take_while(|(a, b)| a.to_lowercase().eq(b.to_lowercase()))
                .map(|(c, _)| c.len_utf8())
                .sum();
            common.truncate(length);
        }
        // The filter is matched against raw names, so only text that displays as itself can be written back: a name's
        // hidden or escaped characters end the shared prefix.
        let visible: usize = common.chars().take_while(|c| !is_hidden(*c)).map(char::len_utf8).sum();
        common.truncate(visible);
        let next = if common.to_lowercase().starts_with(&b.filter.to_lowercase()) {
            format!("{}{}", b.typed, display_name(&common))
        } else {
            self.line.clone()
        };
        let mut effects = self.changed(next.clone(), false);
        effects.insert(0, Effect::SetLine(next));
        effects
    }

    fn listing(&mut self, serial: u64, dir: PathBuf, listing: Listing) -> Vec<Effect> {
        let Place::Folder(b) = &mut self.place else {
            return vec![];
        };
        if b.serial != Some(serial) || b.dir != dir {
            return vec![];
        }
        let held = std::mem::take(&mut b.held_enters);
        let selected = self
            .selected
            .and_then(|i| self.items.get(i))
            .and_then(item_raw)
            .filter(|_| b.listing.is_some())
            .map(OsString::from);
        b.serial = None;
        b.later_serial = Some(serial);
        b.shown = Shown::Done;
        if b.requested_filter.is_none() && matches!(listing, Listing::Read { .. } | Listing::Denied { .. }) {
            self.recent.retain(|(key, _)| key != &dir);
            self.recent.push_back((dir, listing.clone()));
            if self.recent.len() > 8 {
                self.recent.pop_front();
            }
        }
        if b.requested_filter.is_none() {
            b.needs_filter = limited(Some(&listing));
        }
        let reread = b.needs_filter && !b.filter.is_empty() && b.requested_filter.as_deref() != Some(&b.filter);
        b.listing = Some(listing);
        b.git.clear();
        if !self.opening {
            self.message = None;
        }
        self.remember_shown();
        self.rebuild();
        self.select_left();
        if let Some(raw) = selected {
            self.select_raw(&raw);
        }
        if reread {
            let effects = self.request(false);
            if !self.opening && !self.dialog {
                if let Place::Folder(b) = &mut self.place {
                    b.held_enters = held;
                }
            }
            return effects;
        }
        if self.opening || self.dialog {
            return vec![];
        }
        for remaining in (0..held).rev() {
            let effects = self.activate_folder();
            if !effects.is_empty() {
                if effects.iter().any(|e| matches!(e, Effect::Read { .. })) {
                    if let Place::Folder(b) = &mut self.place {
                        b.held_enters = remaining;
                    }
                }
                return effects;
            }
        }
        vec![]
    }

    fn remember_shown(&mut self) {
        if let Place::Folder(b) = &self.place {
            if readable(b.listing.as_ref()) {
                self.last_shown = Some(FolderShown {
                    dir: b.dir.clone(),
                    typed: b.typed.clone(),
                    listing: b.listing.clone().unwrap(),
                });
            }
        }
    }

    fn select_raw(&mut self, raw: &std::ffi::OsStr) {
        if let Some(index) = self.items.iter().position(|item| item_raw(item) == Some(raw)) {
            self.selected = Some(index);
        }
    }

    fn select_left(&mut self) {
        if let Place::Folder(b) = &self.place {
            if let Some(raw) = b.left.clone() {
                self.select_raw(&raw);
            }
        }
    }

    fn rebuild_keep_selection(&mut self) {
        let raw = self
            .selected
            .and_then(|i| self.items.get(i))
            .and_then(item_raw)
            .map(OsString::from);
        self.rebuild();
        if let Some(raw) = raw {
            self.select_raw(&raw);
        }
    }

    fn rebuild(&mut self) {
        let items: Vec<Item> = match &self.place {
            Place::Projects => {
                let needle = self.line.trim().to_lowercase();
                let mut items: Vec<_> = self
                    .rows
                    .iter()
                    .filter(|r| r.name.to_lowercase().contains(&needle) || r.shown.to_lowercase().contains(&needle))
                    .cloned()
                    .map(Item::Project)
                    .collect();
                let first_run = self.rows.is_empty() && matches!(self.mode, Mode::Launch) && needle.is_empty();
                if first_run {
                    items.push(Item::Browse);
                }
                if matches!(self.mode, Mode::Launch) && needle.is_empty() {
                    if let Some(home) = &self.home {
                        if !self.rows.iter().any(|r| &r.path == home) {
                            items.push(Item::Home(home.clone()));
                        }
                    }
                }
                if !first_run {
                    items.push(Item::Browse);
                }
                items
            }
            Place::Folder(b) => {
                let mut items = Vec::new();
                if b.filter.is_empty() {
                    items.push(Item::Here);
                }
                let (entries, exact) = match &b.listing {
                    Some(Listing::Read { entries, exact, .. }) => (entries.as_slice(), exact.as_ref()),
                    Some(Listing::Denied { exact }) => (&[][..], exact.as_ref()),
                    _ => (&[][..], None),
                };
                let needle = b.filter.to_lowercase();
                let mut entries: Vec<_> = entries
                    .iter()
                    .chain(exact)
                    .filter(|e| {
                        let name = e.raw.to_string_lossy();
                        (!name.starts_with('.') || self.hidden || needle.starts_with('.'))
                            && if is_file(e) {
                                !b.filter.is_empty() && name == b.filter
                            } else {
                                name.to_lowercase().contains(&needle)
                            }
                    })
                    .cloned()
                    .collect();
                entries.sort_by(|a, b| {
                    rank(&a.raw.to_string_lossy(), &needle)
                        .cmp(&rank(&b.raw.to_string_lossy(), &needle))
                        .then_with(|| natural_cmp(&a.raw.to_string_lossy(), &b.raw.to_string_lossy()))
                        .then_with(|| a.raw.cmp(&b.raw))
                });
                entries.dedup_by(|a, b| a.raw == b.raw);
                items.extend(entries.into_iter().map(Item::Entry));
                items
            }
        };
        let view_rows: Arc<[RowView]> = items.iter().map(|item| self.row_view(item)).collect();
        let key = match &self.place {
            Place::Folder(b) => Some(b.dir.clone()),
            Place::Projects => None,
        };
        // The revision moves only with the rows a press can land on: their items, their text and the folder they open
        // from. A rebuild that leaves all three as they were keeps the view's rows and any gesture in flight.
        if items != self.items || view_rows != self.view_rows || key != self.rows_key {
            self.rows_changed();
            self.rows_key = key;
        }
        self.items = items;
        self.view_rows = view_rows;
        self.selected = self.selected.map(|i| i.min(self.items.len().saturating_sub(1)));
    }

    fn recent_mark(&self, physical: Option<&Path>) -> Option<String> {
        let path = physical?;
        let t = text::text(self.lang);
        if matches!(&self.mode, Mode::Window { root } if root == path) {
            return Some(t.open.into());
        }
        self.rows
            .iter()
            .find(|row| row.path == path)
            .and_then(|row| match row.fact {
                Fact::Open => Some(t.open.into()),
                Fact::Present | Fact::Unknown if !row.age.is_empty() => Some(display_name(&row.age)),
                _ => None,
            })
    }

    fn row_view(&self, item: &Item) -> RowView {
        let t = text::text(self.lang);
        let mut row = RowView {
            name: String::new(),
            shown: String::new(),
            right: String::new(),
            dimmed: false,
            kind: RowKind::Action,
        };
        match item {
            Item::Project(project) => {
                row.kind = RowKind::Project;
                row.name = display_name(&project.name);
                row.shown = display_name(&project.shown);
                row.right = match project.fact {
                    Fact::Open => t.open.into(),
                    Fact::Gone => t.gone.into(),
                    _ => display_name(&project.age),
                };
                row.dimmed = project.fact == Fact::Gone;
            }
            Item::Browse => row.name = t.folder.into(),
            Item::Home(path) => {
                row.name = t.home.into();
                row.shown = self.displayed_path(path);
            }
            Item::Here => {
                row.name = ".".into();
                row.kind = RowKind::Here;
                row.right = t.here.into();
                if let Place::Folder(b) = &self.place {
                    row.shown = self.displayed_path(&b.dir);
                    let physical = match &b.listing {
                        Some(Listing::Read { dir, .. }) => Some(dir.as_path()),
                        _ => None,
                    };
                    if let Some(mark) = self.recent_mark(physical) {
                        row.right.push_str(&format!(" · {mark}"));
                    }
                }
            }
            Item::Entry(e) => {
                row.name = display_name(&e.raw.to_string_lossy());
                let mut marks = Vec::new();
                row.kind = match &e.kind {
                    EntryKind::Dir { mount_point: true } => {
                        marks.push(t.mount.to_owned());
                        RowKind::Mount
                    }
                    EntryKind::Dir { mount_point: false } => RowKind::Folder,
                    EntryKind::File => {
                        marks.push(t.file.to_owned());
                        RowKind::File
                    }
                    EntryKind::Link { target, kind } => {
                        row.shown = format!("-> {}", display_name(target));
                        match kind {
                            LinkKind::File => {
                                marks.push(t.file.to_owned());
                                RowKind::File
                            }
                            LinkKind::Dir => RowKind::Folder,
                            LinkKind::Unknown | LinkKind::Other => {
                                marks.push(t.link.to_owned());
                                RowKind::Link
                            }
                        }
                    }
                };
                if !is_file(e) {
                    row.name.push('/');
                }
                row.dimmed = matches!(
                    e.kind,
                    EntryKind::Link {
                        kind: LinkKind::Other,
                        ..
                    }
                );
                if let Place::Folder(b) = &self.place {
                    if b.git.contains(&e.raw) {
                        marks.push(t.git.into());
                    }
                }
                if let Some(mark) = self.recent_mark(e.physical.as_deref()) {
                    marks.push(mark);
                }
                row.right = marks.join(" · ");
            }
        }
        row
    }

    /// The recent projects held, after leaving out the window's own: what the carrier's rows log counts.
    pub fn projects(&self) -> usize {
        self.rows.len()
    }

    pub fn view(&self) -> ChooserView {
        let t = text::text(self.lang);
        let browsing = matches!(self.place, Place::Folder(_));
        let message = self.message.clone().or_else(|| match &self.place {
            Place::Projects => {
                (self.list_serial.is_none() && self.rows.is_empty() && self.line.is_empty()).then(|| t.empty.into())
            }
            Place::Folder(b) => match b.shown {
                Shown::Slow => Some(t.reading.into()),
                Shown::Stuck => Some(t.stuck.into()),
                Shown::Waiting => None,
                Shown::Done => match &b.listing {
                    Some(Listing::Denied { .. }) => Some(t.denied.into()),
                    Some(Listing::Missing) => Some(text::missing(self.lang, &display_name(&self.line))),
                    Some(Listing::NotAFolder) => Some(t.not_folder.into()),
                    Some(Listing::Loop) => Some(t.looped.into()),
                    Some(Listing::Stuck) => Some(t.stuck.into()),
                    Some(Listing::TooManyStuck) => Some(t.too_many.into()),
                    Some(Listing::Read {
                        capped: Some(Cap::Folders),
                        ..
                    }) => Some(t.folders_cap.into()),
                    Some(Listing::Read {
                        capped: Some(Cap::Scanned),
                        ..
                    }) => Some(t.scanned_cap.into()),
                    Some(Listing::Read { .. }) if self.items.is_empty() => {
                        Some(text::no_match(self.lang, &display_name(&b.filter)))
                    }
                    Some(Listing::Read { .. }) if self.items.len() == 1 && b.filter.is_empty() => {
                        Some(t.no_folders.into())
                    }
                    _ => None,
                },
            },
        });
        let footer = if let Place::Folder(b) = &self.place {
            let item = self.selected.and_then(|i| self.items.get(i));
            let (name, enter) = match item {
                Some(Item::Entry(e)) if is_file(e) => (display_name(&e.raw.to_string_lossy()), text::Enter::OpenFile),
                Some(Item::Entry(e))
                    if matches!(
                        e.kind,
                        EntryKind::Link {
                            kind: LinkKind::Other,
                            ..
                        }
                    ) =>
                {
                    (display_name(&e.raw.to_string_lossy()), text::Enter::Inert)
                }
                Some(Item::Entry(e)) => (display_name(&e.raw.to_string_lossy()), text::Enter::Into),
                _ => (
                    display_name(&b.dir.file_name().unwrap_or(b.dir.as_os_str()).to_string_lossy()),
                    text::Enter::Open,
                ),
            };
            text::footer(self.lang, &name, enter, self.in_line)
        } else {
            if self.in_line { t.footer } else { t.footer_list }.into()
        };
        ChooserView {
            title: if browsing {
                format!("{}{}", t.browse, if self.hidden { t.hidden } else { "" })
            } else {
                t.title.into()
            },
            line: self.line.clone(),
            placeholder: if self.in_line {
                t.placeholder
            } else {
                t.placeholder_list
            }
            .into(),
            selected: self.selected.filter(|i| *i < self.items.len()),
            rows: self.view_rows.clone(),
            rows_rev: self.rows_rev,
            browsing,
            hidden: self.hidden,
            message,
            footer,
            opening: self.opening,
            in_line: self.in_line,
        }
    }

    pub fn opening(&mut self, name: &str) {
        self.rows_changed();
        self.opening = true;
        self.drop_held();
        self.message = Some(text::opening(self.lang, &display_name(name)));
    }

    pub fn failed(&mut self, message: String) {
        self.rows_changed();
        self.opening = false;
        self.drop_held();
        self.message = Some(display_name(&message));
    }

    /// A start failed: the keys return with what is known of why.
    pub fn start_failed(&mut self, path: PathBuf, gone: bool) {
        let row = Row::new(path, self.home.as_deref(), String::new(), Fact::Unknown);
        self.failed(start_failed_text(self.lang, &row, gone));
    }
}

fn limited(listing: Option<&Listing>) -> bool {
    matches!(
        listing,
        Some(Listing::Read { capped: Some(_), .. } | Listing::Denied { .. })
    )
}

fn readable(listing: Option<&Listing>) -> bool {
    matches!(listing, Some(Listing::Read { .. } | Listing::Denied { .. }))
}

fn is_file(entry: &Entry) -> bool {
    matches!(
        entry.kind,
        EntryKind::File
            | EntryKind::Link {
                kind: LinkKind::File,
                ..
            }
    )
}

fn item_raw(item: &Item) -> Option<&std::ffi::OsStr> {
    match item {
        Item::Here => Some(std::ffi::OsStr::new(".")),
        Item::Entry(e) => Some(&e.raw),
        Item::Project(r) => r.path.file_name(),
        _ => None,
    }
}

fn rank(name: &str, needle: &str) -> u8 {
    let name = name.to_lowercase();
    if name == needle {
        0
    } else if name.starts_with(needle) {
        1
    } else {
        2
    }
}

fn update_links(listing: &mut Listing, links: &[(OsString, LinkKind)]) {
    let (entries, exact) = match listing {
        Listing::Read { entries, exact, .. } => (entries.as_mut_slice(), exact.as_mut()),
        Listing::Denied { exact } => (&mut [][..], exact.as_mut()),
        _ => return,
    };
    for entry in entries.iter_mut().chain(exact) {
        if let EntryKind::Link { kind, .. } = &mut entry.kind {
            if let Some((_, found)) = links.iter().find(|(raw, _)| *raw == entry.raw) {
                *kind = *found;
            }
        }
    }
}

pub fn is_path(text: &str) -> bool {
    ["/", "~", "./", "../"].iter().any(|prefix| text.starts_with(prefix))
}
