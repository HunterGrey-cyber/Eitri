//! What a window's chrome shows, as values the controller keeps and the shell draws: the top bar's text, the
//! tray's chips, the strips' and the float bars' labels, the prefix strip (and with it the app name's armed
//! block), the toast, the app name's flash and the window's mode. Every field is plain text or a flag the shell
//! sets as it is, never markup.
//!
//! The model is derived per field from the inputs it reads, each a revision the controller bumps when the input
//! may have changed ([`Dirty`]): a field whose inputs did not change since its last derivation is not derived
//! again, so the events that come by the hundred (pointer motion during a drag, a worker's wake, a busy tick of a
//! chat no chip shows) derive nothing. The toast, the flash and the mode are set where they happen. Once per
//! batch, when any field's value changed, the window's chrome revision is bumped and announced as
//! [`HostOp::Chrome`]; the shell reads the model back through [`Controller::chrome`](super::Controller::chrome)
//! and redraws only the widgets whose field changed.
//!
//! What the floats cover is judged once the driver's queue has drained, whenever the layout or the grid's size
//! moved or a chat page came on screen: the panel is told the covered chat views when they changed and which
//! ones a float left, and a page that came on screen is judged against the coverage the batch left, so a float
//! the same batch put over it means the page was not seen.

use super::host::{frame_with, Batch, ChatOp, HostOp, HostReads, TimerToken};
use super::modes::{self, ModeEvent};
use super::prefix_panel::{self, PrefixPanel};
use super::prefix_strip::{self, StripPiece};
use super::titles;
use super::toasts::{refusal_may_show, ToastKind};
use super::tray::{chip_order, chip_states};
use super::window::{Tiling, WinId, Window, WindowMode as WindowState};
use crate::attention::Attention;
use crate::keymap::prefix::Waiting;
use crate::keymap::Keymap;
use crate::layout::{arrange, covering, Arrangement, Layout, ModuleId, ModuleKeys, Workspaces};
use crate::onboarding::OnboardingView;
use crate::tabs::ViewId;
use std::rc::Rc;
use std::time::Duration;

/// What a hub's top bar calls the windows whose marks it sums.
pub const HUB_OTHERS: &str = "chats";

/// How long a toast stays up; a new one starts its time again.
pub const TOAST_FOR: Duration = Duration::from_secs(4);

/// A window's chrome as the controller decides it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ChromeModel {
    /// The top bar's text and the app name's flash.
    pub top_bar: TopBar,
    /// Every chip the tray can show, in its fixed order, shown or not.
    pub tray: Vec<Chip>,
    /// Each tabbed or stacked container's strip on screen, in arrangement order.
    pub strips: Vec<StripLabels>,
    /// Each float on screen, bottom first: what its bar says.
    pub float_bars: Vec<FloatLabel>,
    /// The prefix strip's pieces while the prefix waits, and the app name's armed block with them; `None` while it
    /// waits for nothing.
    pub prefix_strip: Option<Vec<StripPiece>>,
    /// The hint panel while it is up: what each key after the prefix does. `None` while it is not shown, which is
    /// always the case with `keymap.which_key_delay` off.
    pub prefix_panel: Option<PrefixPanel>,
    /// The toast up now; `None` once its time is up.
    pub toast: Option<Toast>,
    /// What the chrome shows for the window's mode.
    pub modes: Modes,
    /// The rename entry while it is open, holding this text.
    pub rename: Option<String>,
    /// The window close flow's or a module kill's y/n; `None` while none is asked. Its `closes_window` flag
    /// routes the window stage's answer to the close flow. Where it sits needs the
    /// strip's place and the bar's and the label's heights, which only the shell's widgets know, so the shell
    /// places it.
    pub prompt: Option<Prompt>,
    /// The first-start screen while it is shown; `None` while there is none, and while a question holds the keys
    /// the screen gave back. Set where it changes ([`touched`]), as the prompt is, so it has no field of its own
    /// among the derived ones.
    pub onboarding: Option<OnboardingView>,
}

