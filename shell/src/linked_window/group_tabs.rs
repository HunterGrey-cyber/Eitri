//! One tabs file for a group. In compositor mode no chat window writes a file: each tells window 1 its tabs, window 1
//! keeps every window's part and writes them all to the project's one `tabs/<16 hex>.json`, the file `eitri` mode reads
//! and writes too (`saved_tabs::merge_windows`), so a mode flip finds every chat. Window 1 is the only process that
//! outlives a chat window's crash and the only one that can give it its tabs again.
//!
//! **In window 1**, [`GroupTabs`] reads the file once at its start, and offers window 2's and an unpaired window's left
//! tabs into `c1` through the claims `eitri` mode uses. The window numbers that hold tabs open at the start, and every
//! window that links is given its part: at a first link, a respawn after a crash and a relink alike. A window the user
//! closed drops its part, since those tabs were closed, once [`super::closes::WindowCloses`] has settled that it was the
//! user. One that went any other way keeps it for its next start: a crash, an end signal, a close that left tabs on offer
//! unanswered, or a close of every window at once.
//!
//! **In a chat window**, [`TabReports`] carries each new set to window 1: a change on its own at once, and what follows
//! close behind on a short timer, so a burst of tab changes costs two reports. While the link is down only the latest
//! set is kept, and it goes as soon as a link is up: window 1 is a new process then, and knows only what its file says.
//! Any close says its last set at once: its own ahead of its `bye`, the group's when window 1 asks, before it says its
//! own `bye`.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::time::Duration;

use eitri_core::saved_tabs::{self, SavedTabs, TabWriter};
use eitri_core::window_link::names::WindowName;
use eitri_core::window_link::pair::Role;
use eitri_core::window_link::Msg;
use serde_json::Value;

use super::driver::LinkDriver;

/// How long a chat window gathers the tab changes that follow a report into one more.
pub const REPORT_AFTER: Duration = Duration::from_millis(250);

/// The group's tabs, in window 1.
pub struct GroupTabs {
    /// What reading the files could not use ([`GroupTabs::startup_problems`]).
    startup_problems: Vec<String>,
    writer: TabWriter,
    /// Each chat window's tabs, none of them empty: what it last reported, or the file's part before its first report.
    parts: BTreeMap<WindowName, SavedTabs>,
    /// The group is closing: a window that goes now goes with it, and its tabs stay.
    keep_windows: bool,
    /// Window 1 is closing and the file is done: what is here is what the next launch gets.
    closing: bool,
}

impl GroupTabs {
    /// What reading the project's files could not use, for the window to say once.
    pub fn startup_problems(&self) -> &[String] {
        &self.startup_problems
    }

    /// Reads the project's file from `dir` once and takes up what other windows left: window 2's, and an unpaired
    /// window's when no other window is on the project. Those claims go `.taken` once the file holds their tabs, and
    /// their tabs join `c1`, the window that always exists.
    pub fn read(dir: Option<&Path>, project_root: &Path, alone_on_project: bool) -> GroupTabs {
        let (own, notes) = saved_tabs::startup(dir, project_root);
        let others: &[Role] = if alone_on_project {
            &[Role::Second, Role::Unpaired]
        } else {
            &[Role::Second]
        };
        let (offered, offer_notes, takeovers) = saved_tabs::startup_others(dir, project_root, Role::Main, others);
        let mut startup_problems = Vec::new();
        for note in notes.into_iter().chain(offer_notes) {
            eprintln!("{note}");
            startup_problems.push(note);
        }
        let own = own.unwrap_or_default();
        let mut writer = TabWriter::new_as(dir.map(Path::to_path_buf), PathBuf::from(project_root), Role::Main);
        writer.starts_from(own.clone());
        writer.take_over_when_written(takeovers);
        let file = match offered {
            Some(offered) => own.followed_by(offered.in_base_view()),
            None => own,
        };
        let parts = saved_tabs::windows_holding(&file)
            .into_iter()
            .map(|name| {
                let part = saved_tabs::tabs_for(&file, &name);
                (name, part)
            })
            .collect();
        GroupTabs {
            startup_problems,
            writer,
            parts,
            keep_windows: false,
            closing: false,
        }
    }

