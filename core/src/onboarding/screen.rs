//! The screen's state machine: steps, keys and the view each step shows.
//!
//! Nothing here does I/O or holds a timer: every key, answer and timeout is a method call, and everything the
//! screen wants done comes back as an [`Effect`] for the controller to carry out. The generated file's text is
//! built from the same [`Choice`] the review step shows, so what is shown is what is written.
//!
//! `Opening::config_path` is the configuration directory; the file the screen names is `init.lua` inside it.

use std::time::Duration;

use super::detect::{prefix_is_a_compositor_bind, preselect, NOTE_HYPR_LUA};
use super::display::display_text;
use super::presets::{self, bound_in, start_table};
use super::text::{fill, text, TextId};
use super::{
    Build, Choice, DesktopRow, Found, Generated, InputRow, Lang, NeovimRow, OnboardingView, OpenMode, Opening,
    PresetId, Scroll, ShowWhy, Tiling, TmuxRow, ViewButton, ViewItem, ViewKey, ViewRow, WmMod, WriteOutcome,
    FOOTER_SEPARATOR,
};
use crate::keymap::prefix::Press;
use crate::keymap::stock::STOCK_PREFIX;
use crate::keymap::{KeyName, KeySpec, Keymap, KeymapOp};

/// How long the press test waits for the key before it says the key never arrived.
pub const PRESS_TEST_TIMEOUT: Duration = Duration::from_secs(8);

/// The name `{wm}` falls back to when a warning has no compositor name.
const UNNAMED_COMPOSITOR: &str = "compositor";

/// Where the screen is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Not shown: a trust question has the keys, or the screen has not been shown yet. `resume` is what comes
    /// back.
    Held {
        resume: Box<Step>,
    },
    /// The presets; `cursor` indexes the offered list.
    Choose {
        cursor: usize,
    },
    /// Preset 3's (and the review's `p`) choice of prefix: `a`, `s` or `k`.
    SubPrefix,
    /// `k` on the prefix choice: the next key is the prefix.
    SubKey,
    /// Preset 4's choice of the window manager's `$mod`.
    SubMod,
    /// The lines that would be written (or, read-only, that would be).
    Review,
    /// The press test: `seen` is the last other key that arrived; `timed_out` once nothing came in time, when
    /// the keys are back on the screen.
    Press {
        want: KeySpec,
        seen: Option<String>,
        timed_out: bool,
    },
    Writing,
    Done {
        path: String,
    },
    Failed {
        why: String,
    },
    NotWritten {
        why: String,
    },
}

/// What the marker file records when the screen is left without writing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkerEvent {
    NotNow,
    Declined,
}

/// What the screen asks its controller to do. The screen does none of it.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// The keys go to the screen.
    FocusScreen,
    /// The keys go back to the module that held them.
    FocusBack,
    ArmPressTimer,
    CancelPressTimer,
    /// Read Neovim's mappings again, probing these keys as well.
    Rescan(Vec<KeySpec>),
    /// Move the screen's text.
    Scroll(Scroll),
    /// Check and write this file.
    Write(Generated),
    Marker(MarkerEvent),
    /// Show these lines in a scratch buffer in the editor.
    OpenLines(Vec<String>),
    OpenTutor,
    Close,
}

/// Why the file the user chose was not written, when the check before writing said no.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckWhy {
    /// The check could not run to an answer.
    Unverified,
    /// The keymap refused what the file says, in its own words.
    Refused(String),
}

/// What a press means to the screen: the keys it reads, with every chord and unnamed key one `Other`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum In {
    Esc,
    Char(char),
    Enter,
    Back,
    Up,
    Down,
    PageUp,
    PageDown,
    Other,
}

fn classify(press: &Press) -> In {
    match press {
        Press::Escape => In::Esc,
        Press::Key(key) if !key.ctrl && !key.meta => match key.key {
            KeyName::Char(c) => In::Char(c),
            KeyName::Space => In::Char(' '),
            KeyName::Enter => In::Enter,
            KeyName::BSpace => In::Back,
            KeyName::Up => In::Up,
            KeyName::Down => In::Down,
            KeyName::PPage => In::PageUp,
            KeyName::NPage => In::PageDown,
            _ => In::Other,
        },
        _ => In::Other,
    }
}

/// Every footer the screen shows.
const FOOTERS: [TextId; 8] = [
    TextId::FootChoose,
    TextId::FootChooseShow,
    TextId::FootReview,
    TextId::FootReviewPress,
    TextId::FootPress,
    TextId::FootDone,
    TextId::FootShow,
    TextId::FootFailed,
];

/// The key each part of footer `id` names, in the footer's order; `None` for a part that names no single key
/// (`j/k or 1-4 choose`: the presets are buttons of their own; `j/k scroll`: nothing to press it with).
fn footer_keys(id: TextId) -> &'static [Option<ViewKey>] {
    use ViewKey::{Back, Char, Enter, Esc};
    match id {
        TextId::FootChoose => &[
            None,
            Some(Enter),
            Some(Esc),
            Some(Char('d')),
            Some(Char('r')),
            Some(Char('?')),
        ],
        TextId::FootChooseShow => &[None, Some(Enter), Some(Esc), Some(Char('r')), Some(Char('?'))],
        TextId::FootReview | TextId::FootReviewPress => &[Some(Enter), Some(Back), Some(Esc), None, Some(Char('?'))],
        TextId::FootPress => &[Some(Esc)],
        TextId::FootDone => &[Some(Char('t')), Some(Enter)],
        TextId::FootShow => &[Some(Char('o')), Some(Esc), None, Some(Char('?'))],
        TextId::FootFailed => &[Some(Back), Some(Esc)],
        _ => &[],
    }
}

/// The part of footer `id` that names `key`.
fn footer_part(lang: Lang, id: TextId, key: ViewKey) -> &'static str {
    text(lang, id)
        .split(FOOTER_SEPARATOR)
        .zip(footer_keys(id))
        .find_map(|(part, named)| (*named == Some(key)).then_some(part))
        .unwrap_or_default()
}

fn parsed(spelling: &str) -> KeySpec {
    KeySpec::parse(spelling).expect("the screen's own key spellings parse")
}

fn stock_prefix() -> KeySpec {
    parsed(STOCK_PREFIX)
}

fn ctrl_space() -> KeySpec {
    parsed("C-Space")
}

/// Why `key` cannot be the prefix, in the keymap's own words, or `None` when it can. The keymap decides; the
/// screen keeps no second rule.
fn prefix_refusal(key: &KeySpec) -> Option<String> {
    let op = KeymapOp::Prefix { key: key.to_string() };
    Keymap::defaults().then_user(&[op], &[]).err().map(|e| e.to_string())
}

fn is_tiling_compositor(row: &DesktopRow) -> bool {
    row.live && matches!(row.wm, Some("Hyprland" | "sway" | "niri"))
}

/// The first-start screen.
#[derive(Debug, Clone)]
pub struct Screen {
    opening: Opening,
    step: Step,
    /// Whether the screen has been shown at least once; a screen held from its opening is shown, with the
    /// detection started, when the hold ends.
    started: bool,
    /// Whether the user has pressed a key on the list: the preselection follows detection only until then.
    touched: bool,
    help: bool,
    tmux: Option<TmuxRow>,
    desktop: Option<DesktopRow>,
    windows: Option<Option<Tiling>>,
    neovim: Option<NeovimRow>,
    input: Option<InputRow>,
    preset: PresetId,
    prefix: Option<KeySpec>,
    wm_mod: Option<WmMod>,
    tiling_choice: Option<Tiling>,
    /// A prefix the press test has seen arrive.
    tested: Option<KeySpec>,
    /// Why the last prefix key was refused, as the sentence the screen shows.
    refused: Option<String>,
}

impl Screen {
    /// A screen for `opening`, held until [`Screen::shown`].
    pub fn new(opening: Opening) -> Screen {
        let mut screen = Screen {
            preset: PresetId::NvimNoTmux,
            step: Step::Held {
                resume: Box::new(Step::Choose { cursor: 0 }),
            },
            opening,
            started: false,
            touched: false,
            help: false,
            tmux: None,
            desktop: None,
            windows: None,
            neovim: None,
            input: None,
            prefix: None,
            wm_mod: None,
            tiling_choice: None,
            tested: None,
            refused: None,
        };
        screen.resync();
        screen
    }

    /// The generation the screen was opened with.
    pub fn generation(&self) -> u64 {
        self.opening.generation
    }

    pub fn step(&self) -> &Step {
        &self.step
    }

    /// Whether the keys belong to something else: the controller gives them on while this is true.
    pub fn held(&self) -> bool {
        matches!(self.step, Step::Held { .. })
    }

    /// The screen is up. A screen opened with a trust question already open stays held and asks for nothing.
    pub fn shown(&mut self) -> Vec<Effect> {
        if self.started || self.opening.trust_open {
            return Vec::new();
        }
        self.started = true;
        self.unhold();
        vec![Effect::FocusScreen, self.rescan()]
    }

    /// The rescan to ask for: the fixed chords always come, and the prefixes in play may not be among them.
    fn rescan(&self) -> Effect {
        let mut keys = vec![self.opening.running_prefix];
        keys.extend(self.prefix.filter(|chosen| *chosen != self.opening.running_prefix));
        Effect::Rescan(keys)
    }

    // ---- what detection and the build make of the choices ----

    fn lang(&self) -> Lang {
        self.opening.lang
    }

    fn build(&self) -> Build {
        self.opening.build
    }

    fn say(&self, id: TextId, args: &[(&str, &str)]) -> String {
        fill(text(self.lang(), id), args)
    }

    fn read_only(&self) -> bool {
        self.opening.mode == OpenMode::ShowOnly
    }

    fn tiling_live(&self) -> bool {
        self.desktop.as_ref().is_some_and(is_tiling_compositor)
    }

    fn offered(&self) -> Vec<PresetId> {
        presets::offered(self.build(), self.tiling_live())
    }

    fn cursor_of(&self, preset: PresetId) -> usize {
        self.offered().iter().position(|p| *p == preset).unwrap_or(0)
    }

    fn preselected_cursor(&self) -> usize {
        let row = self.tmux.clone().unwrap_or(TmuxRow {
            path: None,
            prefix: None,
            changes_keys: false,
        });
        self.cursor_of(preselect(&row, self.desktop.as_ref(), self.build()))
    }

    /// Moves the list's cursor to what detection preselects, until the user has pressed a key.
    fn resync(&mut self) {
        if self.touched {
            return;
        }
        let cursor = self.preselected_cursor();
        match &mut self.step {
            Step::Choose { cursor: at } => *at = cursor,
            Step::Held { resume } => {
                if let Step::Choose { cursor: at } = resume.as_mut() {
                    *at = cursor;
                }
            }
            _ => {}
        }
    }

    /// The tiling the file states: the user's `w`, else what `auto` resolves to here.
    fn tiling(&self) -> Tiling {
        self.tiling_choice.or(self.windows.flatten()).unwrap_or(Tiling::Unasked)
    }

    /// Whether the review offers `w`: a build with compositor mode, a live tiling compositor, and a preset that
    /// leaves the choice open.
    fn tiling_offered(&self) -> bool {
        self.build().compositor_mode
            && matches!(self.windows, Some(Some(_)))
            && matches!(
                self.preset,
                PresetId::NvimTmux | PresetId::NvimNoTmux | PresetId::OtherPrefix
            )
    }

    /// The prefix a window started from the written file has.
    fn effective_prefix(&self) -> KeySpec {
        if let Some(key) = self.prefix {
            return key;
        }
        match self.preset {
            PresetId::NvimNoTmux => stock_prefix(),
            PresetId::OtherPrefix => self.opening.running_prefix,
            _ => self.tmux.as_ref().and_then(|t| t.prefix).unwrap_or_else(stock_prefix),
        }
    }

    fn needs_press_test(&self) -> bool {
        let prefix = self.effective_prefix();
        prefix != self.opening.running_prefix && self.tested != Some(prefix)
    }