/// A y/n the window asks: its text, as plain text, and whether it is the window close's own question.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Prompt {
    pub text: String,
    pub closes_window: bool,
    pub offers_move: bool,
}

/// The top bar's text, left to right, and the app name's flash.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TopBar {
    /// The project's name.
    pub project: String,
    /// The current workspace's name, only while there are several.
    pub workspace: Option<String>,
    /// The selected container, `selected: tabbed (editor, agent)`, while one is selected.
    pub selection: Option<String>,
    /// The module `swap.marked` swaps with, `marked: agent`, while one is marked.
    pub mark: Option<String>,
    /// Bumped by every refusal: each new value flashes the app name once, so a repeat is a new flash.
    pub flash: u64,
    /// What the other window of a linked pair holds, from its latest summary.
    pub other: Option<String>,
}

/// One tray chip: its module, its text, whether it is shown (the module is off screen or under a float),
/// whether its chat asks for the user, and whether it is a stowed scratch float.
#[derive(Debug, Clone, PartialEq)]
pub struct Chip {
    pub module: ModuleId,
    pub label: String,
    pub shown: bool,
    pub attention: bool,
    pub stowed: bool,
}

/// A container strip's entries, left to right, and which of them is drawn.
#[derive(Debug, Clone, PartialEq)]
pub struct StripLabels {
    pub entries: Vec<EntryLabel>,
    pub active: Option<usize>,
}

/// One strip entry: its module and what it says, as the module's chip would.
#[derive(Debug, Clone, PartialEq)]
pub struct EntryLabel {
    pub module: ModuleId,
    pub label: String,
}

/// A float's bar: its module and the module's title.
#[derive(Debug, Clone, PartialEq)]
pub struct FloatLabel {
    pub module: ModuleId,
    pub label: String,
}

/// The toast up now. Each toast shown gets a new serial, so the same text twice is two toasts.
#[derive(Debug, Clone, PartialEq)]
pub struct Toast {
    pub text: String,
    pub kind: ToastKind,
    pub serial: u64,
}

/// What the chrome shows for the window's mode: the mode, and whether a wait has brought the top bar out.
#[derive(Debug, Clone, PartialEq)]
pub struct Modes {
    /// The mode as `window.immersive` here toggled it. A shell that still changes the mode on its own (bare F11,
    /// the editor's fullscreen setting) and refuses `SetMode` keeps the window's real mode in its own toggle, and
    /// this one can differ from it; it becomes the window's mode once the host carries out `SetMode` and every
    /// mode change reaches the controller.
    pub mode: modes::WindowMode,
    /// The workspace chooser waits: its strip is the only list of what its keys pick, so the bar is out even
    /// where the mode hides it.
    pub top_bar_revealed: bool,
}

impl Default for Modes {
    fn default() -> Self {
        Modes {
            mode: modes::WindowMode::Windowed,
            top_bar_revealed: false,
        }
    }
}

/// The revision of each input the model's fields are derived from. The controller bumps one when its input may
/// have changed; a field is derived again only when one of the revisions it reads has moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Dirty {
    /// The attached peer and its latest summary.
    pub peer: u64,
    /// The layout: every change the controller makes, and every change the shell reports.
    pub layout: u64,
    /// The attention of a chat that a chip or a strip entry shows.
    pub attention: u64,
    /// What the prefix waits for, its plan and its keymap.
    pub prefix: u64,
    /// The hint panel going up or coming down.
    pub hints: u64,
    /// The toast and the flash.
    pub toast: u64,
    /// The window's mode.
    pub modes: u64,
    /// The workspaces: their names, their order and the current one.
    pub workspaces: u64,
    /// The project's name, the Lua panels' titles and the tray's modules.
    pub titles: u64,
    /// The grid's size while a float is up: a float's rectangle is in pixels, so a resize can change what it
    /// covers.
    pub size: u64,
}

