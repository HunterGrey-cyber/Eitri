//! The pair's files as a window uses them: which layout and tabs files a window opens with and writes, what the
//! pair file decides at restore, and the pair file's writer.
//!
//! **Each window its own files.** Window 1 (or a window alone) keeps today's names, so an older build still
//! reads them; window 2 writes `-w2` files; a window that runs unpaired writes `-u` files, reading window 1's
//! layout once, read-only, while it has none of its own. No window writes another's file.
//!
//! **The pair file is the authority at restore.** Window 1's own layout file keeps its `away` placeholders as
//! leaves, but an older build may have written it since, dropping them: a module the pair file gives to window 2
//! is away in window 1 whatever window 1's file says, and one it gives to window 1 is built there even if the file
//! lost its leaf. When window 2 does not come back, window 1 takes back everything the pair file gave it, and its
//! layout records that at once; the pair file keeps naming what window 2 held until window 1's layout has been
//! written at its close (or a window 2 says what it holds), so a launch that ends before that write takes it all
//! back again and nothing is left where no window reaches it.
//!
//! **The pair file is written at once**, not behind the layout's comparison: on window 2's open and close, on
//! every finished move, on every monitor change, by the window that holds the pair (window 1 while linked,
//! window 2 while it is orphaned; window 1 stops when the link breaks without a goodbye, since window 2 may still
//! run). A write is a whole file renamed into place, on a thread of its own. A pair file that cannot be used is
//! set aside as `<name>.unusable` before anything is written over it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use eitri_core::layout::rules::{OpenPoint, NOMINAL_GRID};
use eitri_core::layout::workspace::Workspaces;
use eitri_core::layout::{open_by_rule, Layout, ModuleDecl, ModuleId, ModuleKind, OpenRule, OpenRules};
use eitri_core::saved_tabs::TabFiles;
use eitri_core::window_link::pair::{pair_file_name, read_pair, write_pair, Monitor, PairFile, ReadPair, Role};

/// The holder name of window 1 and of window 2 in the pair file.
pub const WINDOW_1: &str = "1";
pub const WINDOW_2: &str = "2";

/// What the pair-lock holder does with its layout at start, from the pair file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PairRestore {
    /// Held by window 2, which comes back: placeholders here.
    pub away: BTreeSet<ModuleId>,
    /// This window's, built here even when its own file lost the leaf.
    pub build_here: BTreeSet<ModuleId>,
    /// Window 2 does not come back but held something, or was open: its modules come here, and its saved tabs
    /// are offered after this window's own.
    pub take_back: bool,
    /// What the pair file is to say from now on; written at once when it differs from what was read.
    pub start: PairFile,
    /// Whether `start` differs from the file as it was read.
    pub write_now: bool,
    /// An unusable pair file is in the way and could not be set aside: nothing is written over it.
    pub in_the_way: bool,
    pub notes: Vec<String>,
}

/// [`main_restore`] of the pair file in `layout_dir`, an unusable one first moved, whole, to `<name>.unusable`
/// beside it, for a person to read, as the layout and tabs files' are.
pub fn read_for_restore(layout_dir: &Path, project_root: &Path, window2_restored: bool) -> PairRestore {
    let read = read_pair(layout_dir, project_root);
    let mut plan = main_restore(&read, window2_restored);
    if matches!(read, ReadPair::Unusable(_)) {
        let path = layout_dir.join(pair_file_name(project_root));
        let aside = path.with_extension("json.unusable");
        match std::fs::rename(&path, &aside) {
            Ok(()) => plan.notes.push(format!(
                "[window] the unusable pair file is kept as {}",
                aside.display()
            )),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                plan.notes.push(format!(
                    "[window] the unusable pair file could not be set aside ({err}); it is not written over"
                ));
                plan.write_now = false;
                plan.in_the_way = true;
            }
        }
    }
    plan
}

/// The restore the pair file asks of window 1. `window2_restored`: window 2 is started again at this launch
/// (its file usable, `window.restore` not false, and it was open when the pair last ended); otherwise window 1
/// takes back whatever the pair file gave to window 2.
pub fn main_restore(read: &ReadPair, window2_restored: bool) -> PairRestore {
    let mut plan = PairRestore::default();
    let file = match read {
        ReadPair::Missing => return plan,
        ReadPair::Unusable(why) => {
            plan.notes.push(format!(
                "[window] the pair file is unusable ({why}); window 1 holds everything"
            ));
            plan.write_now = true;
            return plan;
        }
        ReadPair::Read(file) => file.clone(),
    };
    let mut holders: BTreeMap<ModuleId, String> = BTreeMap::new();
    for (text, holder) in &file.holders {
        match ModuleId::parse(text) {
            Ok(id) if holder == WINDOW_1 || holder == WINDOW_2 => {
                holders.insert(id, holder.clone());
            }
            _ => plan.notes.push(format!(
                "[window] the pair file names {text:?} held by {holder:?}; left out"
            )),
        }
    }
    plan.start = file.clone();
    if window2_restored && file.window2_open {
        for (id, holder) in holders {
            if holder == WINDOW_2 {
                plan.away.insert(id);
            } else {
                plan.build_here.insert(id);
            }
        }
    } else {
        plan.take_back = file.window2_open || holders.values().any(|h| h == WINDOW_2);
        plan.build_here = holders.into_keys().collect();
        // Window 2's holders stay named until window 1's layout holds what came back
        // ([`PairWriter::forget_window2_holders`]): a launch that ends before then takes it all back again.
        plan.start.window2_open = false;
        if plan.take_back {
            plan.notes.push(
                "[window] window 2 is not restored: its modules are built here and its saved tabs offered after this \
                 window's own"
                    .to_string(),
            );
        }
    }
    plan.write_now = plan.start != file;
    plan
}