    /// The chat windows that hold tabs, by number: the start opens each of them.
    pub fn windows(&self) -> Vec<WindowName> {
        self.parts.keys().cloned().collect()
    }

    /// The chat window `name` reported its tabs. Its input: what is no set of tabs leaves its part as it was.
    pub fn reported(&mut self, name: &WindowName, tabs: &Value) {
        if name.chat_number().is_none() {
            return;
        }
        match SavedTabs::from_wire(tabs) {
            Ok(tabs) if tabs.is_empty() => {
                self.parts.remove(name);
            }
            Ok(tabs) => {
                self.parts.insert(name.clone(), tabs.in_base_view());
            }
            Err(why) => {
                eprintln!("[tabs] what {} reported is dropped: {why}", name.text());
                return;
            }
        }
        self.save();
    }

    /// The window `name` was closed: its tabs are closed, and do not come back.
    pub fn window_closed(&mut self, name: &WindowName) {
        if !self.keep_windows && self.parts.remove(name).is_some() {
            self.save();
        }
    }

    /// The group has started to close: last reports are still taken, and no window's going drops anything.
    pub fn keep_every_window(&mut self) {
        self.keep_windows = true;
    }

    /// What the window `name` is told at each link, when it has tabs to take up.
    pub fn given(&self, name: &WindowName) -> Option<Msg> {
        self.parts.get(name).map(|part| Msg::TabsGiven {
            tabs: part.to_wire(),
            adopt: false,
        })
    }

    /// Window 1 is closing: the file keeps what it has, and the receiver reports once its last write is done. `None`
    /// when nothing is owed.
    pub fn finish(&mut self) -> Option<Receiver<()>> {
        self.keep_windows = true;
        self.closing = true;
        self.writer.finish()
    }

    /// `c1`'s tab on screen is the file's: of every window, it is the only one that no view can carry.
    fn save(&mut self) {
        if self.closing {
            return;
        }
        let parts: Vec<(WindowName, SavedTabs)> = self.parts.iter().map(|(n, p)| (n.clone(), p.clone())).collect();
        let merged = saved_tabs::merge_windows(&parts, parts.first().map(|(name, _)| name));
        self.writer.observe(Some(merged));
    }

    /// Waits until the file holds what it was handed, for a test to read it.
    #[cfg(test)]
    pub(crate) fn wait_written(&self) -> bool {
        self.writer.wait_written(Duration::from_secs(5))
    }
}

/// `open`, the windows the start layout opens, then the windows in `holding` it does not name: every window that holds
/// tabs opens, and the layout's order stands.
pub fn beside_the_layout(mut open: Vec<WindowName>, holding: Vec<WindowName>) -> Vec<WindowName> {
    for name in holding {
        if !open.contains(&name) {
            open.push(name);
        }
    }
    open
}

/// A chat window's tabs on their way to window 1.
#[derive(Default)]
pub struct TabReports {
    /// The set last seen, kept for the next link once it has gone out.
    latest: RefCell<Option<SavedTabs>>,
    /// `latest` has not gone out.
    unsent: Cell<bool>,
    /// A timer is running that will send it.
    timing: Cell<bool>,
}

impl TabReports {
    /// The window's tabs are `tabs` now. With no timer running they go out on `link` at once, and true says one must be
    /// started for [`TabReports::timer_ended`], which gathers what follows.
    pub fn changed(&self, tabs: SavedTabs, link: Option<&LinkDriver>) -> bool {
        *self.latest.borrow_mut() = Some(tabs);
        self.unsent.set(true);
        if self.timing.replace(true) {
            return false;
        }
        if let Some(link) = link {
            self.send(link);
        }
        true
    }

    /// The timer ran out: what it gathered goes out on `link`, or waits for one.
    pub fn timer_ended(&self, link: Option<&LinkDriver>) {
        self.timing.set(false);
        if let (true, Some(link)) = (self.unsent.get(), link) {
            self.send(link);
        }
    }

    /// A link is up: the window at its far end hears the latest set, even one an earlier window heard, since that may
    /// not be the window that is there now.
    pub fn linked(&self, link: &LinkDriver) {
        self.send(link);
    }