/// How many times each derived field was derived, for telling a derivation skipped from one done.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Derivations {
    pub top_bar: u64,
    pub tray: u64,
    pub strips: u64,
    pub float_bars: u64,
    pub prefix_strip: u64,
    pub prefix_panel: u64,
    pub modes: u64,
}

/// What the shell names things by: the project, each Lua panel by the title it registered with, and the modules
/// the tray has chips for (each chat and terminal with all its numbered instances after it).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ChromeTitles {
    /// The declared window title in a plugin's own window, otherwise the project's name.
    pub project: String,
    pub lua: Vec<(ModuleId, String)>,
    pub tray: Vec<ModuleId>,
}

impl ChromeTitles {
    pub fn project_name(root: &std::path::Path, declared: Option<&str>) -> String {
        declared
            .map(str::to_owned)
            .unwrap_or_else(|| super::titles::project_display_name(root))
    }
}

#[cfg(test)]
#[test]
fn the_own_windows_title_is_the_declared_title() {
    let root = std::path::Path::new("/projects/work");
    assert_eq!(ChromeTitles::project_name(root, Some("Demo Window")), "Demo Window");
    assert_eq!(
        ChromeTitles::project_name(root, None),
        super::titles::project_display_name(root)
    );
}

/// The fields derived from inputs, in the order they are derived.
#[derive(Debug, Clone, Copy)]
enum Field {
    TopBar,
    Tray,
    Strips,
    FloatBars,
    PrefixStrip,
    PrefixPanel,
    Modes,
}

const FIELDS: [Field; 7] = [
    Field::TopBar,
    Field::Tray,
    Field::Strips,
    Field::FloatBars,
    Field::PrefixStrip,
    Field::PrefixPanel,
    Field::Modes,
];

impl Field {
    /// The revisions the field reads. The hint panel reads what the prefix waits for, the layout and the titles
    /// only while it is up (`panel_up`): while it is not, a wait begun or ended derives nothing of it.
    fn inputs(self, d: &Dirty, panel_up: bool) -> [u64; 5] {
        match self {
            Field::TopBar => [d.layout, d.workspaces, d.titles, d.peer, 0],
            Field::Tray => [d.layout, d.attention, d.titles, d.size, 0],
            Field::Strips => [d.layout, d.attention, d.titles, 0, 0],
            Field::FloatBars => [d.layout, d.titles, 0, 0, 0],
            Field::PrefixStrip => [d.prefix, d.layout, d.workspaces, d.titles, d.peer],
            Field::PrefixPanel if panel_up => [d.hints, d.prefix, d.layout, d.titles, 0],
            Field::PrefixPanel => [d.hints, 0, 0, 0, 0],
            Field::Modes => [d.prefix, d.modes, 0, 0, 0],
        }
    }
}

/// The controller's bookkeeping for a window's chrome, beside the model it derives.
#[derive(Default)]
pub struct ChromeState {
    pub(super) dirty: Dirty,
    /// The input revisions each field was last derived from. A window starts with every field as derived from
    /// the inputs' first revisions, so its first derivation waits for an input to move (the first layout batch,
    /// the titles, the prefix) rather than falling on whatever event comes first.
    stamps: [[u64; 5]; 7],
    /// The hint panel is up, so its field reads the inputs a shown panel depends on.
    panel_up: bool,
    pub(super) derivations: Derivations,
    titles: ChromeTitles,
    /// The tray's chips, in their fixed order.
    tray_order: Vec<ModuleId>,
    /// A field set where it happened changed, so the batch announces the model.
    announce: bool,
    toast_serial: u64,
    toast_timer: Option<TimerToken>,
    /// Chat pages that came on screen since the queue last drained, in the order they did.
    shown: Vec<ViewId>,
    /// The covered chat views the panel was last told of; it starts knowing of none.
    coverage_told: Vec<ViewId>,
    /// The layout's and the size's revisions the coverage was last judged at.
    coverage_judged: (u64, u64),
    /// The idle that derives what a resize changed, armed by the allocation that saw the new size.
    resize_idle: Option<TimerToken>,
}