/// Window 1's layouts as the pair file leaves them: exactly `plan.away` held away, every other away mark the file
/// had taken off, and each module of `plan.build_here` this window lacks placed by its open rules, kept off the
/// keys, then given to every workspace hidden. `can_build` says which modules this window has a host for; one it
/// cannot build is left out, noted.
pub fn apply_main_restore(
    plan: &PairRestore,
    layout: &mut Layout,
    workspaces: &mut Workspaces,
    rules: &OpenRules,
    decls: &[ModuleDecl],
    can_build: &dyn Fn(&ModuleId) -> bool,
) -> Vec<String> {
    let mut notes = Vec::new();
    for id in super::away_everywhere(layout, workspaces) {
        if !plan.away.contains(&id) {
            super::hold_away(layout, workspaces, &id, false);
        }
    }
    let present: BTreeSet<ModuleId> = workspaces
        .each(layout)
        .into_iter()
        .flat_map(|(_, l)| l.modules())
        .collect();
    for id in &plan.away {
        if present.contains(id) {
            super::hold_away(layout, workspaces, id, true);
        }
    }
    let mut placed_any = false;
    for id in plan.build_here.iter().filter(|id| !present.contains(*id)) {
        if !can_build(id) {
            notes.push(format!(
                "[window] '{id}' was in window 2 and this window cannot build it; left out"
            ));
            continue;
        }
        let keys = layout.focus().clone();
        let position = decls.iter().find(|d| d.id == *id).map(|d| d.placement);
        let (rule, _) = rules.resolve(id, OpenPoint::Reopen, position);
        let rule = OpenRule { focus: false, ..rule };
        match open_by_rule(layout, id, &rule, Some(&keys), NOMINAL_GRID) {
            Ok(_) => {
                placed_any = true;
                notes.push(format!(
                    "[window] '{id}' is back from window 2, placed by its open rule"
                ));
            }
            Err(err) => notes.push(format!("[window] '{id}' could not be placed here: {err}")),
        }
    }
    if placed_any {
        notes.extend(workspaces.fit_all_with(layout, decls));
    }
    notes
}

/// Window 2's start: every module of its layouts that is not one every window has of its own (the terminal and
/// the chat; the editor is retired) and that the pair gives to window 2 (`given`) held away, since window 1 holds
/// it, or no window does and window 1 will be asked for it.
pub fn second_start_away(layout: &mut Layout, workspaces: &mut Workspaces, given: &BTreeSet<ModuleId>) {
    let modules: BTreeSet<ModuleId> = workspaces
        .each(layout)
        .into_iter()
        .flat_map(|(_, l)| l.modules())
        .collect();
    for id in modules {
        let own = id == ModuleId::terminal() || id == ModuleId::agent() || id == ModuleId::editor();
        let held_elsewhere = id.kind() == ModuleKind::LuaWebview || id.number() >= 2;
        if !own && held_elsewhere && !given.contains(&id) {
            super::hold_away(layout, workspaces, &id, true);
        }
    }
}

/// Which saved tabs a window offers and writes. Window 1 offers window 2's after its own when window 2 does not
/// come back, and an unpaired window's when it is alone on the project; window 2 started by `window.new`, not by
/// a restore, offers none (its earlier tabs were window 1's to take back) and writes its own file.
pub fn tab_files(role: Role, take_back_second: bool, alone_on_project: bool, restored: bool) -> TabFiles {
    let mut also_offer = Vec::new();
    if role == Role::Main {
        if take_back_second {
            also_offer.push(Role::Second);
        }
        if alone_on_project {
            also_offer.push(Role::Unpaired);
        }
    }
    TabFiles {
        role,
        also_offer,
        offers_own: role != Role::Second || restored,
    }
}

/// Whether no other window of the project holds its presence lock (`windows/<pid>` in the project's review
/// directory): the pair's window is then alone, and offers what unpaired windows left. Not knowing is not being
/// alone.
pub fn alone_on_project(review_dir: Option<&Path>) -> bool {
    let Some(dir) = review_dir else { return false };
    matches!(
        eitri_core::turn_review::presence::busy_elsewhere(dir, std::process::id()),
        Ok(None)
    )
}