    fn choice(&self, preset: PresetId, press_failed: bool) -> Choice {
        let wm_mod = self
            .wm_mod
            .or(self.desktop.as_ref().and_then(|d| d.wm_mod))
            .unwrap_or(WmMod::Alt);
        let bound_as = self
            .prefix
            .and_then(|key| bound_in(&start_table(preset, wm_mod, &self.opening.keymap_check), &key));
        Choice {
            preset,
            prefix: self.prefix,
            wm_mod,
            tiling: self.tiling(),
            wm_name: self.desktop.as_ref().and_then(|d| d.wm),
            tmux_prefix: self.tmux.as_ref().and_then(|t| t.prefix),
            tmux_config_found: self.tmux.as_ref().is_some_and(|t| t.path.is_some()),
            running_prefix: self.opening.running_prefix,
            nvim_lhs: self.neovim.as_ref().and_then(|n| n.nvim_lhs),
            bound_as,
            press_failed,
            date: self.opening.today.clone(),
        }
    }

    fn generated(&self, press_failed: bool) -> Generated {
        presets::generate(&self.choice(self.preset, press_failed), self.build(), self.lang())
    }

    fn init_path(&self) -> String {
        format!("{}/init.lua", display_text(&self.opening.config_path))
    }

    fn shown_key(&self, key: &KeySpec) -> String {
        display_text(&key.human())
    }

