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
    /// Window 2 is started again at this launch: it was open when the pair last ended, `window.restore` allows it
    /// and its layout file can be used.
    pub restore_second: bool,
    pub notes: Vec<String>,
}

/// [`main_restore`] of the pair file in `layout_dir`, an unusable one first moved, whole, to `<name>.unusable`
/// beside it, for a person to read, as the layout and tabs files' are. Window 2 is started again when it was open
/// at the pair's last end, `window_restore` (`window.restore`) allows it, and `second_layout_usable` says its
/// layout file is not one it would have to set aside (asked only then).
pub fn read_for_restore(
    layout_dir: &Path,
    project_root: &Path,
    window_restore: bool,
    second_layout_usable: &dyn Fn() -> bool,
) -> PairRestore {
    let read = read_pair(layout_dir, project_root);
    let was_open = matches!(&read, ReadPair::Read(file) if file.window2_open);
    let mut refusal = None;
    if was_open && !window_restore {
        refusal = Some("[window] window.restore is off: window 2 is not started again");
    } else if was_open && !second_layout_usable() {
        refusal = Some("[window] window 2's layout file cannot be used: window 2 is not started again");
    }
    let window2_restored = was_open && refusal.is_none();
    let mut plan = main_restore(&read, window2_restored);
    plan.restore_second = window2_restored;
    plan.notes.extend(refusal.map(str::to_string));
    set_aside_unusable(layout_dir, project_root, &read, &mut plan);
    plan
}

/// An unusable pair file moved, whole, to `<name>.unusable` beside it; one that cannot be moved is not written over.
fn set_aside_unusable(layout_dir: &Path, project_root: &Path, read: &ReadPair, plan: &mut PairRestore) {
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
}

/// [`relink_restore`] of the pair file in `layout_dir`, an unusable one set aside as [`read_for_restore`] does.
pub fn read_for_relink(layout_dir: &Path, project_root: &Path, holders: &BTreeMap<String, String>) -> PairRestore {
    let read = read_pair(layout_dir, project_root);
    let mut plan = relink_restore(&read, holders);
    set_aside_unusable(layout_dir, project_root, &read, &mut plan);
    plan
}

/// The restore of a window 1 that linked at its start to an orphaned window 2: what the orphan's `welcome` says it
/// holds (`holders`, the other window's input: each entry checked, at most
/// [`super::relink::MAX_WELCOME_HOLDERS`]) is held away here, the plugin pages and Lua panels it rebuilt when window
/// 1 ended among it, and everything else is this window's, as the pair file gives it. Window 2 is open and is not
/// started again; nothing of it is taken back. The pair file is this window's to write from now on, naming what the
/// orphan holds.
pub fn relink_restore(read: &ReadPair, holders: &BTreeMap<String, String>) -> PairRestore {
    let mut plan = PairRestore::default();
    let file = match read {
        ReadPair::Read(file) => Some(file.clone()),
        ReadPair::Missing => None,
        ReadPair::Unusable(why) => {
            plan.notes.push(format!(
                "[window] the pair file is unusable ({why}); window 2 says what it holds"
            ));
            None
        }
    };
    plan.start = file.clone().unwrap_or_default();
    for (text, holder) in holders.iter().take(super::relink::MAX_WELCOME_HOLDERS) {
        match ModuleId::parse(text) {
            Ok(id) if holder == WINDOW_2 && (id.kind() == ModuleKind::LuaWebview || id.number() >= 2) => {
                plan.away.insert(id);
            }
            _ => plan.notes.push(format!(
                "[window] window 2 names {} held by {}; left out",
                super::relink::shown(text),
                super::relink::shown(holder)
            )),
        }
    }
    for (text, holder) in &plan.start.holders {
        if holder != WINDOW_1 {
            continue;
        }
        match ModuleId::parse(text) {
            Ok(id) if !plan.away.contains(&id) => {
                plan.build_here.insert(id);
            }
            Ok(_) => {}
            Err(_) => plan.notes.push(format!(
                "[window] the pair file names {text:?} held by {holder:?}; left out"
            )),
        }
    }
    let mut named: BTreeMap<String, String> = plan
        .start
        .holders
        .iter()
        .filter(|(text, holder)| *holder == WINDOW_1 && ModuleId::parse(text).is_ok_and(|id| !plan.away.contains(&id)))
        .map(|(text, holder)| (text.clone(), holder.clone()))
        .collect();
    named.extend(
        plan.away
            .iter()
            .map(|id| (id.as_str().to_string(), WINDOW_2.to_string())),
    );
    plan.start.holders = named;
    plan.start.window2_open = true;
    plan.write_now = file.as_ref() != Some(&plan.start);
    plan.notes.push(format!(
        "[window] linked to the waiting window 2: {} module(s) stay there",
        plan.away.len()
    ));
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
    let missing: Vec<&ModuleId> = plan.build_here.iter().filter(|id| !present.contains(*id)).collect();
    notes.extend(place_missing(
        layout,
        workspaces,
        &missing,
        rules,
        decls,
        can_build,
        "was in window 2",
    ));
    notes
}