impl ChromeState {
    /// Whether the field's inputs moved since its derivation. Asked at the end of every batch, so it allocates
    /// nothing.
    fn due(&self, field: Field) -> bool {
        self.stamps[field as usize] != field.inputs(&self.dirty, self.panel_up)
    }

    fn stamp(&mut self, field: Field) {
        self.stamps[field as usize] = field.inputs(&self.dirty, self.panel_up);
    }
}

/// The digits `plan` draws, as the hint coordinator takes them: each module with its digit's value, the tenth
/// module's `0` being 0.
pub fn digits(plan: &[(char, ModuleId)]) -> Vec<(ModuleId, u8)> {
    plan.iter()
        .filter_map(|(digit, module)| digit.to_digit(10).map(|value| (module.clone(), value as u8)))
        .collect()
}

/// The hint panel went up or came down: its field is derived again.
pub(super) fn hints_shown(w: &mut Window, up: bool) {
    w.chrome_state.panel_up = up;
    w.chrome_state.dirty.hints += 1;
}

/// A field was set where it happened: the batch announces the model.
pub(super) fn touched(w: &mut Window) {
    w.chrome_state.announce = true;
}

/// The window's y/n shown with `prompt`, or taken down with `None`.
pub(super) fn set_prompt(w: &mut Window, prompt: Option<Prompt>) {
    if w.chrome.prompt != prompt {
        w.chrome.prompt = prompt;
        touched(w);
    }
}

/// A module's name as its chip and the tray show it, by the titles the shell last gave.
pub(super) fn title(w: &Window, id: &ModuleId) -> String {
    titles::module_title(id, &w.chrome_state.titles.lua)
}

/// The names the chrome uses changed.
pub(super) fn set_titles(w: &mut Window, titles: ChromeTitles) {
    let state = &mut w.chrome_state;
    state.tray_order = chip_order(&titles.tray);
    state.titles = titles;
    state.dirty.titles += 1;
}

/// A refusal: the app name flashes, and the toast says `text` unless it is empty (the flash alone) or a waiting
/// card's toast is up, which a stray key must not erase.
pub(super) fn refused(w: &mut Window, text: &str) {
    w.chrome.top_bar.flash += 1;
    w.chrome_state.dirty.toast += 1;
    touched(w);
    if text.is_empty() || !refusal_may_show(w.chrome.toast.as_ref().map(|toast| toast.kind)) {
        return;
    }
    show_toast(w, text.to_string(), ToastKind::Other);
}

/// Puts `text` up as the toast, a new serial, for [`TOAST_FOR`].
pub(super) fn show_toast(w: &mut Window, text: String, kind: ToastKind) {
    w.chrome_state.toast_serial += 1;
    w.chrome.toast = Some(Toast {
        text,
        kind,
        serial: w.chrome_state.toast_serial,
    });
    w.chrome_state.dirty.toast += 1;
    touched(w);
    if let Some(old) = w.chrome_state.toast_timer.take() {
        w.cancel_timer(old);
    }
    w.chrome_state.toast_timer = Some(w.arm_after(TOAST_FOR));
}

/// A timer fired: when it is the toast's, its time is up and it goes. `false` when `token` is not the toast's.
pub(super) fn toast_timer_fired(w: &mut Window, token: TimerToken) -> bool {
    if w.chrome_state.toast_timer != Some(token) {
        return false;
    }
    w.chrome_state.toast_timer = None;
    w.chrome.toast = None;
    w.chrome_state.dirty.toast += 1;
    touched(w);
    true
}

/// A chat view's attention or busy state changed: what the chrome shows of it is derived again only when a chip
/// or a strip entry shows that chat. A chat on neither has nothing of it drawn.
pub(super) fn attention_changed(w: &mut Window, view: ViewId) {
    let module = view.module();
    let drawn = w.chrome.tray.iter().any(|chip| chip.shown && chip.module == module)
        || w.chrome
            .strips
            .iter()
            .any(|strip| strip.entries.iter().any(|entry| entry.module == module));
    if drawn {
        w.chrome_state.dirty.attention += 1;
    }
}