    /// The window closes: `now`, its tabs as they stand when the panel can say, goes out ahead of the `bye`.
    pub fn closing(&self, link: &LinkDriver, now: Option<SavedTabs>) {
        if now.is_some() {
            *self.latest.borrow_mut() = now;
        }
        self.send(link);
    }

    fn send(&self, link: &LinkDriver) {
        let report = self
            .latest
            .borrow()
            .as_ref()
            .map(|tabs| Msg::TabsReport { tabs: tabs.to_wire() });
        let Some(report) = report else { return };
        match link.send(report, Vec::new()) {
            Ok(()) => self.unsent.set(false),
            Err(err) => eprintln!("[link] window 1 could not be told this window's tabs: {err}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eitri_core::agent_backend::AgentId;
    use eitri_core::agent_bridge::SessionModeChoice;
    use eitri_core::saved_tabs::{file_name_as, load, save, save_as, Loaded, SavedTab};

    use crate::linked_window::tests::Scratch;

    fn root() -> PathBuf {
        PathBuf::from("/p")
    }

    fn c(number: u8) -> WindowName {
        WindowName::Chat(number)
    }

    fn tab(session: &str) -> SavedTab {
        SavedTab {
            agent: AgentId::Claude,
            conversation_id: "0123456789abcdef".to_string(),
            provider_session_id: session.to_string(),
            name: None,
            mode: SessionModeChoice::Auto,
            view: None,
            window: None,
            turn_running: false,
        }
    }

    fn tabs(sessions: &[&str], active: usize) -> SavedTabs {
        SavedTabs::from_parts(sessions.iter().map(|s| tab(s)).collect(), active, Vec::new(), None)
    }

    fn sessions(saved: &SavedTabs) -> Vec<&str> {
        saved.tabs.iter().map(|t| t.provider_session_id.as_str()).collect()
    }

    fn on_disk(dir: &Path) -> SavedTabs {
        match load(dir, &root()) {
            Loaded::Saved(saved) => saved,
            other => panic!("{other:?}"),
        }
    }

    fn given(group: &GroupTabs, name: &WindowName) -> SavedTabs {
        match group.given(name) {
            Some(Msg::TabsGiven { tabs, adopt: false }) => SavedTabs::from_wire(&tabs).unwrap(),
            other => panic!("{other:?}"),
        }
    }

    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn window_1_merges_every_chat_windows_tabs_into_one_file() {
        let dir = Scratch::new("group-tabs-merge");
        let mut group = GroupTabs::read(Some(&dir.0), &root(), true);
        assert!(group.windows().is_empty());
        group.reported(&c(2), &tabs(&["b-1", "b-2"], 1).to_wire());
        group.reported(&c(1), &tabs(&["a"], 0).to_wire());
        group.reported(&WindowName::Page("markdown".into()), &tabs(&["no"], 0).to_wire());
        assert!(group.wait_written());
        let file = on_disk(&dir.0);
        assert_eq!(sessions(&file), ["a", "b-1", "b-2"]);
        assert_eq!(file.tabs[1].window, Some(2));
        assert_eq!(file.tabs[2].view, Some(2), "the tab on screen in c2 is its view");
        assert_eq!(
            file.active_tab().unwrap().provider_session_id,
            "a",
            "c1's tab on screen"
        );
        assert_eq!(
            names_in(&dir.0),
            [file_name_as(&root(), Role::Main)],
            "one file for the group, and nothing else in the directory"
        );

        // A report of no tabs takes the window out, and a set that says nothing changes nothing.
        group.reported(&c(1), &SavedTabs::default().to_wire());
        group.reported(&c(2), &serde_json::json!({"tabs": 7}));
        assert!(group.wait_written());
        assert_eq!(sessions(&on_disk(&dir.0)), ["b-1", "b-2"]);
        assert_eq!(group.windows(), [c(2)]);
    }

    #[test]
    fn a_chat_window_is_given_only_its_own_tabs() {
        let dir = Scratch::new("group-tabs-given");
        let parts = [(c(1), tabs(&["a-1", "a-2"], 1)), (c(3), tabs(&["c-1", "c-2"], 1))];
        let file = saved_tabs::merge_windows(&parts, Some(&c(1)));
        save(&dir.0, &root(), &file).unwrap();

        let mut group = GroupTabs::read(Some(&dir.0), &root(), false);
        assert_eq!(group.windows(), [c(1), c(3)], "each opens at the start");
        let one = given(&group, &c(1));
        assert_eq!(sessions(&one), ["a-1", "a-2"]);
        assert_eq!(one.active, Some(1));
        let three = given(&group, &c(3));
        assert_eq!(sessions(&three), ["c-1", "c-2"]);
        assert_eq!(three.active, Some(1));
        assert!(three.tabs.iter().all(|t| t.view.is_none() && t.window.is_none()));
        assert_eq!(group.given(&c(2)), None, "nothing for a window with no tabs");
        assert_eq!(group.given(&WindowName::Page("markdown".into())), None);

        group.reported(&c(3), &tabs(&["c-3"], 0).to_wire());
        assert_eq!(
            sessions(&given(&group, &c(3))),
            ["c-3"],
            "the last report, not the file"
        );
        assert_eq!(
            sessions(&given(&group, &c(1))),
            ["a-1", "a-2"],
            "and another window's stays"
        );
    }

    #[test]
    fn a_launch_that_changes_nothing_leaves_the_file_alone() {
        let dir = Scratch::new("group-tabs-unchanged");
        save(&dir.0, &root(), &tabs(&["a"], 0)).unwrap();
        let path = dir.0.join(file_name_as(&root(), Role::Main));
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        let mut group = GroupTabs::read(Some(&dir.0), &root(), false);
        group.reported(&c(1), &tabs(&["a"], 0).to_wire());
        assert!(group.wait_written());
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), before);
    }

    #[test]
    fn a_clean_close_drops_that_windows_tabs() {
        let dir = Scratch::new("group-tabs-close");
        save(
            &dir.0,
            &root(),
            &saved_tabs::merge_windows(&[(c(1), tabs(&["a"], 0)), (c(2), tabs(&["b"], 0))], None),
        )
        .unwrap();
        let mut group = GroupTabs::read(Some(&dir.0), &root(), false);
        group.window_closed(&c(2));
        assert!(group.wait_written());
        assert_eq!(sessions(&on_disk(&dir.0)), ["a"]);
        assert_eq!(group.given(&c(2)), None, "its tabs are not reopened");

        // The last one going empties the file: nothing of it is reopened either.
        group.window_closed(&c(1));
        assert!(group.wait_written());
        assert!(on_disk(&dir.0).is_empty());
    }

    /// Windows that go away while window 1 closes are not the user closing tabs: the next launch wants them.
    #[test]
    fn a_closing_window_1_keeps_what_it_has() {
        let dir = Scratch::new("group-tabs-closing");
        let mut group = GroupTabs::read(Some(&dir.0), &root(), false);
        group.reported(&c(2), &tabs(&["b"], 0).to_wire());
        let done = group.finish().expect("a write was owed");
        assert!(done.recv_timeout(Duration::from_secs(5)).is_ok());
        group.window_closed(&c(2));
        group.reported(&c(1), &tabs(&["late"], 0).to_wire());
        assert_eq!(sessions(&on_disk(&dir.0)), ["b"]);
    }

    /// Between window 1's asking for last tabs and its finishing the file, reports still count and no window's going
    /// drops anything.
    #[test]
    fn a_window_that_goes_while_its_group_closes_stays_in_the_file() {
        let dir = Scratch::new("group-tabs-last");
        let mut group = GroupTabs::read(Some(&dir.0), &root(), false);
        group.reported(&c(1), &tabs(&["a"], 0).to_wire());
        group.reported(&c(2), &tabs(&["b"], 0).to_wire());
        group.keep_every_window();
        group.reported(&c(1), &tabs(&["a", "late"], 0).to_wire());
        group.window_closed(&c(2));
        assert!(group.wait_written());
        assert_eq!(sessions(&on_disk(&dir.0)), ["a", "late", "b"]);
    }

    /// With no write owed no writer ever started, and what happens after the close must not start one.
    #[test]
    fn a_closing_window_1_that_owed_no_write_leaves_the_file_as_it_is() {
        let dir = Scratch::new("group-tabs-closing-idle");
        let parts = [(c(1), tabs(&["a"], 0)), (c(2), tabs(&["b"], 0))];
        save(&dir.0, &root(), &saved_tabs::merge_windows(&parts, None)).unwrap();
        let mut group = GroupTabs::read(Some(&dir.0), &root(), false);
        assert!(group.finish().is_none(), "nothing was owed");
        group.window_closed(&c(2));
        group.reported(&c(1), &tabs(&["late"], 0).to_wire());
        assert!(group.wait_written());
        assert_eq!(sessions(&on_disk(&dir.0)), ["a", "b"]);
    }

    #[test]
    fn a_hub_offers_window_2s_saved_tabs_into_c1() {
        let dir = Scratch::new("group-tabs-offer");
        let viewed = SavedTab {
            view: Some(4),
            ..tab("w2-viewed")
        };
        let second = SavedTabs::from_parts(vec![tab("w2"), viewed, tab("mine")], 0, Vec::new(), None);
        save_as(&dir.0, &root(), Role::Second, &second).unwrap();
        save_as(&dir.0, &root(), Role::Unpaired, &tabs(&["u"], 0)).unwrap();
        let mine = saved_tabs::merge_windows(&[(c(3), tabs(&["mine"], 0))], None);
        save(&dir.0, &root(), &mine).unwrap();
        let w2 = file_name_as(&root(), Role::Second);
        let u = file_name_as(&root(), Role::Unpaired);

        let mut group = GroupTabs::read(Some(&dir.0), &root(), true);
        assert_eq!(group.windows(), [c(1), c(3)], "c1 opens for them");
        let first = given(&group, &c(1));
        assert_eq!(
            sessions(&first),
            ["w2", "w2-viewed", "u"],
            "a tab this file has already stays where it is"
        );
        assert!(
            first.tabs.iter().all(|t| t.view.is_none()),
            "they come back in c1 alone"
        );
        assert_eq!(sessions(&given(&group, &c(3))), ["mine"]);
        let names = names_in(&dir.0);
        assert!(names.contains(&format!("{w2}.offered")), "claimed: {names:?}");
        assert!(names.contains(&format!("{u}.offered")), "claimed: {names:?}");

        // Claimed until the file holds them, as a window that takes another's tabs back does.
        group.reported(&c(1), &first.to_wire());
        assert!(group.wait_written());
        assert_eq!(sessions(&on_disk(&dir.0)), ["w2", "w2-viewed", "u", "mine"]);
        let names = names_in(&dir.0);
        assert!(names.contains(&format!("{w2}.taken")), "{names:?}");
        assert!(names.contains(&format!("{u}.taken")), "{names:?}");
    }

    #[test]
    fn a_hub_with_nothing_to_offer_leaves_the_other_files_where_they_are() {
        let dir = Scratch::new("group-tabs-no-offer");
        save_as(&dir.0, &root(), Role::Second, &SavedTabs::default()).unwrap();
        save_as(&dir.0, &root(), Role::Unpaired, &tabs(&["u"], 0)).unwrap();
        let before = names_in(&dir.0);

        // An unpaired window may still run: its file is only offered to a window alone on the project.
        let group = GroupTabs::read(Some(&dir.0), &root(), false);
        assert!(group.windows().is_empty());
        assert_eq!(
            names_in(&dir.0),
            before,
            "an empty file goes back, a live window's is not touched"
        );
    }

    #[test]
    fn with_no_state_directory_nothing_is_read_or_written() {
        let mut group = GroupTabs::read(None, &root(), true);
        group.reported(&c(1), &tabs(&["a"], 0).to_wire());
        assert_eq!(group.windows(), [c(1)]);
        assert!(group.finish().is_none());
    }

    #[test]
    fn the_windows_that_hold_tabs_open_after_the_layouts() {
        assert_eq!(
            beside_the_layout(vec![c(2), c(1)], vec![c(1), c(3)]),
            [c(2), c(1), c(3)]
        );
    }
}