/// Each module of `missing`, which this window is to build and none of its layouts holds, placed by its open rules,
/// kept off the keys, then given to every workspace hidden. `can_build` says which modules this window has a host
/// for; one it cannot build is left out, noted. `came` says where the module was, for the notes.
fn place_missing(
    layout: &mut Layout,
    workspaces: &mut Workspaces,
    missing: &[&ModuleId],
    rules: &OpenRules,
    decls: &[ModuleDecl],
    can_build: &dyn Fn(&ModuleId) -> bool,
    came: &str,
) -> Vec<String> {
    let mut notes = Vec::new();
    let mut placed_any = false;
    for &id in missing {
        if !can_build(id) {
            notes.push(format!(
                "[window] '{id}' {came} and this window cannot build it; left out"
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
                notes.push(format!("[window] '{id}' {came}; placed by its open rule"));
            }
            Err(err) => notes.push(format!("[window] '{id}' could not be placed here: {err}")),
        }
    }
    if placed_any {
        notes.extend(workspaces.fit_all_with(layout, decls));
    }
    notes
}

/// A restored window 2's modules the pair file gives it and none of its layouts holds: its layout file can predate
/// what it held (the pair file's write landed and the layout's did not, as the processes ended), and a module given
/// to it and built nowhere would be lost to both windows, window 1 keeping its placeholder away. Each is placed here
/// by its open rules, as window 1 places what comes back to it.
pub fn place_given(
    layout: &mut Layout,
    workspaces: &mut Workspaces,
    given: &BTreeSet<ModuleId>,
    rules: &OpenRules,
    decls: &[ModuleDecl],
    can_build: &dyn Fn(&ModuleId) -> bool,
) -> Vec<String> {
    let present: BTreeSet<ModuleId> = workspaces
        .each(layout)
        .into_iter()
        .flat_map(|(_, l)| l.modules())
        .collect();
    let missing: Vec<&ModuleId> = given.iter().filter(|id| !present.contains(*id)).collect();
    place_missing(
        layout,
        workspaces,
        &missing,
        rules,
        decls,
        can_build,
        "is this window's by the pair file",
    )
}

/// Window 2's start: every module of its layouts that is not one every window has of its own (the terminal and
/// the chat; the editor is retired) and that the pair gives to window 2 (`given`) held away, since window 1 holds
/// it, or no window does and window 1 will be asked for it. What the pair gives it is its own to build, a plugin
/// page or Lua panel the start held away as window 1's included.
pub fn second_start_away(layout: &mut Layout, workspaces: &mut Workspaces, given: &BTreeSet<ModuleId>) {
    let modules: BTreeSet<ModuleId> = workspaces
        .each(layout)
        .into_iter()
        .flat_map(|(_, l)| l.modules())
        .collect();
    for id in modules {
        let own = id == ModuleId::terminal() || id == ModuleId::agent() || id == ModuleId::editor();
        let held_elsewhere = id.kind() == ModuleKind::LuaWebview || id.number() >= 2;
        if !own && held_elsewhere {
            super::hold_away(layout, workspaces, &id, !given.contains(&id));
        }
    }
}

/// What a window 2 started with `--restore` builds and where it goes, from the pair file read once, read-only: the
/// modules it names window 2 the holder of, the monitor it names for window 2, and the file as read, which window 2
/// keeps as its view of the pair.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SecondRestore {
    pub given: BTreeSet<ModuleId>,
    pub remembered: Option<String>,
    pub file: PairFile,
    pub notes: Vec<String>,
}