    fn wm_name(&self) -> &'static str {
        self.desktop.as_ref().and_then(|d| d.wm).unwrap_or(UNNAMED_COMPOSITOR)
    }

    // ---- keys ----

    /// One key press, while the screen has the keys (or, in the press test, while the editor has them and the
    /// window stage reads them first).
    pub fn press(&mut self, press: &Press) -> Vec<Effect> {
        if self.held() || *press == Press::Modifier {
            return Vec::new();
        }
        if let Some(by) = self.scroll_of(classify(press)) {
            return vec![Effect::Scroll(by)];
        }
        match self.step.clone() {
            Step::Held { .. } => Vec::new(),
            Step::Choose { cursor } => self.on_choose(cursor, classify(press)),
            Step::SubPrefix => self.on_sub_prefix(classify(press)),
            Step::SubKey => self.on_sub_key(press),
            Step::SubMod => self.on_sub_mod(classify(press)),
            Step::Review => self.on_review(classify(press)),
            Step::Press { want, timed_out, .. } => self.on_press_test(press, want, timed_out),
            Step::Writing => Vec::new(),
            Step::Done { .. } => self.on_done(classify(press)),
            Step::Failed { .. } => self.on_failed(classify(press), false),
            Step::NotWritten { .. } => self.on_failed(classify(press), true),
        }
    }

    /// The scroll `key` means on this step, if any. Every step whose keys are the screen's scrolls by page; a step
    /// that reads `j`, `k` or the arrows for itself keeps them; and the two steps that read any key as the answer (the
    /// prefix key, the running press test) leave every key alone.
    fn scroll_of(&self, key: In) -> Option<Scroll> {
        let lines = match &self.step {
            Step::Review | Step::Done { .. } | Step::Failed { .. } | Step::NotWritten { .. } => true,
            Step::Press { timed_out, .. } if *timed_out => true,
            Step::Choose { .. } | Step::SubPrefix | Step::SubMod => false,
            Step::SubKey | Step::Press { .. } | Step::Writing | Step::Held { .. } => return None,
        };
        match key {
            In::PageUp => Some(Scroll::PageUp),
            In::PageDown => Some(Scroll::PageDown),
            In::Up | In::Char('k') if lines => Some(Scroll::LineUp),
            In::Down | In::Char('j') if lines => Some(Scroll::LineDown),
            _ => None,
        }
    }

    /// Leaving without writing: a first start remembers the "not now"; a screen the user asked for does not.
    fn leave(&self) -> Vec<Effect> {
        if self.opening.mode == OpenMode::First {
            vec![Effect::Marker(MarkerEvent::NotNow), Effect::Close]
        } else {
            vec![Effect::Close]
        }
    }

    fn toggle_help(&mut self) -> Vec<Effect> {
        self.help = !self.help;
        Vec::new()
    }

    fn on_choose(&mut self, cursor: usize, key: In) -> Vec<Effect> {
        let offered = self.offered();
        let last = offered.len().saturating_sub(1);
        self.touched = true;
        match key {
            In::Esc => self.leave(),
            // A show-only screen never opens by itself, so there is nothing to decline there.
            In::Char('d') if self.read_only() => Vec::new(),
            In::Char('d') => vec![Effect::Marker(MarkerEvent::Declined), Effect::Close],
            In::Char('j') | In::Down => {
                self.step = Step::Choose {
                    cursor: (cursor + 1).min(last),
                };
                Vec::new()
            }
            In::Char('k') | In::Up => {
                self.step = Step::Choose {
                    cursor: cursor.saturating_sub(1),
                };
                Vec::new()
            }
            In::Char(digit @ '1'..='9') => {
                let at = digit as usize - '1' as usize;
                if at < offered.len() {
                    self.step = Step::Choose { cursor: at };
                }
                Vec::new()
            }
            In::Char('r') => {
                self.neovim = None;
                vec![self.rescan()]
            }
            In::Char('?') => self.toggle_help(),
            In::Enter => {
                self.preset = offered[cursor.min(last)];
                self.prefix = None;
                self.tested = None;
                self.wm_mod = None;
                self.refused = None;
                self.step = match self.preset {
                    PresetId::OtherPrefix => Step::SubPrefix,
                    PresetId::I3Style => Step::SubMod,
                    _ => Step::Review,
                };
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Takes `key` as the prefix unless the keymap refuses it, in which case the step stays and says why.
    fn take_prefix(&mut self, key: KeySpec) {
        match prefix_refusal(&key) {
            Some(why) => {
                let shown = self.shown_key(&key);
                self.refused = Some(self.say(TextId::RefuseKey, &[("key", &shown), ("why", &why)]));
            }
            None => {
                self.refused = None;
                self.prefix = Some(key);
                self.tested = None;
                self.step = Step::Review;
            }
        }
    }

    fn on_sub_prefix(&mut self, key: In) -> Vec<Effect> {
        match key {
            In::Esc => return self.leave(),
            In::Char('a') => self.take_prefix(parsed("C-a")),
            In::Char('s') => self.take_prefix(ctrl_space()),
            In::Char('k') => {
                self.refused = None;
                self.step = Step::SubKey;
            }
            In::Back => {
                self.refused = None;
                self.step = if self.preset == PresetId::OtherPrefix {
                    Step::Choose {
                        cursor: self.cursor_of(self.preset),
                    }
                } else {
                    Step::Review
                };
            }
            In::Char('?') => return self.toggle_help(),
            _ => {}
        }
        Vec::new()
    }

    fn on_sub_key(&mut self, press: &Press) -> Vec<Effect> {
        match classify(press) {
            In::Esc => return self.leave(),
            In::Back => {
                self.refused = None;
                self.step = Step::SubPrefix;
                return Vec::new();
            }
            _ => {}
        }
        match press {
            Press::Key(key) => {
                self.take_prefix(*key);
                // Who has the key in the editor: a key of the user's own is not among the fixed chords.
                if self.prefix == Some(*key) {
                    return vec![self.rescan()];
                }
            }
            _ => {
                let (key, why) = (
                    text(self.lang(), TextId::RefuseUnnamedKey),
                    text(self.lang(), TextId::RefuseUnnamedWhy),
                );
                self.refused = Some(self.say(TextId::RefuseKey, &[("key", key), ("why", why)]));
            }
        }
        Vec::new()
    }

    fn on_sub_mod(&mut self, key: In) -> Vec<Effect> {
        match key {
            In::Esc => return self.leave(),
            In::Char('a') => {
                self.wm_mod = Some(WmMod::Alt);
                self.step = Step::Review;
            }
            In::Char('s') => {
                self.wm_mod = Some(WmMod::Super);
                self.step = Step::Review;
            }
            In::Back => {
                self.step = Step::Choose {
                    cursor: self.cursor_of(self.preset),
                };
            }
            In::Char('?') => return self.toggle_help(),
            _ => {}
        }
        Vec::new()
    }

    fn write(&mut self, press_failed: bool) -> Vec<Effect> {
        self.step = Step::Writing;
        vec![Effect::Write(self.generated(press_failed))]
    }

    fn on_review(&mut self, key: In) -> Vec<Effect> {
        let read_only = self.read_only();
        match key {
            In::Esc => self.leave(),
            In::Enter if read_only => Vec::new(),
            In::Enter if self.needs_press_test() => {
                self.step = Step::Press {
                    want: self.effective_prefix(),
                    seen: None,
                    timed_out: false,
                };
                vec![Effect::FocusBack, Effect::ArmPressTimer]
            }
            In::Enter => self.write(false),
            In::Back => {
                self.step = match self.preset {
                    PresetId::OtherPrefix => Step::SubPrefix,
                    PresetId::I3Style => Step::SubMod,
                    preset => Step::Choose {
                        cursor: self.cursor_of(preset),
                    },
                };
                Vec::new()
            }
            In::Char('p') if !read_only => {
                self.refused = None;
                self.step = Step::SubPrefix;
                Vec::new()
            }
            In::Char('w') if self.tiling_offered() => {
                self.tiling_choice = Some(match self.tiling() {
                    Tiling::Compositor => Tiling::Eitri,
                    _ => Tiling::Compositor,
                });
                Vec::new()
            }
            In::Char('o') if read_only => self.open_lines(),
            In::Char('?') => self.toggle_help(),
            _ => Vec::new(),
        }
    }

    /// The lines in a scratch buffer. The screen closes first, so the keys are back where the buffer opens; a
    /// first start remembers that nothing was written.
    fn open_lines(&self) -> Vec<Effect> {
        let mut effects = Vec::new();
        if self.opening.mode == OpenMode::First {
            effects.push(Effect::Marker(MarkerEvent::NotNow));
        }
        effects.push(Effect::Close);
        effects.push(Effect::OpenLines(self.generated(false).lines));
        effects
    }

    fn on_press_test(&mut self, press: &Press, want: KeySpec, timed_out: bool) -> Vec<Effect> {
        if timed_out {
            match classify(press) {
                In::Esc | In::Back => self.step = Step::Review,
                In::Char('w') => return self.write(true),
                In::Char('p') => {
                    self.refused = None;
                    self.step = Step::SubPrefix;
                }
                In::Char('?') => return self.toggle_help(),
                _ => {}
            }
            return Vec::new();
        }
        match press {
            Press::Escape => {
                self.step = Step::Review;
                vec![Effect::CancelPressTimer, Effect::FocusScreen]
            }
            Press::Key(key) if *key == want => {
                self.tested = Some(want);
                self.step = Step::Review;
                vec![Effect::CancelPressTimer, Effect::FocusScreen]
            }
            other => {
                let got = match other {
                    Press::Key(key) => self.shown_key(key),
                    _ => text(self.lang(), TextId::RefuseUnnamedKey).to_string(),
                };
                if let Step::Press { seen, .. } = &mut self.step {
                    *seen = Some(got);
                }
                Vec::new()
            }
        }
    }

    /// The timer the screen armed ran out.
    pub fn press_timed_out(&mut self) -> Vec<Effect> {
        match &mut self.step {
            Step::Press { timed_out, .. } if !*timed_out => {
                *timed_out = true;
                vec![Effect::FocusScreen]
            }
            _ => Vec::new(),
        }
    }

    fn on_done(&mut self, key: In) -> Vec<Effect> {
        match key {
            In::Char('t') => vec![Effect::Close, Effect::OpenTutor],
            In::Char('?') => self.toggle_help(),
            In::Enter | In::Esc => vec![Effect::Close],
            _ => Vec::new(),
        }
    }

    fn on_failed(&mut self, key: In, lines_shown: bool) -> Vec<Effect> {
        match key {
            In::Esc => self.leave(),
            In::Back => {
                self.step = Step::Review;
                Vec::new()
            }
            In::Char('o') if lines_shown => self.open_lines(),
            In::Char('?') => self.toggle_help(),
            _ => Vec::new(),
        }
    }

    // ---- answers from workers ----

    /// A detection answer, or the trust question opening or closing.
    pub fn found(&mut self, found: Found) -> Vec<Effect> {
        match found {
            Found::Tmux(row) => self.tmux = Some(row),
            Found::Desktop(row) => self.desktop = Some(row),
            Found::Windows(tiling) => self.windows = Some(tiling),
            Found::Neovim(row) => self.neovim = Some(row),
            Found::Input(row) => self.input = Some(row),
            Found::TrustOpen(open) => return if open { self.hold() } else { self.release() },
        }
        self.resync();
        Vec::new()
    }

    fn hold(&mut self) -> Vec<Effect> {
        let effects = match &self.step {
            Step::Held { .. } => return Vec::new(),
            // The keys are with the module already, and the timer must not run out under the question.
            Step::Press { timed_out: false, .. } => vec![Effect::CancelPressTimer],
            _ => vec![Effect::FocusBack],
        };
        let resume = std::mem::replace(&mut self.step, Step::Writing);
        self.step = Step::Held {
            resume: Box::new(resume),
        };
        effects
    }

    /// Takes the screen out of `Held` into what it resumes.
    fn unhold(&mut self) {
        if let Step::Held { resume } = &mut self.step {
            let resumed = std::mem::replace(resume.as_mut(), Step::Writing);
            self.step = resumed;
        }
    }

    fn release(&mut self) -> Vec<Effect> {
        if !self.held() {
            return Vec::new();
        }
        self.unhold();
        if !self.started {
            self.started = true;
            return vec![Effect::FocusScreen, self.rescan()];
        }
        match self.step {
            Step::Press { timed_out: false, .. } => vec![Effect::ArmPressTimer],
            _ => vec![Effect::FocusScreen],
        }
    }

    /// The step a worker's answer about writing replaces, whether the screen is shown or held.
    fn writing_step(&mut self) -> Option<&mut Step> {
        if self.step == Step::Writing {
            return Some(&mut self.step);
        }
        match &mut self.step {
            Step::Held { resume } if **resume == Step::Writing => Some(resume.as_mut()),
            _ => None,
        }
    }

    /// How writing ended. An answer while nothing is being written is dropped; one that arrives under a hold
    /// changes what resumes and moves no focus.
    pub fn written(&mut self, outcome: WriteOutcome) -> Vec<Effect> {
        let next = match outcome {
            WriteOutcome::Written(path) => Step::Done {
                path: display_text(&path.display().to_string()),
            },
            WriteOutcome::Failed(why) => Step::Failed { why },
            WriteOutcome::NotWritten(why) => Step::NotWritten { why },
        };
        if let Some(step) = self.writing_step() {
            *step = next;
        }
        Vec::new()
    }

    /// The check before writing said no.
    pub fn check_failed(&mut self, why: CheckWhy) -> Vec<Effect> {
        let why = match why {
            CheckWhy::Unverified => self.say(TextId::NotwrittenCheck, &[]),
            CheckWhy::Refused(message) => self.say(TextId::FailIo, &[("path", &self.init_path()), ("why", &message)]),
        };
        if let Some(step) = self.writing_step() {
            *step = Step::NotWritten { why };
        }
        Vec::new()
    }

    // ---- the view ----

    /// What the shell draws; `None` while the screen is held.
    pub fn view(&self) -> Option<OnboardingView> {
        if self.held() {
            return None;
        }
        let lang = self.lang();
        let mut view = OnboardingView {
            title: text(lang, TextId::Title).to_string(),
            step: String::new(),
            rows: Vec::new(),
            question: None,
            items: Vec::new(),
            body: Vec::new(),
            status: Vec::new(),
            footer: String::new(),
            buttons: Vec::new(),
            help: self.help.then(|| text(lang, TextId::HelpLine).to_string()),
        };
        match &self.step {
            Step::Held { .. } => return None,
            Step::Choose { cursor } => self.view_choose(&mut view, *cursor),
            Step::SubPrefix => self.view_sub_prefix(&mut view),
            Step::SubKey => {
                self.view_head(&mut view);
                view.question = Some(text(lang, TextId::SubPressKey).to_string());
                view.status.extend(self.refused.clone());
                view.footer = self.footer(TextId::FootFailed);
            }
            Step::SubMod => self.view_sub_mod(&mut view),
            Step::Review => self.view_review(&mut view),
            Step::Press { want, seen, timed_out } => self.view_press(&mut view, *want, seen.as_deref(), *timed_out),
            Step::Writing => {
                self.view_head(&mut view);
                view.body = self.generated(false).lines;
            }
            Step::Done { path } => self.view_done(&mut view, path),
            Step::Failed { why } => {
                view.status.push(why.clone());
                view.footer = self.footer(TextId::FootFailed);
            }
            Step::NotWritten { why } => {
                view.status.push(why.clone());
                view.body = self.generated(false).lines;
                view.footer = self.footer(TextId::FootShow);
            }
        }
        view.buttons.extend(self.footer_buttons(&view.footer));
        Some(view)
    }

    /// Footer `id` as this screen says it: its count of presets is what this build offers here.
    fn footer(&self, id: TextId) -> String {
        self.say(id, &[("n", &self.offered().len().to_string())])
    }

    /// The buttons the footer's own words name: each of its segments (the footer's parts are set apart by ` · `)
    /// that names a key is a button with the segment as its label. The footer is one of the screen's own texts, so
    /// the segment it was made of is found by comparing it, and the keys each segment names are [`footer_keys`]'s.
    fn footer_buttons(&self, footer: &str) -> Vec<ViewButton> {
        let Some(id) = FOOTERS.iter().copied().find(|id| self.footer(*id) == footer) else {
            return Vec::new();
        };
        footer
            .split(FOOTER_SEPARATOR)
            .zip(footer_keys(id))
            .filter_map(|(segment, key)| key.map(|key| ViewButton::new(key, segment)))
            .collect()
    }

    /// The second page's title and counter: the preset the user chose.
    fn view_head(&self, view: &mut OnboardingView) {
        view.title = presets::title(self.preset, self.lang()).to_string();
        view.step = "2 / 2".to_string();
    }

    fn row(&self, label: TextId, body: String, note: String) -> ViewRow {
        ViewRow {
            label: text(self.lang(), label).to_string(),
            text: body,
            note,
        }
    }

    fn view_choose(&self, view: &mut OnboardingView, cursor: usize) {
        let lang = self.lang();
        view.step = "1 / 2".to_string();
        let looking = || text(lang, TextId::RowLooking).to_string();
        let not_found = || text(lang, TextId::RowNotFound).to_string();
        let selected = self.offered().get(cursor).copied().unwrap_or(PresetId::NvimNoTmux);

        view.rows.push(match &self.tmux {
            None => self.row(TextId::RowTmux, looking(), String::new()),
            Some(row) => match &row.path {
                None => self.row(TextId::RowTmux, not_found(), String::new()),
                Some(path) => {
                    let key = self.shown_key(&row.prefix.unwrap_or_else(stock_prefix));
                    let found = self.say(TextId::TmuxFound, &[("path", &display_text(path)), ("key", &key)]);
                    let note = if selected == PresetId::NvimTmux {
                        text(lang, TextId::TmuxWillUse).to_string()
                    } else {
                        String::new()
                    };
                    self.row(TextId::RowTmux, found, note)
                }
            },
        });

        view.rows.push(match &self.desktop {
            None => self.row(TextId::RowDesktop, looking(), String::new()),
            Some(row) => self.row(TextId::RowDesktop, self.desktop_text(row), String::new()),
        });

        if self.build().compositor_mode && self.windows == Some(Some(Tiling::Compositor)) {
            view.rows.push(self.row(
                TextId::RowWindows,
                self.say(TextId::WindowsAuto, &[("wm", self.wm_name())]),
                text(lang, TextId::WindowsHint).to_string(),
            ));
        }

        match &self.neovim {
            None => view.rows.push(self.row(
                TextId::RowNeovim,
                text(lang, TextId::RowReadingNvim).to_string(),
                String::new(),
            )),
            Some(row) if row.rows.is_empty() => {
                view.rows.push(self.row(TextId::RowNeovim, not_found(), String::new()));
            }
            Some(row) => {
                for (at, line) in row.rows.iter().enumerate() {
                    view.rows.push(ViewRow {
                        label: if at == 0 {
                            text(lang, TextId::RowNeovim).to_string()
                        } else {
                            String::new()
                        },
                        text: line.clone(),
                        note: String::new(),
                    });
                }
            }
        }

        view.rows.push(match &self.input {
            None => self.row(TextId::RowInput, looking(), String::new()),
            Some(InputRow { im: Some(im), .. }) => self.row(
                TextId::RowInput,
                self.say(TextId::InputRunning, &[("im", im)]),
                String::new(),
            ),
            Some(InputRow { maybe: true, .. }) => self.row(
                TextId::RowInput,
                text(lang, TextId::InputMaybe).to_string(),
                String::new(),
            ),
            Some(_) => self.row(TextId::RowInput, not_found(), String::new()),
        });

        view.question = Some(text(lang, TextId::Question).to_string());
        let tiling = self.tiling();
        for (at, id) in self.offered().into_iter().enumerate() {
            let choice = self.choice(id, false);
            view.items.push(ViewItem {
                number: at as u32 + 1,
                name: presets::title(id, lang).to_string(),
                summary: presets::summary(id, &choice, tiling, lang),
                tag: (id == PresetId::NvimTmux && self.tmux.as_ref().is_some_and(|t| t.path.is_some()))
                    .then(|| text(lang, TextId::TagFound).to_string()),
                selected: at == cursor,
            });
        }
        view.footer = self.footer(if self.read_only() {
            TextId::FootChooseShow
        } else {
            TextId::FootChoose
        });
    }

    fn desktop_text(&self, row: &DesktopRow) -> String {
        let lang = self.lang();
        if row.note.as_deref() == Some(NOTE_HYPR_LUA) {
            return text(lang, TextId::DesktopHyprLua).to_string();
        }
        let Some(wm) = row.wm else {
            return text(lang, TextId::RowNotFound).to_string();
        };
        let mut out = self.say(
            if row.live {
                TextId::DesktopRunning
            } else {
                TextId::DesktopNamed
            },
            &[("wm", wm)],
        );
        if let Some(wm_mod) = row.wm_mod {
            out.push_str(" · ");
            out.push_str(text(
                lang,
                match wm_mod {
                    WmMod::Alt => TextId::DesktopModAlt,
                    WmMod::Super => TextId::DesktopModSuper,
                },
            ));
        }
        out
    }

    fn has_input_method(&self) -> bool {
        self.input.as_ref().is_some_and(|i| i.im.is_some() || i.maybe)
    }

    fn im_warning(&self) -> String {
        text(self.lang(), TextId::PressImWarning).to_string()
    }

    /// The warnings that depend on the prefix the file would set. Nothing here refuses a key.
    fn prefix_warnings(&self) -> Vec<String> {
        let prefix = self.effective_prefix();
        let mut out = Vec::new();
        if let Some(neovim) = &self.neovim {
            let owners = neovim.chord_lines.iter().filter(|(chord, _)| *chord == prefix);
            out.extend(owners.map(|(_, line)| line.clone()));
        }
        if prefix == ctrl_space() && self.has_input_method() {
            out.push(self.im_warning());
        }
        if let Some(desktop) = &self.desktop {
            if prefix_is_a_compositor_bind(&prefix, desktop) {
                out.push(self.say(
                    TextId::PressBindWarning,
                    &[("key", &self.shown_key(&prefix)), ("wm", self.wm_name())],
                ));
            }
        }
        out
    }

    fn view_sub_prefix(&self, view: &mut OnboardingView) {
        let lang = self.lang();
        self.view_head(view);
        view.question = Some(text(lang, TextId::SubPrefix).to_string());
        for (key, id) in [
            ('a', TextId::SubPrefixA),
            ('s', TextId::SubPrefixS),
            ('k', TextId::SubPrefixK),
        ] {
            view.buttons.push(ViewButton {
                key: ViewKey::Char(key),
                label: text(lang, id).to_string(),
            });
        }
        if self.has_input_method() {
            view.status.push(self.im_warning());
        }
        view.status.extend(self.refused.clone());
        view.footer = self.footer(TextId::FootFailed);
    }

    fn view_sub_mod(&self, view: &mut OnboardingView) {
        let lang = self.lang();
        self.view_head(view);
        view.question = Some(text(lang, TextId::SubMod).to_string());
        let detected = self.desktop.as_ref().and_then(|d| d.wm_mod);
        for (key, id, wm_mod) in [('a', TextId::SubModA, WmMod::Alt), ('s', TextId::SubModS, WmMod::Super)] {
            let mut label = text(lang, id).to_string();
            if detected == Some(wm_mod) {
                label.push(' ');
                label.push_str(text(lang, TextId::SubDetected));
            }
            view.buttons.push(ViewButton {
                key: ViewKey::Char(key),
                label,
            });
        }
        view.footer = self.footer(TextId::FootFailed);
    }

    fn view_review(&self, view: &mut OnboardingView) {
        let lang = self.lang();
        self.view_head(view);
        view.body = self.generated(false).lines;
        let prefix = self.effective_prefix();
        let key = self.shown_key(&prefix);
        if self.read_only() {
            view.question = Some(match &self.opening.show_why {
                Some(ShowWhy::RefusedDir(_)) => {
                    self.say(TextId::FailDir, &[("path", &display_text(&self.opening.config_path))])
                }
                _ => text(lang, TextId::ShowExists).to_string(),
            });
            view.footer = self.footer(TextId::FootShow);
        } else {
            view.question = Some(self.say(TextId::ReviewWrites, &[("path", &self.init_path())]));
            view.footer = self.footer(if self.needs_press_test() {
                TextId::FootReviewPress
            } else {
                TextId::FootReview
            });
            view.buttons.push(ViewButton {
                key: ViewKey::Char('p'),
                label: self.say(TextId::ReviewPrefix, &[("key", &key)]),
            });
        }
        if self.tiling_offered() {
            let id = if self.tiling() == Tiling::Compositor {
                TextId::ReviewWindowsWm
            } else {
                TextId::ReviewWindowsEitri
            };
            view.buttons.push(ViewButton {
                key: ViewKey::Char('w'),
                label: self.say(id, &[("wm", self.wm_name())]),
            });
        }
        if self.preset == PresetId::I3Style && self.build().compositor_mode && self.tiling_live() {
            view.status
                .push(self.say(TextId::ReviewWmWould, &[("wm", self.wm_name())]));
        }
        let sets_prefix = self.prefix.is_some() || self.preset == PresetId::OtherPrefix;
        if let Some(tmux_prefix) = self.tmux.as_ref().and_then(|t| t.prefix) {
            if sets_prefix && tmux_prefix != prefix {
                let shown = self.shown_key(&tmux_prefix);
                view.status.push(self.say(TextId::ReviewTmuxWins, &[("key", &shown)]));
            }
        }
        view.status.extend(self.prefix_warnings());
        if self.tested == Some(prefix) && !self.read_only() {
            view.status.push(self.say(TextId::PressArrived, &[("key", &key)]));
        }
    }

    fn view_press(&self, view: &mut OnboardingView, want: KeySpec, seen: Option<&str>, timed_out: bool) {
        let lang = self.lang();
        self.view_head(view);
        view.body = self.generated(false).lines;
        let key = self.shown_key(&want);
        view.question = Some(self.say(TextId::PressAsk, &[("key", &key)]));
        view.status.extend(self.prefix_warnings());
        if timed_out {
            let running = self.shown_key(&self.opening.running_prefix);
            view.status
                .push(self.say(TextId::PressNever, &[("key", &key), ("running", &running)]));
            // The test runs out by itself, so a mouse alone lands here: what the status offers is clickable.
            view.buttons.push(ViewButton {
                key: ViewKey::Char('p'),
                label: self.say(TextId::ReviewPrefix, &[("key", &key)]),
            });
            view.buttons.push(ViewButton::new(
                ViewKey::Char('w'),
                text(lang, TextId::PressWriteAnyway),
            ));
            view.buttons.push(ViewButton::new(
                ViewKey::Back,
                footer_part(lang, TextId::FootFailed, ViewKey::Back),
            ));
        } else {
            // The test ends by itself only after a wait, so the way out of it is named while it runs.
            view.footer = self.footer(TextId::FootPress);
            if let Some(got) = seen {
                view.status
                    .push(self.say(TextId::PressOther, &[("got", got), ("key", &key)]));
            }
        }
    }

    fn view_done(&self, view: &mut OnboardingView, path: &str) {
        let running = self.shown_key(&self.opening.running_prefix);
        view.body
            .push(self.say(TextId::DoneWritten, &[("path", path), ("key", &running)]));
        // Presets that leave the tmux import on make the stock keys after the prefix whatever tmux says.
        let imported = self.tmux.as_ref().is_some_and(|t| t.changes_keys) && self.preset != PresetId::NvimNoTmux;
        if imported {
            let key = self.shown_key(&self.effective_prefix());
            view.body.push(self.say(TextId::DoneTutorStock, &[("key", &key)]));
        }
        let sets_tiling = self
            .generated(false)
            .settings
            .iter()
            .any(|(key, _)| key == presets::TILING_KEY);
        if sets_tiling {
            view.body.push(self.say(TextId::DoneWindowsNext, &[]));
        }
        // The tutor can be opened in this run or the next, so either of them being in that mode counts.
        let opens_as_windows = self.windows == Some(Some(Tiling::Compositor)) || self.tiling() == Tiling::Compositor;
        if self.build().compositor_mode && opens_as_windows {
            view.body
                .push(self.say(TextId::DoneTutorWindows, &[("wm", self.wm_name())]));
        }
        view.footer = self.footer(TextId::FootDone);
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    use super::*;
    use crate::keymap::Chord;
    use crate::onboarding::trigger::reopen_mode;

    macro_rules! nothing {
        ($effects:expr) => {
            assert_eq!($effects, Vec::<Effect>::new())
        };
    }

    fn key(spelling: &str) -> KeySpec {
        KeySpec::parse(spelling).unwrap()
    }

    fn k(c: char) -> Press {
        Press::Key(KeySpec::char(c, false, false))
    }

    fn named(name: KeyName) -> Press {
        Press::Key(KeySpec::named(name, false, false, false))
    }

    fn chord(spelling: &str) -> Press {
        Press::Key(key(spelling))
    }

    fn enter() -> Press {
        named(KeyName::Enter)
    }

    fn backspace() -> Press {
        named(KeyName::BSpace)
    }

    fn opening(mode: OpenMode) -> Opening {
        Opening {
            generation: 1,
            mode,
            show_why: None,
            trust_open: false,
            lang: Lang::En,
            build: Build::THIS_BINARY,
            running_prefix: key("C-b"),
            keymap_check: Keymap::defaults()
                .bindings()
                .iter()
                .map(|b| (b.key, b.action.name()))
                .collect(),
            today: "2026-10-31".to_string(),
            config_path: "~/.config/eitri".to_string(),
        }
    }

    fn shown(opening: Opening) -> Screen {
        let mut screen = Screen::new(opening);
        let running = screen.opening.running_prefix;
        assert_eq!(screen.shown(), [Effect::FocusScreen, Effect::Rescan(vec![running])]);
        screen
    }

    fn screen_in(mode: OpenMode) -> Screen {
        shown(opening(mode))
    }

    fn screen() -> Screen {
        screen_in(OpenMode::First)
    }

    /// A screen of a window that already runs with `prefix`, as one whose tmux configuration sets it does.
    fn screen_running(prefix: &str) -> Screen {
        let mut o = opening(OpenMode::First);
        o.running_prefix = key(prefix);
        shown(o)
    }

    fn tmux(prefix: &str) -> Found {
        Found::Tmux(TmuxRow {
            path: Some("~/.tmux.conf".to_string()),
            prefix: Some(key(prefix)),
            changes_keys: false,
        })
    }

    fn desktop(wm: &'static str, binds: Vec<Chord>) -> Found {
        Found::Desktop(DesktopRow {
            wm: Some(wm),
            live: true,
            wm_mod: None,
            note: None,
            binds,
        })
    }

    fn input(im: &'static str) -> Found {
        Found::Input(InputRow {
            im: Some(im),
            maybe: false,
        })
    }

    fn view(screen: &Screen) -> OnboardingView {
        screen.view().expect("the screen is shown")
    }

    fn status(screen: &Screen) -> String {
        view(screen).status.join("\n")
    }

    fn en(id: TextId) -> &'static str {
        text(Lang::En, id)
    }

    fn selected(screen: &Screen) -> PresetId {
        let Step::Choose { cursor } = screen.step() else {
            panic!("not on the list: {:?}", screen.step());
        };
        screen.offered()[*cursor]
    }

    /// Chooses the preset with `digit` and goes on.
    fn go(screen: &mut Screen, digit: char) {
        screen.press(&k(digit));
        screen.press(&enter());
    }

    fn written_text(effects: &[Effect]) -> String {
        match effects {
            [Effect::Write(generated)] => generated.text(),
            other => panic!("expected one Write, got {other:?}"),
        }
    }

    fn private_dir(path: &std::path::Path) {
        std::fs::create_dir_all(path).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let dir = std::env::temp_dir().join(format!("eitri-screen-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            private_dir(&dir);
            Scratch(dir)
        }

        fn config(&self) -> PathBuf {
            let config = self.0.join("config");
            private_dir(&config);
            config
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn opened_from(mode: OpenMode, why: Option<ShowWhy>) -> Screen {
        let mut o = opening(mode);
        o.show_why = why;
        shown(o)
    }

    /// A screen whose press test is running on the prefix `letter` chooses (`a` Ctrl+a, `s` Ctrl+Space).
    fn press_test_on(letter: char) -> Screen {
        let mut s = screen();
        go(&mut s, '3');
        s.press(&k(letter));
        assert_eq!(s.press(&enter()), [Effect::FocusBack, Effect::ArmPressTimer]);
        s
    }

    #[test]
    fn the_preselection_moves_with_detection_until_a_key_is_pressed() {
        let mut s = screen();
        assert_eq!(selected(&s), PresetId::NvimNoTmux);
        s.found(tmux("C-a"));
        assert_eq!(selected(&s), PresetId::NvimTmux);
        s.found(Found::Tmux(TmuxRow {
            path: None,
            prefix: None,
            changes_keys: false,
        }));
        assert_eq!(selected(&s), PresetId::NvimNoTmux);
        s.press(&k('j'));
        s.press(&k('k'));
        assert_eq!(selected(&s), PresetId::NvimNoTmux);
        s.found(tmux("C-a"));
        assert_eq!(
            selected(&s),
            PresetId::NvimNoTmux,
            "the user moved: detection no longer does"
        );
    }

    #[test]
    fn a_row_says_looking_until_found() {
        let mut s = screen();
        let rows = view(&s).rows;
        assert_eq!(rows[0].text, en(TextId::RowLooking));
        assert_eq!(rows[1].text, en(TextId::RowLooking));
        assert_eq!(rows[2].text, en(TextId::RowReadingNvim));
        assert_eq!(rows[3].text, en(TextId::RowLooking));
        s.found(tmux("C-a"));
        s.found(desktop("sway", Vec::new()));
        s.found(input("fcitx5"));
        s.found(Found::Neovim(NeovimRow {
            rows: vec![
                "Ctrl+h/j/k/l: vim-tmux-navigator (works as is)".into(),
                "leader: Space".into(),
            ],
            nvim_lhs: None,
            chord_lines: Vec::new(),
        }));
        let rows = view(&s).rows;
        assert_eq!(rows[0].text, "~/.tmux.conf: prefix Ctrl+a");
        assert_eq!(rows[1].text, "sway, running");
        assert_eq!(rows[2].label, "Neovim");
        assert_eq!(rows[3].label, "");
        assert_eq!(rows[3].text, "leader: Space");
        assert_eq!(rows[4].text, "fcitx5 is running: Ctrl+Space may switch input methods");
        s.found(Found::Input(InputRow { im: None, maybe: false }));
        assert_eq!(view(&s).rows[4].text, en(TextId::RowNotFound));
    }

    #[test]
    fn the_selected_tmux_preset_marks_the_row_as_used() {
        let mut s = screen();
        s.found(tmux("C-a"));
        assert_eq!(selected(&s), PresetId::NvimTmux);
        let v = view(&s);
        assert_eq!(v.rows[0].note, en(TextId::TmuxWillUse));
        assert_eq!(v.items[0].tag.as_deref(), Some(en(TextId::TagFound)));
        assert!(v.items[0].selected);
        assert_eq!(v.items[0].summary, "Ctrl+a, then a key, as in tmux");
        s.press(&k('2'));
        assert_eq!(view(&s).rows[0].note, "");
    }

    #[test]
    fn esc_is_marker_not_now_and_close() {
        let not_now = vec![Effect::Marker(MarkerEvent::NotNow), Effect::Close];
        let mut s = screen();
        assert_eq!(s.press(&Press::Escape), not_now);

        let mut s = screen();
        s.press(&enter());
        assert_eq!(s.step(), &Step::Review);
        assert_eq!(s.press(&Press::Escape), not_now);

        for outcome in [
            WriteOutcome::Failed("an init.lua appeared meanwhile".into()),
            WriteOutcome::NotWritten("this file system cannot create init.lua safely".into()),
        ] {
            let mut s = screen();
            s.press(&enter());
            s.press(&enter());
            assert_eq!(s.step(), &Step::Writing);
            s.written(outcome);
            assert_eq!(s.press(&Press::Escape), not_now, "{:?}", s.step());
        }

        let mut s = screen();
        go(&mut s, '3');
        assert_eq!(s.step(), &Step::SubPrefix);
        assert_eq!(s.press(&Press::Escape), not_now);
    }

    #[test]
    fn esc_in_a_reopened_screen_touches_no_marker() {
        let scratch = Scratch::new("reopen");
        let config = scratch.config();
        let (mode, why) = reopen_mode(&config);
        assert_eq!(mode, OpenMode::Asked);
        let mut s = opened_from(mode, why);
        assert_eq!(s.press(&Press::Escape), [Effect::Close]);

        std::fs::write(config.join("init.lua"), "-- mine\n").unwrap();
        let (mode, why) = reopen_mode(&config);
        assert_eq!(mode, OpenMode::ShowOnly);
        let mut s = opened_from(mode, why);
        assert_eq!(s.press(&Press::Escape), [Effect::Close]);
        s.press(&enter());
        assert_eq!(s.press(&Press::Escape), [Effect::Close]);
    }

    #[test]
    fn show_only_runs_choose_then_a_read_only_review() {
        let scratch = Scratch::new("showonly");
        let config = scratch.config();
        std::fs::write(config.join("init.lua"), "-- mine\n").unwrap();
        let (mode, why) = reopen_mode(&config);
        let mut s = opened_from(mode, why);
        assert!(matches!(s.step(), Step::Choose { .. }));
        nothing!(s.press(&enter()));
        assert_eq!(s.step(), &Step::Review);
        let v = view(&s);
        assert_eq!(v.question.as_deref(), Some(en(TextId::ShowExists)));
        assert!(!v.body.is_empty());
        assert!(v.buttons.iter().all(|b| b.key != 'p'));
        assert_eq!(v.footer, en(TextId::FootShow));
        nothing!(s.press(&enter()));
        nothing!(s.press(&k('p')));
        assert_eq!(s.step(), &Step::Review);
        let lines = v.body.clone();
        assert_eq!(s.press(&k('o')), [Effect::Close, Effect::OpenLines(lines)]);
    }

    #[test]
    fn show_only_offers_o_and_never_a_write() {
        let mut s = opened_from(OpenMode::ShowOnly, Some(ShowWhy::HasInit));
        go(&mut s, '3');
        s.press(&k('s'));
        assert_eq!(s.step(), &Step::Review);
        for press in [enter(), k('w'), k('p'), k('y'), chord("C-Space")] {
            let effects = s.press(&press);
            assert!(!effects.iter().any(|e| matches!(e, Effect::Write(_))), "{effects:?}");
        }
        assert_eq!(s.step(), &Step::Review);
        assert!(view(&s).buttons.iter().any(|b| b.key == 'o'));
    }

    #[test]
    fn a_refused_directory_says_fail_dir_not_show_exists() {
        let mut s = opened_from(OpenMode::ShowOnly, Some(ShowWhy::RefusedDir("not private".to_string())));
        s.press(&enter());
        let question = view(&s).question.unwrap();
        assert_eq!(
            question,
            "~/.config/eitri is not a directory only you can write; nothing was written"
        );
        assert!(!question.contains("already have"));
    }

    #[test]
    fn d_is_marker_declined_and_close() {
        let mut s = screen();
        assert_eq!(s.press(&k('d')), [Effect::Marker(MarkerEvent::Declined), Effect::Close]);
        let mut s = screen_in(OpenMode::Asked);
        assert_eq!(
            s.press(&k('d')),
            [Effect::Marker(MarkerEvent::Declined), Effect::Close],
            "what the footer offers is what it does, on a screen that was asked for too"
        );
        assert!(footer_of(&s).contains("don't ask again"));
    }

    #[test]
    fn a_show_only_screen_offers_nothing_to_decline() {
        let mut s = opened_from(OpenMode::ShowOnly, Some(ShowWhy::HasInit));
        let footer = footer_of(&s);
        assert!(
            !footer.contains("don't ask again") && !footer.contains("not now"),
            "{footer}"
        );
        assert!(!view(&s).buttons.iter().any(|b| b.key == 'd'));
        nothing!(s.press(&k('d')));
        assert!(matches!(s.step(), Step::Choose { .. }));
    }

    #[test]
    fn the_footer_counts_the_presets_this_screen_offers() {
        let s = screen();
        assert_eq!(view(&s).items.len(), 4);
        assert!(footer_of(&s).starts_with("j/k or 1-4 choose"), "{}", footer_of(&s));
        assert!(
            !view(&s).buttons.is_empty(),
            "the footer with its count filled in is still found"
        );
        let mut o = opening(OpenMode::First);
        o.build = Build {
            compositor_mode: true,
            nvim_prefix: true,
        };
        let mut s = shown(o);
        s.found(Found::Desktop(DesktopRow {
            wm: Some("sway"),
            live: true,
            wm_mod: None,
            note: None,
            binds: Vec::new(),
        }));
        assert_eq!(view(&s).items.len(), 5);
        assert!(footer_of(&s).starts_with("j/k or 1-5 choose"), "{}", footer_of(&s));
    }

    #[test]
    fn the_keys_only_the_help_named_are_on_the_footer_and_are_buttons() {
        let mut s = screen();
        let v = view(&s);
        for key in ['r', '?'] {
            assert!(v.buttons.iter().any(|b| b.key == key), "{key}: {v:?}");
        }
        assert_eq!(click(&mut s, ViewKey::Char('r')), [Effect::Rescan(vec![key("C-b")])]);
        click(&mut s, ViewKey::Char('?'));
        let help = view(&s).help.unwrap();
        assert!(help.contains("PageDown") && help.contains("r reads"), "{help}");
    }

    #[test]
    fn scroll_keys_on_the_lines_and_on_a_list() {
        use Scroll::{LineDown, LineUp, PageDown, PageUp};
        let (j, up, down) = (k('j'), named(KeyName::Up), named(KeyName::Down));
        let (page_up, page_down) = (named(KeyName::PPage), named(KeyName::NPage));

        let mut s = screen();
        let before = s.step().clone();
        assert_eq!(s.press(&page_down), [Effect::Scroll(PageDown)]);
        assert_eq!(s.press(&page_up), [Effect::Scroll(PageUp)]);
        assert_eq!(s.step(), &before, "the list reads j, k and the arrows for itself");
        s.found(tmux("C-a"));
        assert_eq!(
            selected(&s),
            PresetId::NvimTmux,
            "a page key leaves the preselection free to move"
        );

        let mut s = screen();
        s.press(&enter());
        assert_eq!(s.step(), &Step::Review);
        assert!(footer_of(&s).contains("j/k scroll"));
        for (press, by) in [
            (&j, LineDown),
            (&k('k'), LineUp),
            (&up, LineUp),
            (&down, LineDown),
            (&page_up, PageUp),
            (&page_down, PageDown),
        ] {
            assert_eq!(s.press(press), [Effect::Scroll(by)], "{press:?}");
        }
        assert_eq!(s.step(), &Step::Review);

        let mut s = screen();
        s.press(&enter());
        s.press(&enter());
        s.written(WriteOutcome::Written(PathBuf::from("/c/init.lua")));
        assert_eq!(s.press(&j), [Effect::Scroll(LineDown)], "the done step");
        let mut s = screen();
        s.press(&enter());
        s.press(&enter());
        s.written(WriteOutcome::Failed("no".to_string()));
        assert_eq!(s.press(&j), [Effect::Scroll(LineDown)], "the failed step");
        let mut s = screen();
        s.press(&enter());
        s.press(&enter());
        s.written(WriteOutcome::NotWritten("no".to_string()));
        assert_eq!(s.press(&down), [Effect::Scroll(LineDown)], "the lines not written");
    }

    #[test]
    fn a_key_that_is_an_answer_is_no_scroll() {
        let page_down = named(KeyName::NPage);
        let mut s = screen();
        go(&mut s, '3');
        assert_eq!(s.step(), &Step::SubPrefix);
        s.press(&k('k'));
        assert_eq!(s.step(), &Step::SubKey, "k here is the key-of-your-own choice");
        s.press(&page_down);
        assert!(
            !matches!(s.step(), Step::SubKey) || view(&s).status.iter().any(|l| l.contains("PageDown")),
            "the key was taken as the prefix's answer: {:?}",
            view(&s).status
        );

        let mut s = press_test_on('s');
        s.press(&k('j'));
        let Step::Press { seen, .. } = s.step() else {
            panic!("{:?}", s.step());
        };
        assert_eq!(
            seen.as_deref(),
            Some("j"),
            "the running test counts it as the wrong key"
        );
        s.press_timed_out();
        assert_eq!(
            s.press(&k('j')),
            [Effect::Scroll(Scroll::LineDown)],
            "the keys are back"
        );
    }

    #[test]
    fn the_running_press_test_names_its_way_out() {
        let mut s = press_test_on('s');
        let v = view(&s);
        assert_eq!(v.footer, en(TextId::FootPress));
        let keys: Vec<ViewKey> = v.buttons.iter().map(|b| b.key).collect();
        assert_eq!(keys, [ViewKey::Esc]);
        assert_eq!(
            click(&mut s, ViewKey::Esc),
            [Effect::CancelPressTimer, Effect::FocusScreen]
        );
        assert_eq!(s.step(), &Step::Review);
    }

    #[test]
    fn a_locale_alone_says_an_input_method_may_be_there_in_the_screens_language() {
        let mut o = opening(OpenMode::First);
        o.lang = Lang::ZhCn;
        let mut s = shown(o);
        s.found(Found::Input(InputRow { im: None, maybe: true }));
        let v = view(&s);
        let row = v.rows.iter().find(|r| r.label == "输入法").unwrap();
        assert_eq!(row.text, text(Lang::ZhCn, TextId::InputMaybe));
        assert!(!row.text.contains("input method"), "{}", row.text);
        go(&mut s, '3');
        assert!(
            status(&s).contains(text(Lang::ZhCn, TextId::PressImWarning)),
            "the Ctrl+Space warning still shows"
        );
    }

    #[test]
    fn the_review_says_who_has_the_prefix_the_file_would_set() {
        let mut s = screen();
        s.found(Found::Neovim(NeovimRow {
            rows: Vec::new(),
            nvim_lhs: None,
            chord_lines: vec![
                (key("C-Space"), "Ctrl+Space in Insert: probably blink.cmp".to_string()),
                (key("C-b"), "Ctrl+b in Insert: probably blink.cmp".to_string()),
            ],
        }));
        go(&mut s, '3');
        s.press(&k('s'));
        let said = status(&s);
        assert!(said.contains("Ctrl+Space in Insert: probably blink.cmp"), "{said}");
        assert!(!said.contains("Ctrl+b in Insert"), "{said}");
    }

    #[test]
    fn a_key_of_the_users_own_asks_the_editor_about_it() {
        let mut s = screen();
        go(&mut s, '3');
        s.press(&k('k'));
        let custom = key("M-a");
        assert_eq!(s.press(&Press::Key(custom)), [Effect::Rescan(vec![key("C-b"), custom])]);
        assert_eq!(s.step(), &Step::Review);
        let mut s = screen();
        go(&mut s, '3');
        s.press(&k('k'));
        nothing!(s.press(&chord("C-h")));
        assert_eq!(s.step(), &Step::SubKey, "a refused key asks the editor nothing");
    }

    #[test]
    fn gnome_is_named_on_the_desktop_row() {
        let mut s = screen();
        s.found(Found::Desktop(crate::onboarding::detect::desktop_row(
            crate::compositor::Wm::Gnome,
            false,
            None,
            Vec::new(),
            false,
        )));
        let v = view(&s);
        let row = v.rows.iter().find(|r| r.label == "desktop").unwrap();
        assert_eq!(row.text, "GNOME");
    }

    #[test]
    fn digits_choose_only_offered_presets() {
        let mut s = screen();
        s.press(&k('4'));
        assert_eq!(selected(&s), PresetId::I3Style);
        s.press(&k('5'));
        assert_eq!(
            selected(&s),
            PresetId::I3Style,
            "preset 5 is not offered in this binary"
        );
        s.press(&k('1'));
        assert_eq!(selected(&s), PresetId::NvimTmux);
        s.press(&named(KeyName::Down));
        assert_eq!(selected(&s), PresetId::NvimNoTmux);
        s.press(&named(KeyName::Up));
        assert_eq!(selected(&s), PresetId::NvimTmux);
        s.press(&k('k'));
        assert_eq!(selected(&s), PresetId::NvimTmux, "the list does not wrap");
    }

    #[test]
    fn preset_three_opens_on_its_prefix_choice() {
        let mut s = screen();
        go(&mut s, '3');
        assert_eq!(s.step(), &Step::SubPrefix);
        let v = view(&s);
        assert_eq!(v.question.as_deref(), Some(en(TextId::SubPrefix)));
        let keys: Vec<ViewKey> = v.buttons.iter().map(|b| b.key).collect();
        assert_eq!(
            &keys[..3],
            [ViewKey::Char('a'), ViewKey::Char('s'), ViewKey::Char('k')],
            "the prefixes first, the footer's keys after"
        );
    }

    #[test]
    fn preset_four_opens_on_alt_super_with_the_detected_one_marked() {
        let mut s = screen();
        s.found(Found::Desktop(DesktopRow {
            wm: Some("i3"),
            live: false,
            wm_mod: Some(WmMod::Super),
            note: None,
            binds: Vec::new(),
        }));
        go(&mut s, '4');
        assert_eq!(s.step(), &Step::SubMod);
        let v = view(&s);
        assert_eq!(v.question.as_deref(), Some(en(TextId::SubMod)));
        assert_eq!(v.buttons[0].label, "a  Alt");
        assert_eq!(v.buttons[1].label, "s  Super (found)");
        s.press(&k('a'));
        assert_eq!(s.step(), &Step::Review);
        assert!(view(&s).body.contains(&"eitri.keymap.use(\"i3-alt\")".to_string()));
        s.press(&backspace());
        assert_eq!(s.step(), &Step::SubMod);
        s.press(&k('s'));
        assert!(!view(&s).body.contains(&"eitri.keymap.use(\"i3-alt\")".to_string()));
    }

    #[test]
    fn a_prefix_other_than_the_running_one_leads_to_the_press_test() {
        let mut s = screen();
        go(&mut s, '3');
        s.press(&k('a'));
        assert_eq!(s.step(), &Step::Review);
        assert_eq!(view(&s).footer, en(TextId::FootReviewPress));
        assert_eq!(s.press(&enter()), [Effect::FocusBack, Effect::ArmPressTimer]);
        assert_eq!(
            s.step(),
            &Step::Press {
                want: key("C-a"),
                seen: None,
                timed_out: false
            }
        );
        assert_eq!(view(&s).question.as_deref(), Some("Press Ctrl+a once"));
    }

    #[test]
    fn the_same_prefix_writes_directly() {
        let mut s = screen();
        go(&mut s, '3');
        s.press(&k('k'));
        assert_eq!(s.step(), &Step::SubKey);
        s.press(&chord("C-b"));
        assert_eq!(s.step(), &Step::Review);
        assert_eq!(view(&s).footer, en(TextId::FootReview));
        let effects = s.press(&enter());
        assert!(matches!(effects.as_slice(), [Effect::Write(_)]), "{effects:?}");
        assert_eq!(s.step(), &Step::Writing);

        let mut s = screen();
        s.press(&enter());
        let effects = s.press(&enter());
        assert!(matches!(effects.as_slice(), [Effect::Write(_)]), "{effects:?}");
    }

    #[test]
    fn it_arrived_offers_enter_write() {
        let mut s = press_test_on('a');
        assert_eq!(s.press(&chord("C-a")), [Effect::CancelPressTimer, Effect::FocusScreen]);
        assert_eq!(s.step(), &Step::Review);
        assert!(status(&s).contains("✓ Ctrl+a reaches Eitri."));
        let text = written_text(&s.press(&enter()));
        assert!(text.contains("eitri.keymap.prefix(\"C-a\")"));
        assert!(!text.contains(en(TextId::CPressFailed)));
    }

    #[test]
    fn something_else_says_what_arrived_and_waits() {
        let mut s = press_test_on('a');
        nothing!(s.press(&k('x')));
        assert!(matches!(s.step(), Step::Press { timed_out: false, .. }));
        assert_eq!(
            status(&s),
            "That was x (not Ctrl+a). Press Ctrl+a, or choose another prefix."
        );
        nothing!(s.press(&Press::Other));
        assert!(status(&s).contains("a key tmux has no name for"));
    }

    #[test]
    fn a_bare_modifier_is_ignored() {
        let mut s = press_test_on('a');
        let before = view(&s);
        nothing!(s.press(&Press::Modifier));
        assert_eq!(view(&s), before);
        assert!(matches!(s.step(), Step::Press { seen: None, .. }));
    }

    #[test]
    fn nothing_in_time_offers_w() {
        let mut s = press_test_on('a');
        assert_eq!(s.press_timed_out(), [Effect::FocusScreen]);
        nothing!(s.press_timed_out());
        assert!(status(&s).contains("Ctrl+a never reached Eitri"));
        assert!(status(&s).contains("(Ctrl+b keeps today's)"));
        let text = written_text(&s.press(&k('w')));
        assert!(text.contains(en(TextId::CPressFailed)), "{text}");
        assert_eq!(s.step(), &Step::Writing);
    }

    #[test]
    fn the_keys_come_back_after_a_timeout() {
        let mut s = press_test_on('a');
        s.press_timed_out();
        nothing!(s.press(&k('p')));
        assert_eq!(s.step(), &Step::SubPrefix);
        s.press(&k('a'));
        s.press(&enter());
        s.press_timed_out();
        nothing!(s.press(&backspace()));
        assert_eq!(s.step(), &Step::Review);
    }

    #[test]
    fn esc_during_the_press_test_goes_back_to_review_and_cancels_the_timer() {
        let mut s = press_test_on('a');
        assert_eq!(s.press(&Press::Escape), [Effect::CancelPressTimer, Effect::FocusScreen]);
        assert_eq!(s.step(), &Step::Review);
        assert!(!status(&s).contains("reaches Eitri"));
        nothing!(s.press_timed_out());
        assert_eq!(s.step(), &Step::Review);
    }

    #[test]
    fn choosing_ctrl_space_with_an_input_method_shows_the_warning() {
        for im in ["fcitx5", "an input method"] {
            let mut s = screen();
            s.found(input(im));
            go(&mut s, '3');
            assert!(status(&s).contains(en(TextId::PressImWarning)), "sub-choice, {im}");
            s.press(&k('s'));
            assert!(status(&s).contains(en(TextId::PressImWarning)), "review, {im}");
            assert_eq!(s.press(&enter()), [Effect::FocusBack, Effect::ArmPressTimer]);
            assert!(status(&s).contains(en(TextId::PressImWarning)), "press, {im}");
            assert_eq!(s.press_timed_out(), [Effect::FocusScreen], "the test still ran");
        }
        let mut s = screen();
        s.found(Found::Input(InputRow { im: None, maybe: false }));
        go(&mut s, '3');
        s.press(&k('s'));
        assert!(!status(&s).contains("input method"));
        let mut s = screen();
        s.found(input("fcitx5"));
        go(&mut s, '3');
        s.press(&k('a'));
        assert!(!status(&s).contains("input method"), "Ctrl+a is no input method's key");
    }

    #[test]
    fn a_prefix_that_is_a_compositor_bind_warns() {
        let mut s = screen();
        s.found(desktop("sway", vec![Chord::of(&key("C-a"))]));
        go(&mut s, '3');
        s.press(&k('a'));
        assert!(
            status(&s).contains("Ctrl+a is also a binding of sway."),
            "{}",
            status(&s)
        );
        assert_eq!(s.press(&enter()), [Effect::FocusBack, Effect::ArmPressTimer]);
        assert!(status(&s).contains("Ctrl+a is also a binding of sway."));
        s.press(&Press::Escape);
        s.press(&k('p'));
        s.press(&k('s'));
        assert_eq!(s.step(), &Step::Review, "nothing is refused");
        assert!(!status(&s).contains("binding"), "Ctrl+Space is no bind here");
    }

    #[test]
    fn a_prefix_the_keymap_refuses_is_refused_on_the_screen() {
        let mut s = screen();
        go(&mut s, '3');
        s.press(&k('k'));
        for press in [chord("C-h"), k('x'), Press::Other] {
            nothing!(s.press(&press));
            assert_eq!(s.step(), &Step::SubKey, "{press:?}");
            assert!(
                status(&s).contains("cannot be the prefix: "),
                "{press:?}: {}",
                status(&s)
            );
        }
        s.press(&chord("C-h"));
        assert!(
            status(&s).starts_with("Ctrl+h cannot be the prefix: "),
            "{}",
            status(&s)
        );
        s.press(&k('x'));
        assert!(status(&s).contains("must be a chord"), "{}", status(&s));
        s.press(&Press::Other);
        assert!(status(&s).contains("tmux can name"), "{}", status(&s));
        assert!(
            !s.view().unwrap().footer.is_empty(),
            "the step offers a way out while it refuses"
        );
        s.press(&chord("C-o"));
        assert_eq!(s.step(), &Step::Review, "a key the keymap takes goes on");
        assert!(!status(&s).contains("cannot be the prefix"));
    }

    #[test]
    fn a_pressed_key_that_was_bound_notes_what_it_was() {
        let mut s = screen();
        go(&mut s, '3');
        s.press(&k('k'));
        s.press(&chord("C-o"));
        let body = view(&s).body.join("\n");
        assert!(body.contains("eitri.keymap.del(\"prefix\", \"C-o\")"), "{body}");
        assert!(body.contains("was "), "{body}");
    }

    #[test]
    fn a_trust_question_holds_the_screen() {
        let mut s = screen();
        s.press(&k('4'));
        let before = s.step().clone();
        assert_eq!(s.found(Found::TrustOpen(true)), [Effect::FocusBack]);
        assert!(s.held());
        assert!(s.view().is_none());
        nothing!(s.press(&k('j')));
        nothing!(s.found(Found::TrustOpen(true)));
        s.found(desktop("sway", Vec::new()));
        assert_eq!(s.found(Found::TrustOpen(false)), [Effect::FocusScreen]);
        assert_eq!(s.step(), &before);
        assert!(view(&s).rows[1].text.starts_with("sway"), "detection kept arriving");
        nothing!(s.found(Found::TrustOpen(false)));
    }

    #[test]
    fn an_opening_with_a_question_open_stays_held() {
        let mut o = opening(OpenMode::First);
        o.trust_open = true;
        let mut s = Screen::new(o);
        nothing!(s.shown());
        assert!(s.held());
        assert!(s.view().is_none());
        nothing!(s.press(&enter()));
        s.found(tmux("C-a"));
        assert_eq!(
            s.found(Found::TrustOpen(false)),
            [Effect::FocusScreen, Effect::Rescan(vec![key("C-b")])]
        );
        assert!(!s.held());
        assert_eq!(selected(&s), PresetId::NvimTmux, "detection filled the held screen");
        assert!(s.view().is_some());
    }

    #[test]
    fn a_hold_during_the_press_test_cancels_and_rearms_the_timer() {
        let mut s = press_test_on('a');
        s.press(&k('x'));
        assert_eq!(s.found(Found::TrustOpen(true)), [Effect::CancelPressTimer]);
        nothing!(s.press_timed_out());
        assert_eq!(s.found(Found::TrustOpen(false)), [Effect::ArmPressTimer]);
        assert!(matches!(
            s.step(),
            Step::Press {
                seen: Some(_),
                timed_out: false,
                ..
            }
        ));

        let mut s = press_test_on('a');
        s.press_timed_out();
        assert_eq!(s.found(Found::TrustOpen(true)), [Effect::FocusBack]);
        assert_eq!(s.found(Found::TrustOpen(false)), [Effect::FocusScreen]);
    }

    #[test]
    fn a_written_while_held_updates_what_resumes() {
        let mut s = screen();
        s.press(&enter());
        s.press(&enter());
        assert_eq!(s.step(), &Step::Writing);
        assert_eq!(s.found(Found::TrustOpen(true)), [Effect::FocusBack]);
        nothing!(s.written(WriteOutcome::Written(PathBuf::from("/home/u/.config/eitri/init.lua"))));
        assert!(s.held());
        assert_eq!(
            s.step(),
            &Step::Held {
                resume: Box::new(Step::Done {
                    path: "/home/u/.config/eitri/init.lua".to_string()
                })
            }
        );
        assert_eq!(s.found(Found::TrustOpen(false)), [Effect::FocusScreen]);
        assert!(matches!(s.step(), Step::Done { .. }));
    }

    #[test]
    fn written_goes_to_done() {
        let mut s = screen();
        s.press(&enter());
        s.press(&enter());
        nothing!(s.written(WriteOutcome::Written(PathBuf::from("/home/u/.config/eitri/init.lua"))));
        let v = view(&s);
        assert_eq!(
            v.body[0],
            "Written /home/u/.config/eitri/init.lua. Your keys change the next time Eitri starts; this window keeps Ctrl+b."
        );
        assert_eq!(v.footer, en(TextId::FootDone));
        assert_eq!(s.press(&enter()), [Effect::Close]);
    }

    #[test]
    fn an_answer_while_nothing_is_being_written_is_dropped() {
        let mut s = screen();
        nothing!(s.written(WriteOutcome::Written(PathBuf::from("/x"))));
        assert!(matches!(s.step(), Step::Choose { .. }));
        nothing!(s.check_failed(CheckWhy::Unverified));
        assert!(matches!(s.step(), Step::Choose { .. }));
    }

    #[test]
    fn failed_shows_why_with_backspace_back() {
        let mut s = screen();
        s.press(&enter());
        s.press(&enter());
        let why = "an init.lua appeared meanwhile (another window?); nothing was written";
        s.written(WriteOutcome::Failed(why.into()));
        let v = view(&s);
        assert_eq!(v.status, [why]);
        assert_eq!(v.footer, en(TextId::FootFailed));
        nothing!(s.press(&backspace()));
        assert_eq!(s.step(), &Step::Review);
    }

    #[test]
    fn not_written_shows_the_lines_and_o() {
        let mut s = screen();
        s.press(&enter());
        let lines = view(&s).body;
        s.press(&enter());
        s.written(WriteOutcome::NotWritten(
            "this file system cannot create init.lua safely".into(),
        ));
        let v = view(&s);
        assert_eq!(v.status, ["this file system cannot create init.lua safely"]);
        assert_eq!(v.body, lines);
        assert!(v.buttons.iter().any(|b| b.key == 'o'));
        assert_eq!(v.footer, en(TextId::FootShow));
    }

    #[test]
    fn a_failed_check_shows_the_lines_and_o() {
        let mut s = screen();
        s.press(&enter());
        s.press(&enter());
        nothing!(s.check_failed(CheckWhy::Unverified));
        let v = view(&s);
        assert_eq!(v.status, [en(TextId::NotwrittenCheck)]);
        assert!(!v.body.is_empty());
        assert!(v.buttons.iter().any(|b| b.key == 'o'));

        let mut s = screen();
        s.press(&enter());
        s.press(&enter());
        s.check_failed(CheckWhy::Refused(
            "eitri.keymap.prefix(\"C-h\"): that chord is already a root key".into(),
        ));
        let v = view(&s);
        assert_eq!(
            v.status,
            ["could not write ~/.config/eitri/init.lua: eitri.keymap.prefix(\"C-h\"): that chord is already a root key; nothing was written"]
        );
        assert!(v.buttons.iter().any(|b| b.key == 'o'));
    }

    #[test]
    fn o_is_close_then_open_lines() {
        let mut s = screen();
        s.press(&enter());
        let lines = view(&s).body;
        s.press(&enter());
        s.written(WriteOutcome::NotWritten(
            "this file system cannot create init.lua safely".into(),
        ));
        assert_eq!(
            s.press(&k('o')),
            [
                Effect::Marker(MarkerEvent::NotNow),
                Effect::Close,
                Effect::OpenLines(lines.clone())
            ],
            "a first start that wrote nothing remembers it"
        );
        let mut s = screen_in(OpenMode::Asked);
        s.press(&enter());
        s.press(&enter());
        s.written(WriteOutcome::NotWritten(
            "this file system cannot create init.lua safely".into(),
        ));
        assert_eq!(s.press(&k('o')), [Effect::Close, Effect::OpenLines(lines)]);
    }

    #[test]
    fn t_on_done_is_close_then_open_tutor() {
        let mut s = screen();
        s.press(&enter());
        s.press(&enter());
        s.written(WriteOutcome::Written(PathBuf::from("/x/init.lua")));
        assert_eq!(s.press(&k('t')), [Effect::Close, Effect::OpenTutor]);
        assert_eq!(
            s.press(&Press::Escape),
            [Effect::Close],
            "a written file needs no marker"
        );
    }

    #[test]
    fn done_adds_the_stock_keys_line_when_the_import_changed_keys() {
        let rebinding = Found::Tmux(TmuxRow {
            path: Some("~/.tmux.conf".to_string()),
            prefix: Some(key("C-a")),
            changes_keys: true,
        });
        let mut s = screen_running("C-a");
        s.found(rebinding.clone());
        s.press(&enter());
        assert_eq!(s.preset, PresetId::NvimTmux);
        s.press(&enter());
        s.written(WriteOutcome::Written(PathBuf::from("/x/init.lua")));
        let body = view(&s).body;
        assert_eq!(body.len(), 2);
        assert_eq!(
            body[1],
            "The tutor teaches the stock keys after the prefix; your tmux configuration changes some of them (Ctrl+a ? lists yours)."
        );

        let mut s2 = screen();
        s2.found(rebinding);
        s2.press(&k('2'));
        s2.press(&enter());
        s2.press(&enter());
        s2.written(WriteOutcome::Written(PathBuf::from("/x/init.lua")));
        assert_eq!(view(&s2).body.len(), 1, "preset 2 turns the import off");

        let mut s = screen_running("C-a");
        s.found(tmux("C-a"));
        s.press(&enter());
        s.press(&enter());
        s.written(WriteOutcome::Written(PathBuf::from("/x/init.lua")));
        assert_eq!(view(&s).body.len(), 1, "no changed keys, no line");
    }

    #[test]
    fn untrusted_values_are_escaped_and_constant_wording_is_not_cut() {
        let mut s = screen();
        s.found(Found::Tmux(TmuxRow {
            path: Some("~/a\u{202e}b.conf".to_string()),
            prefix: Some(key("C-a")),
            changes_keys: false,
        }));
        let row = view(&s).rows[0].text.clone();
        assert!(row.contains("\\u{202e}"), "{row}");
        assert!(!row.contains('\u{202e}'));

        let mut o = opening(OpenMode::First);
        o.config_path = "/h/\u{202e}evil".to_string();
        let mut s2 = shown(o);
        s2.press(&enter());
        let question = view(&s2).question.unwrap();
        assert!(
            question.contains("\\u{202e}") && !question.contains('\u{202e}'),
            "{question}"
        );

        let mut s = screen();
        s.found(input("fcitx5"));
        go(&mut s, '3');
        s.press(&k('s'));
        s.press(&enter());
        s.press_timed_out();
        let status = status(&s);
        let never = fill(en(TextId::PressNever), &[("key", "Ctrl+Space"), ("running", "Ctrl+b")]);
        assert!(never.chars().count() > 80);
        assert!(status.contains(&never), "{status}");
        assert!(en(TextId::PressImWarning).chars().count() > 80);
        assert!(status.contains(en(TextId::PressImWarning)));
    }

    #[test]
    fn a_long_nvim_owner_is_cut_at_eighty_by_the_row_that_reaches_the_screen() {
        use crate::nvim_keys::findings::{Certainty, ChordOwner, Mode, NavOwner, NvimFindings, Owner};
        let findings = NvimFindings {
            leader: "<Space>".to_string(),
            chords: vec![ChordOwner {
                chord: key("C-b"),
                mode: Mode::Insert,
                owner: Owner::Desc("x".repeat(200)),
                certainty: Certainty::Observed,
            }],
            nav: NavOwner::VimTmuxNavigator,
            pending: Vec::new(),
            distro: None,
        };
        let row = crate::onboarding::detect::neovim_row_with(&findings, Lang::En, &display_text, &key("C-b"));
        let mut s = screen();
        s.found(Found::Neovim(row));
        let lines: Vec<String> = view(&s)
            .rows
            .iter()
            .filter(|r| r.text.contains("xxx"))
            .map(|r| r.text.clone())
            .collect();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].matches('x').count(), 80);
    }

    #[test]
    fn review_lines_equal_the_written_text() {
        for digit in ['1', '2', '3', '4'] {
            // Preset 2 turns the tmux import off, so its prefix is the stock one whatever tmux says.
            let mut s = screen_running(if digit == '2' { "C-b" } else { "C-a" });
            s.found(tmux("C-a"));
            go(&mut s, digit);
            if digit == '3' {
                s.press(&k('a'));
            }
            if digit == '4' {
                s.press(&k('a'));
            }
            let shown = view(&s).body;
            let effects = s.press(&enter());
            let [Effect::Write(generated)] = effects.as_slice() else {
                panic!("preset {digit}: {effects:?}");
            };
            assert_eq!(generated.lines, shown, "preset {digit}");
            assert_eq!(generated.text(), format!("{}\n", shown.join("\n")));
        }
    }

    #[test]
    fn the_file_names_the_prefix_the_user_chose() {
        let mut s = screen();
        go(&mut s, '3');
        s.press(&k('s'));
        let body = view(&s).body.join("\n");
        assert!(body.contains("eitri.keymap.prefix(\"C-Space\")"), "{body}");
        assert!(body.contains("2026-10-31"));
    }

    #[test]
    fn a_tmux_prefix_the_file_overrides_is_said() {
        let mut s = screen();
        s.found(tmux("C-a"));
        go(&mut s, '3');
        s.press(&k('s'));
        assert!(status(&s).contains("Your tmux prefix (Ctrl+a) is not used in Eitri; this line wins."));
    }

    #[test]
    fn p_on_the_review_changes_the_prefix_and_backspace_comes_back() {
        let mut s = screen();
        s.press(&enter());
        assert_eq!(view(&s).buttons[0].label, "Prefix: Ctrl+b   p change");
        s.press(&k('p'));
        assert_eq!(s.step(), &Step::SubPrefix);
        s.press(&backspace());
        assert_eq!(s.step(), &Step::Review);
        s.press(&k('p'));
        s.press(&k('s'));
        assert_eq!(s.step(), &Step::Review);
        assert_eq!(view(&s).buttons[0].label, "Prefix: Ctrl+Space   p change");
        s.press(&backspace());
        assert!(matches!(s.step(), Step::Choose { .. }));
    }

    #[test]
    fn question_mark_shows_the_help_line() {
        let mut s = screen();
        assert!(view(&s).help.is_none());
        s.press(&k('?'));
        assert_eq!(view(&s).help.as_deref(), Some(en(TextId::HelpLine)));
        s.press(&k('?'));
        assert!(view(&s).help.is_none());
    }

    #[test]
    fn r_rescans_and_the_neovim_row_reads_again() {
        let mut s = screen();
        s.found(Found::Neovim(NeovimRow {
            rows: vec!["leader: Space".into()],
            nvim_lhs: None,
            chord_lines: Vec::new(),
        }));
        assert_eq!(s.press(&k('r')), [Effect::Rescan(vec![key("C-b")])]);
        assert_eq!(view(&s).rows[2].text, en(TextId::RowReadingNvim));
    }

    #[test]
    fn the_leader_line_follows_what_neovim_reported() {
        let mut s = screen();
        s.found(Found::Neovim(NeovimRow {
            rows: vec!["leader: Space".into()],
            nvim_lhs: Some("<leader>E"),
            chord_lines: Vec::new(),
        }));
        s.press(&enter());
        let body = view(&s).body.join("\n");
        assert!(
            body.contains("eitri.config.set(\"keymap.nvim_prefix\", \"<leader>E\")"),
            "{body}"
        );
    }

    #[test]
    fn the_chinese_screen_is_set_in_chinese() {
        let mut o = opening(OpenMode::First);
        o.lang = Lang::ZhCn;
        let s = shown(o);
        let v = view(&s);
        assert_eq!(v.title, text(Lang::ZhCn, TextId::Title));
        assert_eq!(v.footer, text(Lang::ZhCn, TextId::FootChoose).replace("{n}", "4"));
    }

    #[test]
    fn every_footer_part_has_its_key_in_both_languages() {
        for lang in [Lang::En, Lang::ZhCn] {
            for id in FOOTERS {
                let parts = text(lang, id).split(FOOTER_SEPARATOR).count();
                assert_eq!(parts, footer_keys(id).len(), "{id:?} in {lang:?}: {}", text(lang, id));
            }
        }
    }

    fn footer_of(s: &Screen) -> String {
        view(s).footer
    }

    /// What a click on `wanted` does: the press it posts, which only a button of the step (or, for a number, a
    /// preset's row) can post.
    fn click(s: &mut Screen, wanted: ViewKey) -> Vec<Effect> {
        let v = view(s);
        let digit = match wanted {
            ViewKey::Char(c) => c.to_digit(10),
            _ => None,
        };
        let a_row = digit.is_some_and(|n| v.items.iter().any(|i| i.number == n));
        assert!(
            a_row || v.buttons.iter().any(|b| b.key == wanted),
            "nothing to click for {wanted} on {:?}: {v:?}",
            s.step()
        );
        s.press(&wanted.press().0)
    }

    #[test]
    fn a_mouse_alone_gets_through_every_step() {
        use ViewKey::{Back, Char, Enter, Esc};
        for lang in [Lang::En, Lang::ZhCn] {
            let shown_in = |mode| {
                let mut o = opening(mode);
                o.lang = lang;
                shown(o)
            };

            // The shortest way: a preset, review, write, done.
            let mut s = shown_in(OpenMode::First);
            let labels: Vec<String> = view(&s).buttons.iter().map(|b| b.label.clone()).collect();
            assert!(labels.iter().all(|l| !l.contains(" · ")), "{labels:?}");
            // The Enter button is the footer's own part, in the screen's language.
            let button = view(&s).buttons.into_iter().find(|b| b.key == Enter).unwrap();
            assert!(
                text(lang, TextId::FootChoose).contains(&button.label),
                "{}",
                button.label
            );
            click(&mut s, Enter);
            assert_eq!(s.step(), &Step::Review);
            click(&mut s, Enter);
            assert_eq!(s.step(), &Step::Writing);
            s.written(WriteOutcome::Written(PathBuf::from("/home/u/.config/eitri/init.lua")));
            assert_eq!(click(&mut s, Char('t')), [Effect::Close, Effect::OpenTutor]);
            let mut s = shown_in(OpenMode::First);
            click(&mut s, Enter);
            click(&mut s, Enter);
            s.written(WriteOutcome::Written(PathBuf::from("/home/u/.config/eitri/init.lua")));
            assert_eq!(click(&mut s, Enter), [Effect::Close]);

            // The sub-choices, and the way back out of each.
            let mut s = shown_in(OpenMode::First);
            click(&mut s, Char('3'));
            click(&mut s, Enter);
            assert_eq!(s.step(), &Step::SubPrefix);
            click(&mut s, Char('k'));
            assert_eq!(s.step(), &Step::SubKey);
            click(&mut s, Back);
            assert_eq!(s.step(), &Step::SubPrefix);
            click(&mut s, Back);
            assert_eq!(selected(&s), PresetId::OtherPrefix);
            click(&mut s, Char('4'));
            click(&mut s, Enter);
            assert_eq!(s.step(), &Step::SubMod);
            click(&mut s, Char('s'));
            assert_eq!(s.step(), &Step::Review);
            click(&mut s, Back);
            assert_eq!(s.step(), &Step::SubMod);
            click(&mut s, Char('a'));
            assert_eq!(s.step(), &Step::Review);
            assert_eq!(click(&mut s, Esc), [Effect::Marker(MarkerEvent::NotNow), Effect::Close]);

            // The running press test has one button, the way out. It runs out by itself, and what it then says
            // has a button each: another prefix, back, write it anyway.
            let mut s = shown_in(OpenMode::First);
            click(&mut s, Char('3'));
            click(&mut s, Enter);
            click(&mut s, Char('a'));
            assert_eq!(click(&mut s, Enter), [Effect::FocusBack, Effect::ArmPressTimer]);
            assert_eq!(view(&s).buttons.len(), 1, "{:?}", view(&s));
            s.press_timed_out();
            click(&mut s, Char('p'));
            assert_eq!(s.step(), &Step::SubPrefix);
            click(&mut s, Char('s'));
            click(&mut s, Enter);
            s.press_timed_out();
            click(&mut s, Back);
            assert_eq!(s.step(), &Step::Review);
            click(&mut s, Enter);
            s.press_timed_out();
            click(&mut s, Char('w'));
            assert_eq!(s.step(), &Step::Writing);

            // What writing can end in.
            s.written(WriteOutcome::Failed("no".to_string()));
            click(&mut s, Back);
            assert_eq!(s.step(), &Step::Review);
            click(&mut s, Enter);
            s.press_timed_out();
            click(&mut s, Char('w'));
            s.written(WriteOutcome::Failed("no".to_string()));
            assert_eq!(click(&mut s, Esc), [Effect::Marker(MarkerEvent::NotNow), Effect::Close]);
            let mut s = shown_in(OpenMode::First);
            click(&mut s, Enter);
            click(&mut s, Enter);
            s.written(WriteOutcome::NotWritten("no".to_string()));
            let effects = click(&mut s, Char('o'));
            assert!(effects.contains(&Effect::Close), "{effects:?}");
            assert!(matches!(effects.last(), Some(Effect::OpenLines(_))), "{effects:?}");

            // A screen that only shows the lines.
            let mut s = shown_in(OpenMode::ShowOnly);
            click(&mut s, Enter);
            assert_eq!(s.step(), &Step::Review);
            assert!(matches!(click(&mut s, Char('o')).last(), Some(Effect::OpenLines(_))));
            let mut s = shown_in(OpenMode::ShowOnly);
            click(&mut s, Enter);
            assert_eq!(click(&mut s, Esc), [Effect::Close]);
        }
    }

    #[test]
    fn a_press_test_that_ran_out_has_a_button_for_what_it_says() {
        use ViewKey::{Back, Char};
        for lang in [Lang::En, Lang::ZhCn] {
            let mut o = opening(OpenMode::First);
            o.lang = lang;
            // The running prefix came from tmux and the preset goes back to the stock one.
            o.running_prefix = key("C-a");
            let mut s = shown(o);
            go(&mut s, '2');
            assert_eq!(s.press(&enter()), [Effect::FocusBack, Effect::ArmPressTimer]);
            s.press_timed_out();
            let v = view(&s);
            let label = |wanted: ViewKey| {
                let button = v.buttons.iter().find(|b| b.key == wanted);
                button.unwrap_or_else(|| panic!("no {wanted}: {v:?}")).label.clone()
            };
            let never = v.status.last().unwrap();
            assert!(never.contains(&label(Char('w'))), "{never}");
            assert!(label(Char('p')).contains('p'), "{}", label(Char('p')));
            assert!(text(lang, TextId::FootReview).contains(&label(Back)), "{}", label(Back));
            assert_eq!(v.footer, "");
        }
    }

    #[test]
    fn the_footer_hint_leaves_out_what_the_buttons_say() {
        for lang in [Lang::En, Lang::ZhCn] {
            let mut o = opening(OpenMode::First);
            o.lang = lang;
            let mut s = shown(o);
            let filled = text(lang, TextId::FootChoose).replace("{n}", "4");
            let first = filled.split(FOOTER_SEPARATOR).next().unwrap();
            assert_eq!(view(&s).footer_hint(), first);
            s.press(&enter());
            let scroll = text(lang, TextId::FootReview).split(FOOTER_SEPARATOR).nth(3).unwrap();
            assert_eq!(view(&s).footer_hint(), scroll, "review");
            s.press(&enter());
            s.written(WriteOutcome::Failed("no".to_string()));
            assert_eq!(view(&s).footer_hint(), "", "failed");
            s.press(&backspace());
            s.press(&enter());
            s.written(WriteOutcome::NotWritten("no".to_string()));
            assert_eq!(view(&s).footer_hint(), scroll, "not written");
            let mut s = screen();
            s.press(&enter());
            s.press(&enter());
            s.written(WriteOutcome::Written(PathBuf::from("/home/u/init.lua")));
            assert_eq!(view(&s).footer_hint(), "", "done");
        }
    }

    #[test]
    fn a_click_is_the_press_of_the_key_it_names() {
        for (key, press) in [
            (ViewKey::Enter, enter()),
            (ViewKey::Back, named(KeyName::BSpace)),
            (ViewKey::Esc, Press::Escape),
            (ViewKey::Char('d'), Press::Key(KeySpec::char('d', false, false))),
        ] {
            assert_eq!(key.press().0, press, "{key}");
        }
        assert_eq!(ViewKey::Esc.press().1, None);
    }

    #[test]
    fn a_new_screen_is_held_and_the_press_timeout_is_eight_seconds() {
        assert_eq!(PRESS_TEST_TIMEOUT, Duration::from_secs(8));
        let s = Screen::new(opening(OpenMode::First));
        assert!(s.held() && s.view().is_none());
        assert_eq!(s.generation(), 1);
    }

    #[test]
    fn this_binary_offers_four_presets_and_no_windows_row() {
        let s = screen();
        assert_eq!(view(&s).items.len(), 4);
        assert!(!view(&s).rows.iter().any(|r| r.label == "windows"));
    }

    #[test]
    fn compositor_mode_adds_the_windows_row_the_fifth_preset_and_w() {
        let mut o = opening(OpenMode::First);
        o.build = Build {
            compositor_mode: true,
            nvim_prefix: true,
        };
        let mut s = shown(o);
        s.found(desktop("sway", Vec::new()));
        s.found(Found::Windows(Some(Tiling::Compositor)));
        assert_eq!(
            selected(&s),
            PresetId::Compositor,
            "a live tiling compositor preselects preset 5"
        );
        let v = view(&s);
        assert_eq!(v.items.len(), 5);
        assert!(v.rows.iter().any(|r| r.label == "windows"));
        s.press(&k('1'));
        s.press(&enter());
        let v = view(&s);
        assert!(v
            .buttons
            .iter()
            .any(|b| b.key == 'w' && b.label == "Windows: sway arranges   w change"));
        assert!(v.body.join("\n").contains("window.tiling\", \"compositor\""));
        s.press(&k('w'));
        let v = view(&s);
        assert!(v
            .buttons
            .iter()
            .any(|b| b.key == 'w' && b.label == "Windows: Eitri arranges   w change"));
        assert!(v.body.join("\n").contains("window.tiling\", \"eitri\""));
        s.press(&backspace());
        s.press(&k('4'));
        s.press(&enter());
        s.press(&k('s'));
        assert!(status(&s).contains("sway would otherwise arrange them"));
    }

    /// A screen with compositor mode on sway, with preset `preset` chosen.
    fn on_sway(preset: char) -> Screen {
        let mut o = opening(OpenMode::First);
        o.build = Build {
            compositor_mode: true,
            nvim_prefix: true,
        };
        let mut s = shown(o);
        s.found(desktop("sway", Vec::new()));
        s.found(Found::Windows(Some(Tiling::Compositor)));
        s.press(&k(preset));
        s.press(&enter());
        s
    }

    fn done_body(s: &mut Screen) -> String {
        s.press(&enter());
        s.written(WriteOutcome::Written(PathBuf::from("/x/init.lua")));
        view(s).body.join("\n")
    }

    #[test]
    fn done_says_a_window_tiling_line_waits_for_the_next_start() {
        let mut s = on_sway('1');
        s.press(&k('w'));
        let body = done_body(&mut s);
        assert!(body.contains(en(TextId::DoneWindowsNext)), "{body}");

        let mut s = screen();
        s.press(&enter());
        s.press(&enter());
        s.written(WriteOutcome::Written(PathBuf::from("/x/init.lua")));
        assert!(
            !view(&s).body.join("\n").contains(en(TextId::DoneWindowsNext)),
            "a file with no such line says nothing of it"
        );
    }

    #[test]
    fn done_says_what_the_tutor_teaches_where_the_compositor_arranges_the_windows() {
        let wants = "The tutor teaches the keys of Eitri's own layout: where sway arranges the windows";
        let mut kept = on_sway('1');
        assert!(done_body(&mut kept).contains(wants));
        // Eitri arranges from the next start, but this run's tutor still meets the keys that are off.
        let mut chose_eitri = on_sway('1');
        chose_eitri.press(&k('w'));
        assert!(done_body(&mut chose_eitri).contains(wants));
        assert!(!{
            let mut s = screen();
            s.press(&enter());
            s.press(&enter());
            s.written(WriteOutcome::Written(PathBuf::from("/x/init.lua")));
            view(&s).body.join("\n")
        }
        .contains(wants));
    }
}