/// A chat page came on screen during a batch: it is judged once the queue has drained.
pub(super) fn page_shown(w: &mut Window, view: ViewId) {
    if !w.chrome_state.shown.contains(&view) {
        w.chrome_state.shown.push(view);
    }
    attention_changed(w, view);
}

/// The grid was allocated at a new size. While a float is up, what it covers may have changed: the tray is
/// derived again from an idle, since no batch is computed inside an allocation, and the coverage is judged once
/// that idle's batch has drained.
pub(super) fn resized(w: &mut Window) {
    if w.layout.try_borrow().is_ok_and(|layout| layout.floats().is_empty()) {
        return;
    }
    w.chrome_state.dirty.size += 1;
    if w.chrome_state.resize_idle.is_none() {
        w.chrome_state.resize_idle = Some(w.arm_idle());
    }
}

/// A timer fired: whether it is the idle a resize armed. Its batch derives what the resize changed.
pub(super) fn resize_idle_fired(w: &mut Window, token: TimerToken) -> bool {
    if w.chrome_state.resize_idle != Some(token) {
        return false;
    }
    w.chrome_state.resize_idle = None;
    true
}

/// Whether the coverage is to be judged: a page came on screen, or the layout or the grid's size moved since it
/// was last judged. Asked every time the queue drains, so it allocates nothing.
pub(super) fn coverage_due(w: &Window) -> bool {
    let state = &w.chrome_state;
    !state.shown.is_empty() || state.coverage_judged != (state.dirty.layout, state.dirty.size)
}

/// The coverage the whole batch left, judged: the panel is told which chat views a float covers now, when that
/// changed, and which views a float left (what finished under it is seen once it is clear); each page that came
/// on screen and is not covered now is seen. A page a float of the same batch covers is not.
pub(super) fn reconcile_coverage(w: &mut Window, win: WinId, reads: &dyn HostReads, batch: &mut Batch) {
    let shown = std::mem::take(&mut w.chrome_state.shown);
    let layout = w.layout.clone();
    let Ok(layout) = layout.try_borrow() else {
        w.warn("[wm] BUG: the layout was borrowed when the coverage was judged; judged at the next drain".into());
        w.chrome_state.shown = shown;
        return;
    };
    let judged = (w.chrome_state.dirty.layout, w.chrome_state.dirty.size);
    let covered = if layout.floats().is_empty() {
        Vec::new()
    } else {
        let arrangement = arrangement(&layout, reads, win);
        covered_views(&layout, &arrangement)
    };
    drop(layout);
    w.chrome_state.coverage_judged = judged;
    if w.chrome_state.coverage_told != covered {
        batch.push_effect(HostOp::Chat(ChatOp::SetCoverage {
            win,
            covered: covered.clone(),
        }));
        let was = std::mem::replace(&mut w.chrome_state.coverage_told, covered.clone());
        // A page that came on screen is told below, whether a float left it or it was mapped.
        let uncovered: Vec<ViewId> = was
            .into_iter()
            .filter(|view| !covered.contains(view) && !shown.contains(view))
            .collect();
        if !uncovered.is_empty() {
            batch.push_effect(HostOp::Chat(ChatOp::TellUncovered { win, views: uncovered }));
        }
    }
    for view in shown {
        if covered.contains(&view) {
            w.say(format!(
                "[attention] {}: on screen under a float, not seen",
                view.module()
            ));
        } else {
            batch.push_effect(HostOp::Chat(ChatOp::PageShown { win, view }));
        }
    }
}

/// `window.immersive`: the mode toggled, and the window asked for its new state. The chrome follows from the mode
/// once the batch is derived; the window's own state changes only through the op.
pub(super) fn toggle_immersive(w: &mut Window, win: WinId, batch: &mut Batch) {
    let next = w.chrome.modes.mode.next(ModeEvent::ToggleImmersive);
    w.chrome.modes.mode = next;
    w.chrome_state.dirty.modes += 1;
    touched(w);
    w.say(format!("[window_mode] immersive -> {next:?}"));
    batch.push_effect(HostOp::SetMode {
        win,
        mode: WindowState {
            immersive: matches!(next, modes::WindowMode::Immersive { .. }),
            fullscreen: next.is_fullscreen(),
            top_bar_revealed: prefix_strip::wait_needs_the_top_bar(w.keys.waiting),
        },
    });
}