/// [`SecondRestore`] of `read`. A holder that is not a module id is left out, noted; a file that is missing or
/// unusable gives nothing, so window 2 builds only what every window has.
pub fn second_restore(read: &ReadPair) -> SecondRestore {
    let mut restore = SecondRestore::default();
    match read {
        ReadPair::Read(file) => {
            for (text, holder) in &file.holders {
                if holder != WINDOW_2 {
                    continue;
                }
                match ModuleId::parse(text) {
                    Ok(id) => {
                        restore.given.insert(id);
                    }
                    Err(_) => restore
                        .notes
                        .push(format!("[window] the pair file gives window 2 {text:?}; left out")),
                }
            }
            restore.remembered = remembered_monitor(file, WINDOW_2);
            restore.file = file.clone();
        }
        ReadPair::Missing => restore
            .notes
            .push("[window] window 2 is started again but there is no pair file; it builds its own modules".into()),
        ReadPair::Unusable(why) => restore.notes.push(format!(
            "[window] window 2 is started again but the pair file is unusable ({why}); it builds its own modules"
        )),
    }
    restore.file.window2_open = true;
    restore
}

/// The remembered output for a newly opened window 2, without restoring anything it previously held.
pub fn second_placement(read: &ReadPair) -> Option<String> {
    match read {
        ReadPair::Read(file) => remembered_monitor(file, WINDOW_2),
        ReadPair::Missing | ReadPair::Unusable(_) => None,
    }
}

/// The connector of the output the pair file names for `window`, when it is one a window manager could name.
pub fn remembered_monitor(file: &PairFile, window: &str) -> Option<String> {
    file.monitors
        .get(window)
        .map(|monitor| monitor.connector.clone())
        .filter(|connector| eitri_core::compositor::valid_output_name(connector))
}

/// The output window 1 is put on at its start: the one the pair file names for it, only when window 2 comes back
/// with it. A window starting alone is placed by the window manager, wherever it was when it last closed.
pub fn window1_start_monitor(file: &PairFile, window2_restored: bool) -> Option<String> {
    window2_restored.then(|| remembered_monitor(file, WINDOW_1)).flatten()
}

/// The longest monitor description kept in the pair file: it is only ever shown, and the other window sends it.
const DESCRIPTION_MAX: usize = 128;

/// The other window's report of its monitor as the pair file keeps it: a connector a window manager could name,
/// and its description cut to [`DESCRIPTION_MAX`] characters with control characters dropped. `None` for a
/// connector that is not one.
pub fn peer_monitor(connector: &str, description: &str) -> Option<Monitor> {
    if !eitri_core::compositor::valid_output_name(connector) {
        return None;
    }
    Some(Monitor {
        connector: connector.to_string(),
        description: description
            .chars()
            .filter(|c| !c.is_control())
            .take(DESCRIPTION_MAX)
            .collect(),
    })
}

/// This window's own monitor reports, as the pair file names them: the output list and the output the window is
/// on, from the reports the window's monitor watch posts.
#[derive(Default)]
pub struct MonitorReports {
    monitors: Vec<eitri_core::wm::Monitor>,
}

impl MonitorReports {
    /// The monitor `event` puts this window on, when it says so; the output list is kept for the description.
    pub fn report(&mut self, event: &eitri_core::wm::HostEvent) -> Option<Monitor> {
        use eitri_core::wm::HostEvent;
        match event {
            HostEvent::MonitorsChanged { monitors } => {
                self.monitors = monitors.clone();
                None
            }
            HostEvent::WindowMonitor { monitor: Some(id), .. } => {
                let description = self
                    .monitors
                    .iter()
                    .find(|m| m.id == *id)
                    .map(|m| m.name.clone())
                    .unwrap_or_default();
                peer_monitor(&id.0, &description)
            }
            _ => None,
        }
    }
}

/// A window 1 whose window 2 was to come back and does not (its start refused, or it never finished its
/// handshake): the placeholders this window kept for it come back as at the end of a link (the window controller's,
/// on its own layout), the pair file says window 2 is not open while still naming what it held (until this window's
/// layout holds that), and the tab files to offer from now on add window 2's. Those tab files.
pub fn take_back_unrestored(writer: Option<&mut PairWriter>, alone_on_project: bool) -> TabFiles {
    if let Some(writer) = writer {
        writer.window2_left();
    }
    tab_files(Role::Main, true, alone_on_project, false)
}