/// How long [`PairWriter::hand_over`] waits for a write already running: the window's own thread waits.
const HAND_OVER_WAIT: Duration = Duration::from_millis(250);

/// What the writer thread and the window share.
#[derive(Default)]
struct Slot {
    next: Option<PairFile>,
    busy: bool,
    closed: bool,
    /// The next write first reads the file on disk for what this window never knew of (window 1's monitor, for an
    /// orphaned window 2), and every write from then on keeps it.
    inherit: bool,
    #[cfg(test)]
    written: usize,
}

struct Shared {
    slot: Mutex<Slot>,
    changed: Condvar,
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, Slot> {
        self.slot.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The pair file kept as the pair is now. Each change that is a trigger writes it at once, on the writer's
/// thread, the newest file replacing one not yet written; a window that does not hold the pair (window 2 while
/// linked) keeps the file's state without writing it, ready to take over.
pub struct PairWriter {
    dir: PathBuf,
    root: PathBuf,
    file: PairFile,
    writing: bool,
    shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl PairWriter {
    /// `file` is what the pair file says now; `writing` whether this window writes it.
    pub fn new(layout_dir: &Path, project_root: &Path, file: PairFile, writing: bool) -> PairWriter {
        PairWriter {
            dir: layout_dir.to_path_buf(),
            root: project_root.to_path_buf(),
            file,
            writing,
            shared: Arc::new(Shared {
                slot: Mutex::default(),
                changed: Condvar::new(),
            }),
            thread: None,
        }
    }

    #[cfg(test)]
    pub fn file(&self) -> &PairFile {
        &self.file
    }

    /// From now on this window writes the pair file (window 2 once orphaned), starting with the file as it is
    /// and the monitors the file on disk names that this window never heard of.
    pub fn take_over(&mut self) {
        self.writing = true;
        self.shared.lock().inherit = true;
        self.write();
    }

    /// The link broke without a goodbye: the other window may still run and, orphaned, write the pair file
    /// itself, so this one writes it no more. If it died instead, the file still names what it held, and the next
    /// launch takes that back. A write still queued is dropped, since it holds this window's view from before the
    /// link ended and would land over the other window's; one already running is waited for, briefly (a small
    /// file renamed into place), so it lands before the other window's first write rather than after.
    pub fn hand_over(&mut self) {
        self.writing = false;
        self.shared.lock().next = None;
        if !self.flush(HAND_OVER_WAIT) {
            eprintln!("[window] the pair file was still being written as the other window took it over");
        }
    }

    /// A window 2 linked again after [`PairWriter::hand_over`] (one started anew once the old one was gone):
    /// this window writes the pair file once more, keeping the monitors the file names that it never heard of
    /// (an orphaned window 2 may have written them). Nothing changes for a window that never stopped writing.
    pub fn resume(&mut self) {
        if self.writing {
            return;
        }
        self.writing = true;
        self.shared.lock().inherit = true;
    }

    /// Window 1's layout now holds what it took back at its launch (written at the close): the pair file forgets
    /// what the window 2 that is not open held. Nothing to do while window 2 is open, or held nothing.
    pub fn forget_window2_holders(&mut self) {
        if self.file.window2_open || !self.file.holders.values().any(|holder| holder == WINDOW_2) {
            return;
        }
        self.file.holders.retain(|_, holder| holder != WINDOW_2);
        self.write();
    }

    /// Window 2 opened: written at once.
    pub fn window2_opened(&mut self) {
        self.file.window2_open = true;
        self.write();
    }

    /// Window 2 closed: it holds nothing any more, and the next launch does not start it.
    pub fn window2_closed(&mut self) {
        self.file.window2_open = false;
        self.file.holders.retain(|_, holder| holder != WINDOW_2);
        self.write();
    }

    /// Window 2 closed on its own while it kept the pair file (orphaned): the pair ends and the next launch does
    /// not start it again, while what it held stays named, for window 1 to take back.
    pub fn window2_left(&mut self) {
        self.file.window2_open = false;
        self.write();
    }

    /// A move finished: `id` is held by window `to` now. A module back in window 1 is not named.
    #[cfg_attr(not(test), allow(dead_code))] // The move verbs report here once they move anything.
    pub fn move_done(&mut self, id: &ModuleId, to: &str) {
        if to == WINDOW_1 {
            self.file.holders.remove(id.as_str());
        } else {
            self.file.holders.insert(id.as_str().to_string(), to.to_string());
        }
        self.write();
    }

    /// What window 2 holds, as it told window 1: every module it names that only one window can have (a numbered
    /// instance, a page) is held by window 2, and no other is. The terminal and the chat every window has of its
    /// own are no one's to hold. Written when that changes what the file says.
    pub fn window2_holds(&mut self, ids: &BTreeSet<String>) {
        let mut holders = self.file.holders.clone();
        holders.retain(|_, holder| holder != WINDOW_2);
        for text in ids {
            let Ok(id) = ModuleId::parse(text) else { continue };
            if id.kind() == ModuleKind::LuaWebview || id.number() >= 2 {
                holders.insert(text.clone(), WINDOW_2.to_string());
            }
        }
        if holders != self.file.holders {
            self.file.holders = holders;
            self.write();
        }
    }

    /// Window `window` is on `monitor` now: written at once when that is news.
    #[cfg_attr(not(test), allow(dead_code))] // Each window's monitor reports come here once they are made.
    pub fn monitor(&mut self, window: &str, monitor: Monitor) {
        if self.file.monitors.get(window) == Some(&monitor) {
            return;
        }
        self.file.monitors.insert(window.to_string(), monitor);
        self.write();
    }

    /// Writes the file as it is now when this window holds the pair; for a restore that changed it.
    pub fn write_now(&mut self) {
        self.write();
    }

    fn write(&mut self) {
        if !self.writing {
            return;
        }
        self.start_thread();
        self.shared.lock().next = Some(self.file.clone());
        self.shared.changed.notify_all();
    }

    fn start_thread(&mut self) {
        if self.thread.is_some() {
            return;
        }
        let (shared, dir, root) = (self.shared.clone(), self.dir.clone(), self.root.clone());
        let spawned = std::thread::Builder::new()
            .name("pair-file-writer".into())
            .spawn(move || write_loop(&shared, &dir, &root));
        match spawned {
            Ok(thread) => self.thread = Some(thread),
            Err(err) => {
                eprintln!("[window] could not start the pair file's writer ({err}); the pair file is not kept");
                self.writing = false;
            }
        }
    }

    /// Waits up to `bound` for every write handed over; whether they all finished.
    pub fn flush(&self, bound: Duration) -> bool {
        let deadline = Instant::now() + bound;
        let mut slot = self.shared.lock();
        while slot.next.is_some() || slot.busy {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            slot = self
                .shared
                .changed
                .wait_timeout(slot, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        true
    }

    #[cfg(test)]
    fn writes(&self) -> usize {
        self.shared.lock().written
    }
}

impl Drop for PairWriter {
    fn drop(&mut self) {
        // What was handed over is still written: the thread ends once nothing is left.
        self.shared.lock().closed = true;
        self.shared.changed.notify_all();
    }
}

fn write_loop(shared: &Shared, dir: &Path, root: &Path) {
    let mut inherited: BTreeMap<String, Monitor> = BTreeMap::new();
    loop {
        let (mut file, inherit) = {
            let mut slot = shared.lock();
            loop {
                if let Some(file) = slot.next.take() {
                    slot.busy = true;
                    break (file, std::mem::take(&mut slot.inherit));
                }
                if slot.closed {
                    return;
                }
                slot = shared.changed.wait(slot).unwrap_or_else(|e| e.into_inner());
            }
        };
        if inherit {
            if let ReadPair::Read(on_disk) = read_pair(dir, root) {
                inherited = on_disk.monitors;
            }
        }
        for (window, monitor) in &inherited {
            file.monitors.entry(window.clone()).or_insert_with(|| monitor.clone());
        }
        if let Err(err) = write_pair(dir, root, &file) {
            eprintln!("[window] could not write the pair file: {err}");
        }
        let mut slot = shared.lock();
        slot.busy = false;
        #[cfg(test)]
        {
            slot.written += 1;
        }
        drop(slot);
        shared.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linked_window::tests::{id, two_workspaces, window, Scratch};
    use eitri_core::layout::persist::{self, Loaded};
    use eitri_core::window_link::pair::{pair_file_name, read_pair};

    fn root() -> PathBuf {
        PathBuf::from("/home/someone/project")
    }

    fn bytes(path: &Path) -> Option<Vec<u8>> {
        std::fs::read(path).ok()
    }

    const BOUND: Duration = Duration::from_secs(5);

    #[test]
    fn the_pair_file_is_written_by_a_monitor_change_alone() {
        let dir = Scratch::new("pair-monitor");
        let mut writer = PairWriter::new(&dir.0, &root(), PairFile::default(), true);
        writer.window2_opened();
        assert!(writer.flush(BOUND));
        let ReadPair::Read(opened) = read_pair(&dir.0, &root()) else {
            panic!("written on window 2's open")
        };
        assert!(opened.window2_open);
        // Nothing of either layout changes; only window 2 goes to another monitor.
        let before = writer.writes();
        writer.monitor(
            WINDOW_2,
            Monitor {
                connector: "DP-2".into(),
                description: "Dell".into(),
            },
        );
        assert!(writer.flush(BOUND));
        assert_eq!(writer.writes(), before + 1, "a monitor change writes the pair file");
        let ReadPair::Read(moved) = read_pair(&dir.0, &root()) else {
            panic!()
        };
        assert_eq!(moved.monitors[WINDOW_2].connector, "DP-2");
        assert!(moved.window2_open, "and the next launch restores window 2 there");
        // The same monitor again is no news.
        writer.monitor(
            WINDOW_2,
            Monitor {
                connector: "DP-2".into(),
                description: "Dell".into(),
            },
        );
        assert!(writer.flush(BOUND));
        assert_eq!(writer.writes(), before + 1);
    }

    #[test]
    fn a_move_and_window_2s_close_write_the_pair_file_and_a_window_not_holding_it_writes_nothing() {
        let dir = Scratch::new("pair-moves");
        let mut writer = PairWriter::new(&dir.0, &root(), PairFile::default(), true);
        writer.window2_opened();
        writer.move_done(&id("terminal#2"), WINDOW_2);
        assert!(writer.flush(BOUND));
        let ReadPair::Read(file) = read_pair(&dir.0, &root()) else {
            panic!()
        };
        assert_eq!(file.holders["terminal#2"], WINDOW_2);
        writer.move_done(&id("terminal#2"), WINDOW_1);
        writer.move_done(&id("lua:notes"), WINDOW_2);
        writer.window2_closed();
        assert!(writer.flush(BOUND));
        let ReadPair::Read(file) = read_pair(&dir.0, &root()) else {
            panic!()
        };
        assert!(!file.window2_open && file.holders.is_empty(), "{file:?}");

        let other = Scratch::new("pair-not-holder");
        let mut second = PairWriter::new(&other.0, &root(), PairFile::default(), false);
        second.window2_opened();
        second.move_done(&id("terminal#3"), WINDOW_2);
        assert!(second.flush(BOUND));
        assert_eq!(
            read_pair(&other.0, &root()),
            ReadPair::Missing,
            "window 2 writes only once orphaned"
        );
        second.take_over();
        assert!(second.flush(BOUND));
        let ReadPair::Read(file) = read_pair(&other.0, &root()) else {
            panic!()
        };
        assert_eq!(file.holders["terminal#3"], WINDOW_2);
    }

    #[test]
    fn window_2s_own_terminal_and_chat_are_held_by_no_one() {
        let dir = Scratch::new("pair-holds");
        let mut writer = PairWriter::new(&dir.0, &root(), PairFile::default(), true);
        let ids = ["terminal", "agent", "terminal#3", "lua:notes"]
            .map(String::from)
            .into_iter()
            .collect();
        writer.window2_holds(&ids);
        assert!(writer.flush(BOUND));
        assert_eq!(
            writer.file().holders.keys().map(String::as_str).collect::<Vec<_>>(),
            ["lua:notes", "terminal#3"]
        );
        let before = writer.writes();
        writer.window2_holds(&ids);
        assert!(writer.flush(BOUND));
        assert_eq!(writer.writes(), before, "the same set again is not written");
    }

    fn pair_with(window2_open: bool, holders: &[(&str, &str)]) -> ReadPair {
        let mut file = PairFile {
            window2_open,
            ..PairFile::default()
        };
        for (id, holder) in holders {
            file.holders.insert(id.to_string(), holder.to_string());
        }
        ReadPair::Read(file)
    }

    fn can_build_all(_: &ModuleId) -> bool {
        true
    }

    #[test]
    fn restore_follows_holders_over_a_main_file_an_older_build_wrote() {
        // Window 1's file as an older build left it: it knows no pair, so no away mark, every module built,
        // and the plugin page is gone from its trees.
        let older = window(&["terminal#2"]);
        assert!(older.away().is_empty());
        let mut layout = older.clone();
        let mut workspaces = two_workspaces(older);
        let read = pair_with(
            true,
            &[("terminal#2", WINDOW_2), ("lua:notes", WINDOW_1), ("bogus#", WINDOW_2)],
        );
        let plan = main_restore(&read, true);
        assert_eq!(plan.away, BTreeSet::from([id("terminal#2")]));
        assert_eq!(plan.build_here, BTreeSet::from([id("lua:notes")]));
        assert!(!plan.take_back && !plan.write_now);
        assert!(plan.notes.iter().any(|n| n.contains("bogus")), "{:?}", plan.notes);
        let decls = vec![ModuleDecl {
            id: id("lua:notes"),
            placement: eitri_core::layout::Placement::RightOfRoot,
        }];
        apply_main_restore(
            &plan,
            &mut layout,
            &mut workspaces,
            &OpenRules::default(),
            &decls,
            &can_build_all,
        );
        for (name, l) in workspaces.each(&layout) {
            assert!(
                l.is_away(&id("terminal#2")),
                "{name}: window 2's module is away whatever the file said"
            );
            assert!(
                l.contains(&id("lua:notes")),
                "{name}: window 1's module is built though its leaf was lost"
            );
            assert!(!l.is_away(&id("lua:notes")), "{name}");
        }
        assert!(
            layout.is_shown(&id("lua:notes")),
            "placed by its open rule in the current workspace"
        );
        assert_eq!(layout.focus(), &ModuleId::editor(), "and it does not take the keys");
    }

    #[test]
    fn an_away_mark_the_pair_file_does_not_back_is_taken_off() {
        let mut layout = window(&["terminal#2"]);
        layout.set_away(&id("terminal#2"), true).unwrap();
        let mut workspaces = Workspaces::single();
        let plan = main_restore(&ReadPair::Missing, false);
        apply_main_restore(
            &plan,
            &mut layout,
            &mut workspaces,
            &OpenRules::default(),
            &[],
            &can_build_all,
        );
        assert!(!layout.is_away(&id("terminal#2")), "no pair file holds it for window 2");
    }

    #[test]
    fn window_restore_false_brings_window_2s_modules_and_tabs_to_window_1() {
        let stored = {
            let mut l = window(&["terminal#2", "lua:notes"]);
            l.set_away(&id("lua:notes"), true).unwrap();
            l
        };
        let mut layout = stored.clone();
        let mut workspaces = two_workspaces(stored);
        let read = pair_with(true, &[("lua:notes", WINDOW_2), ("terminal#5", WINDOW_2)]);
        // `window.restore` false: window 2 is not started again.
        let plan = main_restore(&read, false);
        assert!(plan.take_back && plan.write_now);
        assert!(plan.away.is_empty());
        assert!(!plan.start.window2_open, "{:?}", plan.start);
        assert_eq!(
            plan.start.holders.len(),
            2,
            "what window 2 held stays named until window 1's layout holds it"
        );
        apply_main_restore(
            &plan,
            &mut layout,
            &mut workspaces,
            &OpenRules::default(),
            &[],
            &can_build_all,
        );
        for (name, l) in workspaces.each(&layout) {
            assert!(!l.is_away(&id("lua:notes")), "{name}: the placeholder is un-retired");
            assert!(
                l.contains(&id("terminal#5")),
                "{name}: a module only window 2 had is built here"
            );
            assert!(l.away().is_empty(), "{name}");
        }
        // Its saved tabs are offered after window 1's own.
        let files = tab_files(Role::Main, plan.take_back, false, false);
        assert_eq!(files.also_offer, vec![Role::Second]);
        assert!(files.offers_own);
        // The launch's write says window 2 is not open and still names what it held: a launch that ends before
        // window 1's layout is written takes it all back again.
        let dir = Scratch::new("pair-take-back");
        let mut writer = PairWriter::new(&dir.0, &root(), plan.start.clone(), true);
        writer.write_now();
        assert!(writer.flush(BOUND));
        let ReadPair::Read(file) = read_pair(&dir.0, &root()) else {
            panic!()
        };
        assert!(!file.window2_open && file.holders.len() == 2, "{file:?}");
        let again = main_restore(&ReadPair::Read(file), false);
        assert!(again.take_back && again.build_here.contains(&id("terminal#5")));
        // Once window 1's layout is written at its close, the pair file says window 2 holds nothing.
        writer.forget_window2_holders();
        assert!(writer.flush(BOUND));
        let ReadPair::Read(file) = read_pair(&dir.0, &root()) else {
            panic!()
        };
        assert!(file.holders.is_empty() && !file.window2_open);
    }

    #[test]
    fn a_take_back_is_a_change_written_to_window_ones_layout_at_once() {
        let dir = Scratch::new("take-back-written");
        // Window 1's file lacks the instance window 2 had; nothing else of it changes at this launch.
        let mut layout = window(&[]);
        let mut workspaces = Workspaces::single();
        persist::save(&dir.0, &root(), &workspaces, &layout).unwrap();
        let plan = main_restore(&pair_with(true, &[("terminal#5", WINDOW_2)]), false);
        let before = (layout.clone(), workspaces.clone());
        apply_main_restore(
            &plan,
            &mut layout,
            &mut workspaces,
            &OpenRules::default(),
            &[],
            &can_build_all,
        );
        assert!(layout.contains(&id("terminal#5")));
        let mut saver =
            eitri_core::wm::persist::Saver::new(Some(dir.0.clone()), &root(), Vec::new(), None, &layout, &workspaces)
                .with_base(&before.0, &before.1);
        assert!(saver.submit_now(&layout, &workspaces), "the restore is a change");
        assert!(saver.save_now(&layout, &workspaces));
        let Loaded::Restored(read) = persist::load_for_role(&dir.0, &root(), Role::Main, &[], &[]) else {
            panic!("window 1's file is written")
        };
        assert!(
            read.current.contains(&id("terminal#5")),
            "the next launch finds the instance in window 1's own file"
        );
    }

    #[test]
    fn an_unusable_pair_file_is_set_aside_before_it_is_written_over() {
        let dir = Scratch::new("pair-unusable");
        let path = dir.0.join(pair_file_name(&root()));
        std::fs::create_dir_all(&dir.0).unwrap();
        std::fs::write(&path, "{ not json").unwrap();
        let plan = read_for_restore(&dir.0, &root(), false);
        assert!(plan.write_now && !plan.in_the_way);
        assert_eq!(
            std::fs::read_to_string(path.with_extension("json.unusable")).unwrap(),
            "{ not json",
            "kept whole for a person to read"
        );
        assert!(!path.exists());
        assert!(plan.notes.iter().any(|n| n.contains(".unusable")), "{:?}", plan.notes);
    }

    #[test]
    fn a_link_broken_without_goodbye_leaves_the_pair_file_to_the_orphaned_window_2() {
        let dir = Scratch::new("pair-hand-over");
        let one_monitor = Monitor {
            connector: "eDP-1".into(),
            description: "Laptop".into(),
        };
        let mut first = PairWriter::new(&dir.0, &root(), PairFile::default(), true);
        first.window2_opened();
        first.monitor(WINDOW_1, one_monitor.clone());
        assert!(first.flush(BOUND));
        // Window 2 knew nothing of window 1's monitor.
        let mut second = PairWriter::new(
            &dir.0,
            &root(),
            PairFile {
                window2_open: true,
                ..PairFile::default()
            },
            false,
        );
        // The link breaks with both windows up: window 1 stops writing, window 2 takes the file over.
        first.hand_over();
        second.window2_holds(&BTreeSet::from(["lua:notes".to_string()]));
        second.take_over();
        assert!(second.flush(BOUND));
        let before = first.writes();
        first.window2_closed();
        first.monitor(
            WINDOW_1,
            Monitor {
                connector: "DP-3".into(),
                description: "Other".into(),
            },
        );
        assert!(first.flush(BOUND));
        assert_eq!(
            first.writes(),
            before,
            "window 1 writes no more once it handed the file over"
        );
        let ReadPair::Read(file) = read_pair(&dir.0, &root()) else {
            panic!()
        };
        assert!(file.window2_open);
        assert_eq!(file.holders["lua:notes"], WINDOW_2);
        assert_eq!(
            file.monitors.get(WINDOW_1),
            Some(&one_monitor),
            "window 1's monitor is kept by window 2's writes"
        );
        // Later writes of window 2 keep it too.
        second.window2_left();
        assert!(second.flush(BOUND));
        let ReadPair::Read(file) = read_pair(&dir.0, &root()) else {
            panic!()
        };
        assert!(!file.window2_open);
        assert_eq!(file.monitors.get(WINDOW_1), Some(&one_monitor));
    }

    #[test]
    fn a_write_still_queued_when_the_link_ends_is_never_written() {
        let dir = Scratch::new("pair-hand-over-queued");
        let mut first = PairWriter::new(&dir.0, &root(), PairFile::default(), true);
        // Queued and not yet taken by a writer thread, as when the link ends between a change and its write.
        first.shared.lock().next = Some(PairFile {
            window2_open: true,
            ..PairFile::default()
        });
        first.hand_over();
        assert!(first.shared.lock().next.is_none(), "the queued write is dropped");
        drop(first);
        assert!(
            !dir.0.join(pair_file_name(&root())).exists(),
            "the other window's writes are not followed by this window's stale view"
        );
    }

    #[test]
    fn a_window_2_linked_after_a_hand_over_has_window_1_write_the_pair_file_again() {
        let dir = Scratch::new("pair-resume");
        let orphan_monitor = Monitor {
            connector: "DP-2".into(),
            description: "Dell".into(),
        };
        let mut first = PairWriter::new(&dir.0, &root(), PairFile::default(), true);
        first.window2_opened();
        assert!(first.flush(BOUND));
        first.hand_over();
        // The orphan wrote the file while it kept it, naming its monitor, then was gone.
        let mut orphan = PairWriter::new(
            &dir.0,
            &root(),
            PairFile {
                window2_open: true,
                ..PairFile::default()
            },
            false,
        );
        orphan.take_over();
        orphan.monitor(WINDOW_2, orphan_monitor.clone());
        assert!(orphan.flush(BOUND));
        drop(orphan);
        // A new window 2 links: window 1 writes the pair again, from its open on.
        first.resume();
        first.window2_opened();
        first.window2_holds(&BTreeSet::from(["lua:notes".to_string()]));
        assert!(first.flush(BOUND));
        let ReadPair::Read(file) = read_pair(&dir.0, &root()) else {
            panic!("written again")
        };
        assert!(file.window2_open);
        assert_eq!(
            file.holders["lua:notes"], WINDOW_2,
            "an ownership change after the link is written"
        );
        assert_eq!(
            file.monitors.get(WINDOW_2),
            Some(&orphan_monitor),
            "the monitor the orphan wrote is kept"
        );
        first.window2_closed();
        assert!(first.flush(BOUND));
        let ReadPair::Read(file) = read_pair(&dir.0, &root()) else {
            panic!()
        };
        assert!(
            !file.window2_open && file.holders.is_empty(),
            "and so is its clean close"
        );
    }

    #[test]
    fn window_2_started_fresh_holds_away_what_the_pair_does_not_give_it() {
        let mut layout = window(&["terminal#2", "agent#3", "lua:notes"]);
        let mut workspaces = Workspaces::single();
        second_start_away(&mut layout, &mut workspaces, &BTreeSet::from([id("agent#3")]));
        assert_eq!(layout.away(), BTreeSet::from([id("terminal#2"), id("lua:notes")]));
        assert!(!layout.is_away(&ModuleId::terminal()) && !layout.is_away(&ModuleId::agent()));
        let files = tab_files(Role::Second, false, true, false);
        assert!(files.also_offer.is_empty() && !files.offers_own);
    }

    #[test]
    fn an_unpaired_window_reads_window_ones_file_once_and_writes_only_its_own() {
        let dir = Scratch::new("unpaired-files");
        let layout_dir = dir.0.clone();
        let tabs_dir = dir.1.join("eitri").join("tabs");
        // Window 1's files: a layout with a module away for window 2, its tabs, and the pair file.
        let mut main = window(&["terminal#2"]);
        main.set_away(&id("terminal#2"), true).unwrap();
        persist::save(&layout_dir, &root(), &Workspaces::single(), &main).unwrap();
        let one_tab = eitri_core::saved_tabs::SavedTabs::from_parts(
            vec![eitri_core::saved_tabs::SavedTab {
                agent: eitri_core::agent_backend::AgentId::Claude,
                conversation_id: "0123456789abcdef0123456789abcdef".into(),
                provider_session_id: "s-main".into(),
                name: None,
                mode: eitri_core::agent_bridge::SessionModeChoice::Auto,
                view: None,
            }],
            0,
            Vec::new(),
            None,
        );
        eitri_core::saved_tabs::save(&tabs_dir, &root(), &one_tab).unwrap();
        eitri_core::window_link::pair::write_pair(&layout_dir, &root(), &PairFile::default()).unwrap();
        let main_layout = layout_dir.join(persist::file_name_v2(&root()));
        let main_tabs = tabs_dir.join(eitri_core::saved_tabs::file_name_as(&root(), Role::Main));
        let pair_path = layout_dir.join(pair_file_name(&root()));
        let before = (bytes(&main_layout), bytes(&main_tabs), bytes(&pair_path));

        // The unpaired window opens with window 1's layout, every away mark ignored.
        let Loaded::Restored(opened) = persist::load_for_role(&layout_dir, &root(), Role::Unpaired, &[], &[]) else {
            panic!("window 1's file is read once")
        };
        let (mut layout, mut workspaces) = (opened.current, opened.workspaces);
        super::super::ignore_away(&mut layout, &mut workspaces);
        assert!(layout.contains(&id("terminal#2")) && !layout.is_away(&id("terminal#2")));
        assert_eq!(
            layout.modules().iter().filter(|m| **m == id("terminal#2")).count(),
            1,
            "built once, its own instance"
        );
        // It changes its arrangement, and its saver and tab writer write.
        let mut saver = eitri_core::wm::persist::Saver::new(
            Some(layout_dir.clone()),
            &root(),
            Vec::new(),
            opened.keep_copy,
            &layout,
            &workspaces,
        );
        saver.set_role(Role::Unpaired);
        eitri_core::layout::hide(
            &mut layout,
            &ModuleId::terminal(),
            &eitri_core::layout::Frame::new(NOMINAL_GRID, 1),
        )
        .unwrap();
        assert!(saver.save_now(&layout, &workspaces));
        saver.collect(Some(&mut workspaces));
        let mut tabs = eitri_core::saved_tabs::TabWriter::new_as(Some(tabs_dir.clone()), root(), Role::Unpaired);
        tabs.observe(Some(one_tab.clone()));
        assert!(tabs.wait_written(BOUND));

        let after = (bytes(&main_layout), bytes(&main_tabs), bytes(&pair_path));
        assert_eq!(
            before, after,
            "window 1's files and the pair file are byte for byte as they were"
        );
        assert!(layout_dir
            .join(persist::file_name_v2_as(&root(), Role::Unpaired))
            .exists());
        assert!(tabs_dir
            .join(eitri_core::saved_tabs::file_name_as(&root(), Role::Unpaired))
            .exists());
        for name in std::fs::read_dir(&layout_dir).unwrap() {
            let name = name.unwrap().file_name().to_string_lossy().into_owned();
            assert!(
                !name.contains(".partial") && !name.contains(".unusable") && !name.contains("-w2"),
                "nothing else is written: {name}"
            );
        }
        // Its next start reads its own file, not window 1's.
        let Loaded::Restored(again) = persist::load_for_role(&layout_dir, &root(), Role::Unpaired, &[], &[]) else {
            panic!()
        };
        assert!(again.current.hidden().contains(&ModuleId::terminal()));
    }
}