/// Derives every field whose inputs moved, and announces the model once when any value changed (or a field set
/// where it happened did). A window whose inputs did not move costs a comparison per field and allocates nothing.
pub(super) fn settle(
    w: &mut Window,
    win: WinId,
    reads: &dyn HostReads,
    keys: Option<&(Rc<Keymap>, Rc<ModuleKeys>)>,
    batch: &mut Batch,
) {
    let due = FIELDS.map(|field| w.chrome_state.due(field));
    let mut changed = std::mem::take(&mut w.chrome_state.announce);
    if due.iter().any(|due| *due) {
        changed |= derive(w, win, reads, keys, due);
    }
    if changed {
        w.chrome_changed(win, batch);
    }
}

/// The fields marked `due`, derived from the layout and the workspaces as they are now. A layout or workspaces
/// borrowed right now leave the fields that read them for the next batch. Whether any value changed.
fn derive(
    w: &mut Window,
    win: WinId,
    reads: &dyn HostReads,
    keys: Option<&(Rc<Keymap>, Rc<ModuleKeys>)>,
    due: [bool; 7],
) -> bool {
    let layout = w.layout.clone();
    let workspaces = w.workspaces.clone();
    let Ok(layout) = layout.try_borrow() else {
        // Only the mode's reveal reads no layout.
        return due[Field::Modes as usize] && derive_modes(w);
    };
    let workspaces = workspaces.try_borrow().ok();
    let needs_arrangement = due[Field::Strips as usize]
        || due[Field::FloatBars as usize]
        || (due[Field::Tray as usize] && !layout.floats().is_empty());
    let arrangement = needs_arrangement.then(|| arrangement(&layout, reads, win));
    let mut changed = false;
    if due[Field::TopBar as usize] {
        changed |= derive_top_bar(w, &layout, workspaces.as_deref());
    }
    if due[Field::Tray as usize] {
        changed |= derive_tray(w, &layout, arrangement.as_ref(), reads);
    }
    if let (true, Some(arrangement)) = (due[Field::Strips as usize], arrangement.as_ref()) {
        changed |= derive_strips(w, arrangement, reads);
    }
    if let (true, Some(arrangement)) = (due[Field::FloatBars as usize], arrangement.as_ref()) {
        changed |= derive_float_bars(w, arrangement);
    }
    if due[Field::PrefixStrip as usize] {
        changed |= derive_prefix_strip(w, &layout, workspaces.as_deref(), keys);
    }
    if due[Field::PrefixPanel as usize] {
        changed |= derive_prefix_panel(w, &layout, keys);
    }
    if due[Field::Modes as usize] {
        changed |= derive_modes(w);
    }
    changed
}

fn derive_top_bar(w: &mut Window, layout: &Layout, workspaces: Option<&Workspaces>) -> bool {
    let Some(workspaces) = workspaces else {
        return false;
    };
    let lua = &w.chrome_state.titles.lua;
    let title = |id: &ModuleId| titles::module_title(id, lua);
    let top_bar = TopBar {
        project: w.chrome_state.titles.project.clone(),
        workspace: titles::workspace_text(workspaces.order(), workspaces.current()),
        selection: titles::selection_text(layout, &title),
        mark: titles::mark_text(layout.mark(), &title),
        flash: w.chrome.top_bar.flash,
        other: w.group_summary().map(|s| {
            // A hub's other windows are its chats, summed: no one window's number names them.
            let name = match w.tiling {
                Tiling::Hub => HUB_OTHERS,
                Tiling::Eitri | Tiling::ModuleWindow => super::moves::other_name(w),
            };
            crate::window_link::summary::marks(name, &s)
        }),
    };
    w.chrome_state.stamp(Field::TopBar);
    w.chrome_state.derivations.top_bar += 1;
    put(&mut w.chrome.top_bar, top_bar)
}