/// What a window's close writes to the pair file whose writer it has. Window 2 closing orphaned (window 1 closed
/// first) leaves the pair as it ended, window 2 open and holding what it holds, so the next launch starts both
/// windows again, as it does after window 1 closed last with window 2 open; a window 2 that closes while window 1
/// lives is ended by window 1's own write (`window2_closed`). Window 1's close forgets what a window 2 that is not
/// open held, once its own layout file holds what it took back.
pub fn on_close(writer: &mut PairWriter, second: bool, layout_written: bool) {
    if !second && layout_written {
        writer.forget_window2_holders();
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
    #[cfg(test)]
    clock: Option<Box<dyn Fn() -> Instant>>,
    #[cfg(test)]
    flush_wait: Option<Box<dyn Fn(Duration)>>,
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
            #[cfg(test)]
            clock: None,
            #[cfg(test)]
            flush_wait: None,
        }
    }

    #[cfg(test)]
    pub fn file(&self) -> &PairFile {
        &self.file
    }

    /// Whether this window writes the pair file now.
    #[cfg(test)]
    pub fn writing(&self) -> bool {
        self.writing
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
    /// file renamed into place). Returns true only when no write remains; a new writer must not be welcomed
    /// while this returns false.
    pub fn hand_over(&mut self) -> bool {
        self.writing = false;
        self.shared.lock().next = None;
        let quiet = self.flush(HAND_OVER_WAIT);
        if !quiet {
            eprintln!("[window] the pair file is still being written");
        }
        quiet
    }

    /// A refused handover leaves this window owning the file, with its current snapshot and pending inheritance.
    pub fn keep_ownership(&mut self) {
        self.writing = true;
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

    /// Window 2 is not open and is not coming back (a restore of it that did not finish): the next launch does not
    /// start it again, while what it held stays named, for window 1 to take back.
    pub fn window2_left(&mut self) {
        self.file.window2_open = false;
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
    ///
    /// Monitor memory is the pair's: while no window 2 is open, the report is kept for the pair's next write and
    /// nothing is written, so a window that never had a window 2 leaves no file naming its monitor.
    pub fn monitor(&mut self, window: &str, monitor: Monitor) {
        if self.file.monitors.get(window) == Some(&monitor) {
            return;
        }
        self.file.monitors.insert(window.to_string(), monitor);
        if self.file.window2_open {
            self.write();
        }
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

    fn now(&self) -> Instant {
        #[cfg(test)]
        if let Some(clock) = &self.clock {
            return clock();
        }
        Instant::now()
    }

    /// Waits up to `bound` for every write handed over; whether they all finished.
    pub fn flush(&self, bound: Duration) -> bool {
        let deadline = self.now() + bound;
        let mut slot = self.shared.lock();
        while slot.next.is_some() || slot.busy {
            let left = deadline.saturating_duration_since(self.now());
            if left.is_zero() {
                return false;
            }
            #[cfg(test)]
            if let Some(wait) = &self.flush_wait {
                wait(left);
                continue;
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

    #[test]
    fn a_busy_pair_file_refuses_handover_and_keeps_its_snapshot_when_ownership_returns() {
        let dir = Scratch::new("pair-busy-handover");
        let file = PairFile {
            window2_open: true,
            ..PairFile::default()
        };
        let mut writer = PairWriter::new(&dir.0, &root(), file.clone(), true);
        writer.shared.lock().busy = true;
        writer.shared.lock().next = Some(PairFile::default());
        let clock = std::rc::Rc::new(std::cell::Cell::new(Instant::now()));
        let reading = clock.clone();
        writer.clock = Some(Box::new(move || reading.get()));
        let waits = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let recorded = waits.clone();
        writer.flush_wait = Some(Box::new(move |left| {
            assert!(recorded.borrow().len() < 2, "flush did not finish after its deadline");
            recorded.borrow_mut().push(left);
            clock.set(clock.get() + left);
        }));
        assert!(!writer.hand_over());
        assert_eq!(
            *waits.borrow(),
            [HAND_OVER_WAIT],
            "the active write gets its full budget"
        );
        assert!(!writer.writing());
        writer.keep_ownership();
        assert!(writer.writing());
        assert_eq!(writer.file(), &file);
        let slot = writer.shared.lock();
        assert!(slot.busy);
        assert!(slot.next.is_none(), "keeping ownership queues no replacement snapshot");
        assert!(writer.thread.is_none());
    }

    #[test]
    fn an_idle_pair_file_hands_over_without_waiting_for_a_worker() {
        let dir = Scratch::new("pair-idle-handover");
        let mut writer = PairWriter::new(&dir.0, &root(), PairFile::default(), true);
        assert!(writer.hand_over());
        assert!(!writer.writing());
        assert!(writer.thread.is_none());
    }

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
    fn a_window_without_a_window_2_neither_writes_nor_goes_back_to_its_monitor() {
        let dir = Scratch::new("pair-monitor-alone");
        let mut writer = PairWriter::new(&dir.0, &root(), PairFile::default(), true);
        let dp2 = Monitor {
            connector: "DP-2".into(),
            description: "Dell".into(),
        };
        writer.monitor(WINDOW_1, dp2.clone());
        assert!(writer.flush(BOUND));
        assert_eq!(
            read_pair(&dir.0, &root()),
            ReadPair::Missing,
            "a lone window's monitor is not written"
        );
        // Kept for the pair: window 2's open writes it with the rest.
        writer.window2_opened();
        assert!(writer.flush(BOUND));
        let ReadPair::Read(file) = read_pair(&dir.0, &root()) else {
            panic!()
        };
        assert_eq!(file.monitors.get(WINDOW_1), Some(&dp2));
        assert_eq!(window1_start_monitor(&file, true).as_deref(), Some("DP-2"));
        assert_eq!(
            window1_start_monitor(&file, false),
            None,
            "window 1 starting without window 2 stays where the window manager puts it"
        );
    }

    /// A move's commit in window 2 changes what it holds, which it says with `away{ids}`: that is what names the
    /// moved module's holder in the file, and what forgets it once it went back.
    #[test]
    fn a_move_and_window_2s_close_write_the_pair_file_and_a_window_not_holding_it_writes_nothing() {
        let dir = Scratch::new("pair-moves");
        let mut writer = PairWriter::new(&dir.0, &root(), PairFile::default(), true);
        writer.window2_opened();
        writer.window2_holds(&["terminal#2".to_string()].into_iter().collect());
        assert!(writer.flush(BOUND));
        let ReadPair::Read(file) = read_pair(&dir.0, &root()) else {
            panic!()
        };
        assert_eq!(file.holders["terminal#2"], WINDOW_2);
        writer.window2_holds(&["lua:notes".to_string()].into_iter().collect());
        assert!(writer.flush(BOUND));
        let ReadPair::Read(file) = read_pair(&dir.0, &root()) else {
            panic!()
        };
        assert!(
            !file.holders.contains_key("terminal#2") && file.holders["lua:notes"] == WINDOW_2,
            "a module moved back is no one's: {file:?}"
        );
        writer.window2_closed();
        assert!(writer.flush(BOUND));
        let ReadPair::Read(file) = read_pair(&dir.0, &root()) else {
            panic!()
        };
        assert!(!file.window2_open && file.holders.is_empty(), "{file:?}");

        let other = Scratch::new("pair-not-holder");
        let mut second = PairWriter::new(&other.0, &root(), PairFile::default(), false);
        second.window2_opened();
        second.window2_holds(&["terminal#3".to_string()].into_iter().collect());
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
        let plan = read_for_restore(&dir.0, &root(), true, &|| true);
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
        second.window2_holds(&BTreeSet::from(["lua:notes".to_string(), "terminal#2".to_string()]));
        assert!(second.flush(BOUND));
        let ReadPair::Read(file) = read_pair(&dir.0, &root()) else {
            panic!()
        };
        assert_eq!(file.monitors.get(WINDOW_1), Some(&one_monitor));
    }

    #[test]
    fn an_orphaned_window_2_closing_leaves_the_pair_to_come_back_at_the_next_launch() {
        // Window 1 closed first and window 2 later: both were open when the pair ended, so both come back.
        let dir = Scratch::new("pair-orphan-close");
        let mut second = PairWriter::new(
            &dir.0,
            &root(),
            PairFile {
                window2_open: true,
                ..PairFile::default()
            },
            false,
        );
        second.take_over();
        second.window2_holds(&BTreeSet::from(["terminal#2".to_string()]));
        on_close(&mut second, true, true);
        assert!(second.flush(BOUND));
        let ReadPair::Read(file) = read_pair(&dir.0, &root()) else {
            panic!()
        };
        assert!(file.window2_open, "{file:?}");
        assert_eq!(file.holders["terminal#2"], WINDOW_2);

        // Window 2 closing while window 1 lives is window 1's to write: closed, and it holds nothing.
        let dir = Scratch::new("pair-window-2-closed-first");
        let mut first = PairWriter::new(&dir.0, &root(), PairFile::default(), true);
        first.window2_opened();
        first.window2_holds(&BTreeSet::from(["terminal#2".to_string()]));
        first.window2_closed();
        on_close(&mut first, false, true);
        assert!(first.flush(BOUND));
        let ReadPair::Read(file) = read_pair(&dir.0, &root()) else {
            panic!()
        };
        assert!(!file.window2_open && file.holders.is_empty(), "{file:?}");
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
    fn a_window_2_from_window_new_goes_to_window_2s_remembered_output() {
        let dir = Scratch::new("pair-second-placement");
        written_pair(&dir, false);
        let path = dir.0.join(pair_file_name(&root()));
        let before = bytes(&path);
        assert_eq!(second_placement(&read_pair(&dir.0, &root())).as_deref(), Some("DP-2"));
        assert_eq!(bytes(&path), before);
    }

    #[test]
    fn a_first_window_new_leaves_window_2_to_the_window_manager() {
        let mut file = PairFile::default();
        file.monitors
            .insert(WINDOW_1.into(), peer_monitor("DP-1", "Left").unwrap());
        for read in [
            ReadPair::Read(file),
            ReadPair::Missing,
            ReadPair::Unusable("bad file".into()),
        ] {
            assert_eq!(second_placement(&read), None);
        }
    }

    #[test]
    fn a_remembered_output_no_window_manager_could_name_is_not_used() {
        let mut file = PairFile::default();
        file.monitors.insert(
            WINDOW_2.into(),
            Monitor {
                connector: "../x".into(),
                description: String::new(),
            },
        );
        assert_eq!(second_placement(&ReadPair::Read(file)), None);
    }

    #[test]
    fn a_restored_window_2_builds_the_pages_the_pair_gives_it() {
        let mut layout = window(&["terminal#2", "lua:notes", "lua:other"]);
        let mut workspaces = two_workspaces(layout.clone());
        // Window 2's start in its order: every page held away as window 1's, then what the pair file gives it.
        let frame = eitri_core::layout::Frame::new(NOMINAL_GRID, 1);
        crate::linked_window::start_second(&mut layout, &mut workspaces, &frame).unwrap();
        second_start_away(
            &mut layout,
            &mut workspaces,
            &BTreeSet::from([id("lua:notes"), id("terminal#2")]),
        );
        assert_eq!(
            crate::linked_window::away_everywhere(&layout, &workspaces),
            BTreeSet::from([id("lua:other")]),
            "a page the pair gives window 2 is built there, in every workspace"
        );
    }

    #[test]
    fn a_restored_window_2_builds_a_module_the_pair_gives_it_that_its_layout_lost() {
        // The pair file names terminal#2 as window 2's, but window 2's layout was written before that move.
        let mut layout = window(&["lua:notes"]);
        let mut workspaces = two_workspaces(layout.clone());
        let frame = eitri_core::layout::Frame::new(NOMINAL_GRID, 1);
        crate::linked_window::start_second(&mut layout, &mut workspaces, &frame).unwrap();
        let given = BTreeSet::from([id("terminal#2"), id("lua:notes")]);
        second_start_away(&mut layout, &mut workspaces, &given);
        let keys = layout.focus().clone();
        let notes = place_given(
            &mut layout,
            &mut workspaces,
            &given,
            &OpenRules::default(),
            &[],
            &can_build_all,
        );
        assert!(notes.iter().any(|n| n.contains("terminal#2")), "{notes:?}");
        for (name, l) in workspaces.each(&layout) {
            assert!(l.contains(&id("terminal#2")), "{name}: built here");
            assert!(!l.is_away(&id("terminal#2")), "{name}");
        }
        assert_eq!(layout.focus(), &keys, "and it does not take the keys");
        // Nothing to place when the layout holds everything it is given.
        assert!(place_given(
            &mut layout,
            &mut workspaces,
            &given,
            &OpenRules::default(),
            &[],
            &can_build_all
        )
        .is_empty());
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
                window: None,
                turn_running: false,
                draft: None,
                row_key: None,
                plan_origin: None,
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

    fn output(connector: &str, name: &str) -> eitri_core::wm::Monitor {
        eitri_core::wm::Monitor {
            id: eitri_core::wm::MonitorId(connector.into()),
            rect: eitri_core::layout::Rect {
                x: 0,
                y: 0,
                w: 1920,
                h: 1080,
            },
            scale: 1.0,
            name: name.into(),
        }
    }

    #[test]
    fn a_monitor_report_writes_the_pair_file_at_once() {
        use eitri_core::wm::{HostEvent, MonitorId, WinId};
        let dir = Scratch::new("pair-own-monitor");
        // A pair whose window 2 is open: a lone window's report is not written
        // (`a_window_without_a_window_2_neither_writes_nor_goes_back_to_its_monitor`).
        let open = PairFile {
            window2_open: true,
            ..PairFile::default()
        };
        let mut writer = PairWriter::new(&dir.0, &root(), open, true);
        let mut reports = MonitorReports::default();
        let on = |connector: Option<&str>| HostEvent::WindowMonitor {
            win: WinId::MAIN,
            monitor: connector.map(|c| MonitorId(c.into())),
        };
        assert_eq!(
            reports.report(&HostEvent::MonitorsChanged {
                monitors: vec![output("DP-1", "Left"), output("DP-2", "Right")],
            }),
            None,
            "the output list alone says where no window is"
        );
        let report = reports.report(&on(Some("DP-2"))).expect("the window is on DP-2");
        assert_eq!(
            report,
            Monitor {
                connector: "DP-2".into(),
                description: "Right".into()
            }
        );
        // Nothing else changes: the report alone writes the file.
        writer.monitor(WINDOW_1, report);
        assert!(writer.flush(BOUND));
        assert_eq!(writer.writes(), 1);
        let ReadPair::Read(file) = read_pair(&dir.0, &root()) else {
            panic!("written at once")
        };
        assert_eq!(remembered_monitor(&file, WINDOW_1).as_deref(), Some("DP-2"));
        assert_eq!(file.monitors[WINDOW_1].description, "Right");
        assert_eq!(reports.report(&on(None)), None, "leaving every output names none");

        // The other window's report is its input: a connector no window manager could name is dropped, and a
        // description is kept short and plain.
        assert_eq!(peer_monitor("DP-1; rm -rf ~", "x"), None);
        assert_eq!(peer_monitor("", "x"), None);
        let long = peer_monitor("HDMI-A-1", &format!("Big\n{}", "screen ".repeat(100))).unwrap();
        assert_eq!(long.connector, "HDMI-A-1");
        assert_eq!(long.description.chars().count(), DESCRIPTION_MAX);
        assert!(!long.description.contains('\n'));
        let mut file = PairFile::default();
        file.monitors.insert(
            WINDOW_2.into(),
            Monitor {
                connector: "not an output".into(),
                description: String::new(),
            },
        );
        assert_eq!(
            remembered_monitor(&file, WINDOW_2),
            None,
            "a remembered name is checked the same way"
        );
    }

    fn written_pair(dir: &Scratch, window2_open: bool) {
        let mut file = PairFile {
            window2_open,
            ..PairFile::default()
        };
        for (id, holder) in [("terminal#2", WINDOW_2), ("lua:notes", WINDOW_2), ("bad id!", WINDOW_2)] {
            file.holders.insert(id.into(), holder.into());
        }
        for (window, connector) in [(WINDOW_1, "DP-1"), (WINDOW_2, "DP-2")] {
            file.monitors.insert(
                window.into(),
                Monitor {
                    connector: connector.into(),
                    description: String::new(),
                },
            );
        }
        eitri_core::window_link::pair::write_pair(&dir.0, &root(), &file).unwrap();
    }

    #[test]
    fn a_restored_window_2_builds_what_the_pair_file_gives_it_and_offers_its_tabs() {
        let dir = Scratch::new("pair-second-restore");
        written_pair(&dir, true);

        // Window 1 starts window 2 again only when every condition holds.
        let plan = read_for_restore(&dir.0, &root(), true, &|| true);
        assert!(plan.restore_second);
        assert_eq!(plan.away, BTreeSet::from([id("terminal#2"), id("lua:notes")]));
        assert_eq!(remembered_monitor(&plan.start, WINDOW_1).as_deref(), Some("DP-1"));
        let off = read_for_restore(&dir.0, &root(), false, &|| {
            panic!("not asked when window.restore is off")
        });
        assert!(
            !off.restore_second && off.take_back,
            "window.restore off: window 1 takes everything back"
        );
        let unusable = read_for_restore(&dir.0, &root(), true, &|| false);
        assert!(!unusable.restore_second && unusable.take_back);
        assert!(
            unusable.notes.iter().any(|n| n.contains("layout file")),
            "{:?}",
            unusable.notes
        );
        assert!(!read_for_restore(&dir.0, &root(), true, &|| true).away.is_empty());
        let closed = Scratch::new("pair-second-closed");
        written_pair(&closed, false);
        assert!(
            !read_for_restore(&closed.0, &root(), true, &|| panic!("not asked for a closed window 2")).restore_second
        );

        // Window 2, started with `--restore`, reads the file once, read-only.
        let path = dir.0.join(pair_file_name(&root()));
        let before = bytes(&path);
        let second = second_restore(&read_pair(&dir.0, &root()));
        assert_eq!(bytes(&path), before, "read, never written");
        assert_eq!(second.given, BTreeSet::from([id("terminal#2"), id("lua:notes")]));
        assert_eq!(second.remembered.as_deref(), Some("DP-2"));
        assert!(second.notes.iter().any(|n| n.contains("bad id!")), "{:?}", second.notes);
        assert!(second.file.window2_open && second.file.monitors.contains_key(WINDOW_1));
        let mut layout = window(&["terminal#2", "agent#3", "lua:notes"]);
        let mut workspaces = Workspaces::single();
        second_start_away(&mut layout, &mut workspaces, &second.given);
        assert_eq!(
            layout.away(),
            BTreeSet::from([id("agent#3")]),
            "it builds what it was given"
        );
        let files = tab_files(Role::Second, false, false, true);
        assert!(
            files.offers_own && files.also_offer.is_empty(),
            "it offers its own saved tabs"
        );

        // A missing pair file gives it nothing to build.
        let missing = second_restore(&ReadPair::Missing);
        assert!(missing.given.is_empty() && missing.remembered.is_none() && !missing.notes.is_empty());
    }

    #[test]
    fn a_restore_that_fails_takes_window_2s_modules_and_tabs_back() {
        let dir = Scratch::new("pair-restore-fails");
        written_pair(&dir, true);
        let plan = read_for_restore(&dir.0, &root(), true, &|| true);
        assert!(plan.restore_second);
        let mut layout = window(&["terminal#2", "lua:notes"]);
        let mut workspaces = two_workspaces(layout.clone());
        apply_main_restore(
            &plan,
            &mut layout,
            &mut workspaces,
            &OpenRules::default(),
            &[],
            &can_build_all,
        );
        assert_eq!(layout.away(), BTreeSet::from([id("terminal#2"), id("lua:notes")]));
        let mut writer = PairWriter::new(&dir.0, &root(), plan.start.clone(), true);
        // Window 1 offered only its own saved tabs, window 2 being expected to offer its own.
        assert_eq!(
            tab_files(Role::Main, !plan.restore_second, false, false).also_offer,
            Vec::<Role>::new()
        );

        // The layouts' half, as the window controller carries it out at the same moment.
        let back = eitri_core::window_link::end::end_plan(
            &crate::linked_window::away_everywhere(&layout, &workspaces),
            eitri_core::window_link::end::EndKind::RestoreFailed,
        );
        eitri_core::window_link::end::apply_end(&back, &mut layout, &mut workspaces);
        let files = take_back_unrestored(Some(&mut writer), false);
        assert_eq!(
            back.back,
            [id("lua:notes")],
            "the page is rebuilt here from its own store"
        );
        assert_eq!(
            back.dropped,
            [id("terminal#2")],
            "the terminal's shell ended with the last run"
        );
        for (name, l) in workspaces.each(&layout) {
            assert!(
                l.away().is_empty(),
                "{name}: no placeholder is left for a window that is not coming"
            );
            assert!(l.contains(&id("lua:notes")) && !l.contains(&id("terminal#2")), "{name}");
        }
        assert!(
            files.offers_own && files.also_offer == [Role::Second],
            "window 2's saved tabs are offered now"
        );
        assert!(writer.flush(BOUND));
        let ReadPair::Read(file) = read_pair(&dir.0, &root()) else {
            panic!("written at once")
        };
        assert!(!file.window2_open, "the next launch does not start window 2");
        assert_eq!(
            file.holders.get("lua:notes").map(String::as_str),
            Some(WINDOW_2),
            "what window 2 held stays named until this window's layout holds it"
        );
        // Without a writer (a window that does not hold the pair file) the tabs are offered all the same.
        let files = take_back_unrestored(None, true);
        assert_eq!(files.also_offer, [Role::Second, Role::Unpaired]);
    }
}