fn derive_tray(w: &mut Window, layout: &Layout, arrangement: Option<&Arrangement>, reads: &dyn HostReads) -> bool {
    let lua = &w.chrome_state.titles.lua;
    let order = if w.chrome_state.tray_order.is_empty() {
        // Before the shell names the tray's modules, the layout's own.
        chip_order(&layout.modules())
    } else {
        w.chrome_state.tray_order.clone()
    };
    let chips: Vec<(ModuleId, String)> = order
        .into_iter()
        .map(|id| {
            let title = titles::module_title(&id, lua);
            (id, title)
        })
        .collect();
    let covered: Vec<ModuleId> = arrangement
        .map(|arrangement| covered_views(layout, arrangement))
        .unwrap_or_default()
        .into_iter()
        .map(ViewId::module)
        .collect();
    let states = chip_states(&chips, layout, &|id| attention_of(reads, id), &covered);
    let tray = chips
        .into_iter()
        .zip(states)
        .map(|((module, title), state)| {
            let stowed = layout.is_stowed(&module);
            match state {
                Some((label, asking)) => Chip {
                    module,
                    label,
                    shown: true,
                    attention: asking,
                    stowed,
                },
                None => Chip {
                    module,
                    label: title,
                    shown: false,
                    attention: false,
                    stowed,
                },
            }
        })
        .collect();
    w.chrome_state.stamp(Field::Tray);
    w.chrome_state.derivations.tray += 1;
    put(&mut w.chrome.tray, tray)
}

fn derive_strips(w: &mut Window, arrangement: &Arrangement, reads: &dyn HostReads) -> bool {
    let lua = &w.chrome_state.titles.lua;
    let strips = arrangement
        .strips
        .iter()
        .map(|strip| StripLabels {
            entries: strip
                .entries
                .iter()
                .map(|entry| EntryLabel {
                    module: entry.id.clone(),
                    label: titles::strip_label(
                        &entry.id,
                        &titles::module_title(&entry.id, lua),
                        attention_of(reads, &entry.id),
                    ),
                })
                .collect(),
            active: strip.entries.iter().position(|entry| entry.active),
        })
        .collect();
    w.chrome_state.stamp(Field::Strips);
    w.chrome_state.derivations.strips += 1;
    put(&mut w.chrome.strips, strips)
}

fn derive_float_bars(w: &mut Window, arrangement: &Arrangement) -> bool {
    let lua = &w.chrome_state.titles.lua;
    let bars = arrangement
        .floats
        .iter()
        .map(|spot| FloatLabel {
            module: spot.id.clone(),
            label: titles::module_title(&spot.id, lua),
        })
        .collect();
    w.chrome_state.stamp(Field::FloatBars);
    w.chrome_state.derivations.float_bars += 1;
    put(&mut w.chrome.float_bars, bars)
}

/// The prefix strip as the prefix's wait reads: the digits and their modules, the workspace chooser's list, or
/// the module keys (those that reach each module now while armed, the fixed ones after a split key or the hide
/// key) and the verbs. A window with no keymap yet has no prefix to wait with.
fn derive_prefix_strip(
    w: &mut Window,
    layout: &Layout,
    workspaces: Option<&Workspaces>,
    keys: Option<&(Rc<Keymap>, Rc<ModuleKeys>)>,
) -> bool {
    let waiting = w.keys.waiting;
    let lua = &w.chrome_state.titles.lua;
    let title = |id: &ModuleId| titles::module_title(id, lua);
    let pieces = match waiting {
        Waiting::No => None,
        Waiting::Number => Some(prefix_strip::number_pieces(&w.keys.plan, &title)),
        Waiting::Workspace { deleting } => {
            let Some(workspaces) = workspaces else {
                return false;
            };
            let mut order = workspaces.order().to_vec();
            if !deleting {
                if let Some(summary) = w.group_summary() {
                    order.extend(
                        summary
                            .workspaces
                            .iter()
                            .map(|name| format!("{}:{name}", super::moves::other_name(w))),
                    );
                }
            }
            Some(prefix_strip::workspace_pieces(&order, workspaces.current(), deleting))
        }
        Waiting::Command | Waiting::Module(_) | Waiting::Hide => keys.map(|(keymap, module_keys)| {
            let entries = match waiting {
                Waiting::Command => crate::layout::strip_direct(module_keys, keymap, layout),
                Waiting::Hide => crate::layout::strip_for_hide(module_keys, layout),
                _ => crate::layout::strip(module_keys, layout),
            };
            let verbs = match w.tiling {
                Tiling::Eitri => keymap.strip_verbs(),
                Tiling::Hub => keymap.compositor_strip_verbs(true),
                Tiling::ModuleWindow => keymap.compositor_strip_verbs(false),
            };
            prefix_strip::strip_pieces(waiting, &entries, &title(layout.focus()), &title, &verbs)
        }),
    };
    w.chrome_state.stamp(Field::PrefixStrip);
    w.chrome_state.derivations.prefix_strip += 1;
    put(&mut w.chrome.prefix_strip, pieces)
}

/// The hint panel as the prefix's wait reads, while it is up; nothing otherwise. A window with no keymap yet has
/// nothing to list.
fn derive_prefix_panel(w: &mut Window, layout: &Layout, keys: Option<&(Rc<Keymap>, Rc<ModuleKeys>)>) -> bool {
    let panel = match keys {
        Some((keymap, module_keys)) if w.keys.hints_shown => {
            let waiting = w.keys.waiting;
            let lua = &w.chrome_state.titles.lua;
            let title = |id: &ModuleId| titles::module_title(id, lua);
            let entries = match waiting {
                Waiting::Module(_) => crate::layout::strip(module_keys, layout),
                Waiting::Hide => crate::layout::strip_for_hide(module_keys, layout),
                _ => Vec::new(),
            };
            prefix_panel::prefix_panel(waiting, keymap, module_keys, &entries, &title(layout.focus()), &title)
        }
        _ => None,
    };
    w.chrome_state.stamp(Field::PrefixPanel);
    w.chrome_state.derivations.prefix_panel += 1;
    put(&mut w.chrome.prefix_panel, panel)
}

fn derive_modes(w: &mut Window) -> bool {
    // The rename entry lives in the top bar, so the bar stays out while it is open.
    let revealed = prefix_strip::wait_needs_the_top_bar(w.keys.waiting) || w.chrome.rename.is_some();
    w.chrome_state.stamp(Field::Modes);
    w.chrome_state.derivations.modes += 1;
    put(&mut w.chrome.modes.top_bar_revealed, revealed)
}

/// The layout arranged in the frame the grid would give it now.
fn arrangement(layout: &Layout, reads: &dyn HostReads, win: WinId) -> Arrangement {
    frame_with(reads, win, reads.grid_size(win), |frame| arrange(layout, frame))
}

/// The chat views a float covers in `arrangement`: the views, not the floats, which would otherwise count as off
/// screen themselves.
fn covered_views(layout: &Layout, arrangement: &Arrangement) -> Vec<ViewId> {
    if layout.floats().is_empty() {
        return Vec::new();
    }
    layout
        .modules()
        .iter()
        .filter_map(ViewId::of_module)
        .filter(|view| !covering(arrangement, &view.module()).is_empty())
        .collect()
}

/// A chat's attention as the panel holds it; nothing for any other module.
fn attention_of(reads: &dyn HostReads, id: &ModuleId) -> Attention {
    ViewId::of_module(id)
        .map(|view| reads.view_attention(view))
        .unwrap_or_default()
}

/// Puts `value` in `slot`; whether it differed.
fn put<T: PartialEq>(slot: &mut T, value: T) -> bool {
    if *slot == value {
        return false;
    }
    *slot = value;
    true
}

#[cfg(test)]
mod tests;
