//! The first-start screen in the window controller: what holds it up, where the keys are while it is shown, and
//! how the screen's effects become host ops.
//!
//! The screen itself is the state machine of [`crate::onboarding::screen`]; it does no I/O and holds no timer.
//! Here it is the window's chrome data (`ChromeModel::onboarding`), answered by the window stage after the y/n
//! and before the prefix ([`super::keys::onboarding_press`]). Every effect it asks for is turned into ops in
//! [`carry`]; the shell draws the view and does the work on worker threads, and what comes back is an
//! [`OnboardingEvent`] carrying the generation the screen opened under.
//!
//! A screen is up from its `Open` until its `Close`. While a question of the trust gate holds the keys the screen
//! is *held*: it shows nothing and reads no key, and the keys are with the module the question lives in. The press
//! test gives the keys back the same way, but the screen stays shown and the window stage still reads every key,
//! which is how the test sees the key arrive.

use super::chrome;
use super::focus::{arrive_tail, give_keys};
use super::host::*;
use super::window::{WinId, Window};
use crate::layout::ModuleId;
use crate::onboarding::screen::{Effect, Screen};

/// The screen a window shows, and what the controller keeps beside it.
pub struct OnboardingState {
    pub screen: Screen,
    /// The module that held the keys when the screen took them, while the screen has them: the keys go back there
    /// when the screen gives them up. `None` while the keys are with a module (a held screen, the press test).
    pub held_module: Option<ModuleId>,
    /// The press test's timer while it runs.
    pub press_timer: Option<TimerToken>,
    /// The generation the screen was opened under.
    pub generation: u64,
    /// The date the marker file carries, from the opening.
    pub today: String,
}

/// `window.setup`: asks the host to check and open the screen as the user asked for it. A screen already up is
/// left as it is.
pub(super) fn setup(w: &mut Window, win: WinId, batch: &mut Batch) {
    if w.onboarding.is_some() {
        w.say("[onboarding] the screen is up already".into());
        return;
    }
    w.onboarding_generation += 1;
    let generation = w.onboarding_generation;
    w.say(format!("[onboarding] setup asked for (generation {generation})"));
    batch.push_effect(HostOp::Onboarding(OnboardingOp::Check {
        win,
        generation,
        requested: true,
    }));
}

/// The generation of the screen that is up, `None` when none is. The start opens its screen under generation `0`.
pub(super) fn generation(w: &Window) -> Option<u64> {
    w.onboarding.as_ref().map(|state| state.generation)
}

/// A fact about the screen. One for a screen that is not up, or for another generation, changes nothing: a worker
/// may well answer after the window left the screen.
pub(super) fn event(w: &mut Window, win: WinId, event: OnboardingEvent, batch: &mut Batch) {
    match event {
        OnboardingEvent::Open { opening, .. } => open(w, win, opening, batch),
        OnboardingEvent::Found { generation, found, .. } => {
            use crate::onboarding::Found;
            let any = matches!(found, Found::TrustOpen(_)) && generation == 0;
            let Some(state) = live(w, generation, any) else {
                return;
            };
            let effects = state.screen.found(found);
            carry(w, win, effects, batch);
        }
        OnboardingEvent::Written {
            generation, outcome, ..
        } => {
            let Some(state) = live(w, generation, false) else {
                return;
            };
            let effects = state.screen.written(outcome);
            carry(w, win, effects, batch);
        }
        OnboardingEvent::CheckFailed { generation, why, .. } => {
            let Some(state) = live(w, generation, false) else {
                return;
            };
            let effects = state.screen.check_failed(why);
            carry(w, win, effects, batch);
        }
    }
}

/// The screen `generation` names; `any` takes whichever is up.
fn live(w: &mut Window, generation: u64, any: bool) -> Option<&mut OnboardingState> {
    w.onboarding
        .as_mut()
        .filter(|state| any || state.generation == generation)
}

fn open(w: &mut Window, win: WinId, opening: crate::onboarding::Opening, batch: &mut Batch) {
    if w.onboarding.is_some() || opening.generation != w.onboarding_generation {
        return;
    }
    let generation = opening.generation;
    let today = opening.today.clone();
    let mut screen = Screen::new(opening);
    let effects = screen.shown();
    w.onboarding = Some(OnboardingState {
        screen,
        held_module: None,
        press_timer: None,
        generation,
        today,
    });
    w.say(format!("[onboarding] screen up (generation {generation})"));
    batch.push_effect(HostOp::Onboarding(OnboardingOp::Detect { win, generation }));
    carry(w, win, effects, batch);
}

/// Turns what the screen asked for into ops, in the order it asked, and announces the view if it changed.
pub(super) fn carry(w: &mut Window, win: WinId, effects: Vec<Effect>, batch: &mut Batch) {
    let Some((generation, today)) = w.onboarding.as_ref().map(|s| (s.generation, s.today.clone())) else {
        return;
    };
    for effect in effects {
        match effect {
            Effect::FocusScreen => focus_screen(w, win, batch),
            Effect::FocusBack => focus_back(w, win, batch),
            Effect::ArmPressTimer => arm_press_timer(w),
            Effect::CancelPressTimer => cancel_press_timer(w),
            Effect::Rescan(extra) => {
                batch.push_effect(HostOp::Onboarding(OnboardingOp::Rescan { win, generation, extra }))
            }
            Effect::Scroll(by) => batch.push_effect(HostOp::Onboarding(OnboardingOp::Scroll { win, by })),
            Effect::Write(generated) => batch.push_effect(HostOp::Onboarding(OnboardingOp::Write {
                win,
                generation,
                generated,
            })),
            Effect::Marker(event) => batch.push_effect(HostOp::Onboarding(OnboardingOp::Marker {
                win,
                event,
                today: today.clone(),
            })),
            Effect::OpenLines(lines) => batch.push_effect(HostOp::Onboarding(OnboardingOp::OpenLines { win, lines })),
            Effect::OpenTutor => batch.push_effect(HostOp::Onboarding(OnboardingOp::OpenTutor { win })),
            Effect::Close => close(w, win, batch),
        }
    }
    sync_view(w);
}

/// The chrome shows what the screen shows now, announced when that changed.
fn sync_view(w: &mut Window) {
    let view = w.onboarding.as_ref().and_then(|state| state.screen.view());
    if w.chrome.onboarding != view {
        w.chrome.onboarding = view;
        chrome::touched(w);
    }
}

/// The keys go to the screen. The module that holds them now is remembered, so they can go back to it.
fn focus_screen(w: &mut Window, win: WinId, batch: &mut Batch) {
    let layout_focus = w.layout.try_borrow().ok().map(|layout| layout.focus().clone());
    let native = w.focus.native_module().cloned();
    let Some(state) = w.onboarding.as_mut() else {
        return;
    };
    state.held_module = native.or(state.held_module.take()).or(layout_focus);
    batch.push_layout(HostOp::Focus {
        win,
        to: FocusTo::Onboarding,
        on_grab: vec![],
    });
}

/// `module` was just given the keys from outside this window (a file handed over): while the screen holds them it
/// goes on holding them, and they go to `module` when it closes. A held screen and a press test have given the keys
/// back already, and they stay where they went.
pub(in crate::wm) fn keep_keys(w: &mut Window, win: WinId, module: &ModuleId, batch: &mut Batch) {
    let Some(state) = w.onboarding.as_mut().filter(|state| state.held_module.is_some()) else {
        return;
    };
    state.held_module = Some(module.clone());
    batch.push_layout(HostOp::Focus {
        win,
        to: FocusTo::Onboarding,
        on_grab: vec![],
    });
}

/// The keys go back to the module that held them; while it is gone or hidden, to what the layout has now.
fn focus_back(w: &mut Window, win: WinId, batch: &mut Batch) {
    let Some(state) = w.onboarding.as_mut() else {
        return;
    };
    // The screen did not have the keys: there is nothing to give back.
    let Some(held) = state.held_module.take() else {
        return;
    };
    let layout = w.layout.clone();
    let target = match layout.try_borrow() {
        Ok(layout) if layout.can_focus(&held).is_ok() => Some(held),
        Ok(layout) => Some(layout.focus().clone()),
        Err(_) => None,
    };
    match target {
        Some(target) => {
            give_keys(w, win, &target, arrive_tail(win, &target, false), batch);
        }
        None => w.warn("[onboarding] BUG: the layout was borrowed when the screen gave the keys back".into()),
    }
}

fn arm_press_timer(w: &mut Window) {
    cancel_press_timer(w);
    let token = w.arm_after(crate::onboarding::screen::PRESS_TEST_TIMEOUT);
    if let Some(state) = w.onboarding.as_mut() {
        state.press_timer = Some(token);
    }
}

fn cancel_press_timer(w: &mut Window) {
    let old = w.onboarding.as_mut().and_then(|state| state.press_timer.take());
    if let Some(old) = old {
        w.cancel_timer(old);
    }
}

/// The screen is over: its timer is cancelled, its view comes down, and the keys go back if it had them.
fn close(w: &mut Window, win: WinId, batch: &mut Batch) {
    cancel_press_timer(w);
    let keys_here = w.onboarding.as_ref().is_some_and(|state| state.held_module.is_some());
    if keys_here {
        focus_back(w, win, batch);
    }
    w.onboarding = None;
    w.say("[onboarding] screen closed".into());
}

/// A timer fired: the press test's, when it is this screen's. `false` for any other token, including a stale one.
pub(super) fn timer_fired(w: &mut Window, win: WinId, token: TimerToken, batch: &mut Batch) -> bool {
    let Some(state) = w.onboarding.as_mut().filter(|state| state.press_timer == Some(token)) else {
        return false;
    };
    state.press_timer = None;
    let effects = state.screen.press_timed_out();
    carry(w, win, effects, batch);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::prefix::{Press, Waiting};
    use crate::keymap::{Action, KeyName, KeySpec, Keymap};
    use crate::layout::{Axis, Layout, ModuleKeys, ModuleKind, Node};
    use crate::onboarding::screen::{CheckWhy, Step, PRESS_TEST_TIMEOUT};
    use crate::onboarding::{Build, DesktopRow, Found, Lang, OpenMode, Opening, TmuxRow, WmMod, WriteOutcome};
    use crate::wm::chrome::ChromeModel;
    use crate::wm::fake::{FakeHost, FakeScheduler, Logged};
    use crate::wm::verbs::rig::{decl, rig_with, WIN};
    use crate::wm::{Driver, Origin};
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::rc::Rc;

    struct Rig {
        driver: Rc<Driver>,
        host: Rc<FakeHost>,
        scheduler: Rc<FakeScheduler>,
        layout: Rc<RefCell<Layout>>,
    }

    fn two() -> Layout {
        Layout::new(
            Node::split(
                Axis::Row,
                0.5,
                Node::Leaf(ModuleId::editor()),
                Node::Leaf(ModuleId::agent()),
            ),
            ModuleId::editor(),
        )
        .unwrap()
    }

    fn terminal2() -> ModuleId {
        ModuleId::instance(ModuleKind::Terminal, 2).unwrap()
    }

    /// `editor | (agent / terminal#2)`, the keys on the second terminal.
    fn with_a_second_terminal() -> Layout {
        Layout::new(
            Node::split(
                Axis::Row,
                0.5,
                Node::Leaf(ModuleId::editor()),
                Node::split(
                    Axis::Column,
                    0.5,
                    Node::Leaf(ModuleId::agent()),
                    Node::Leaf(terminal2()),
                ),
            ),
            terminal2(),
        )
        .unwrap()
    }

    fn driven(layout: Layout) -> Rig {
        let mut r = rig_with(layout, |_, _| None);
        let keymap = Keymap::defaults();
        let module_keys = ModuleKeys::build(&[], &keymap).unwrap();
        r.controller.set_keymap(WIN, Rc::new(keymap), Rc::new(module_keys));
        let hosted: Vec<_> = r.layout.borrow().modules().iter().map(decl).collect();
        for d in hosted {
            r.host.with_surface(WIN, d.module, d.kind, false);
        }
        let driver = Driver::new(r.controller, r.host.clone(), r.scheduler.clone());
        let sink: Rc<dyn HostSink> = driver.clone();
        r.host.attach(Rc::downgrade(&sink));
        r.scheduler.attach(Rc::downgrade(&sink));
        for op in r.first.ops() {
            r.host.apply(op);
        }
        Rig {
            driver,
            host: r.host,
            scheduler: r.scheduler,
            layout: r.layout,
        }
    }

    fn opening(generation: u64, trust_open: bool) -> Opening {
        Opening {
            generation,
            mode: OpenMode::First,
            show_why: None,
            trust_open,
            lang: Lang::En,
            build: Build::THIS_BINARY,
            running_prefix: KeySpec::parse("C-b").unwrap(),
            keymap_check: Keymap::defaults()
                .bindings()
                .iter()
                .map(|b| (b.key, b.action.name()))
                .collect(),
            today: "2026-10-31".to_string(),
            config_path: "/scratch/config".to_string(),
        }
    }

    fn key_press(press: Press, key: Option<KeyName>, ctrl: bool) -> KeyPress {
        KeyPress {
            keycode: 42,
            press,
            key,
            mods: Mods {
                ctrl,
                ..Mods::default()
            },
            super_or_hyper: false,
            super_or_meta: false,
        }
    }

    fn key(text: &str) -> KeyPress {
        let spec = KeySpec::parse(text).unwrap();
        key_press(Press::Key(spec), Some(spec.key), spec.ctrl)
    }

    fn escape() -> KeyPress {
        key_press(Press::Escape, None, false)
    }

    fn enter() -> KeyPress {
        let spec = KeySpec::named(KeyName::Enter, false, false, false);
        key_press(Press::Key(spec), Some(spec.key), false)
    }

    fn modifier() -> KeyPress {
        key_press(Press::Modifier, None, false)
    }

    fn tmux_row(prefix: &str) -> Found {
        Found::Tmux(TmuxRow {
            path: Some("/home/u/.tmux.conf".to_string()),
            prefix: Some(KeySpec::parse(prefix).unwrap()),
            changes_keys: false,
        })
    }

    fn sway_row() -> Found {
        Found::Desktop(DesktopRow {
            wm: Some("sway"),
            live: true,
            wm_mod: Some(WmMod::Super),
            note: None,
            binds: Vec::new(),
        })
    }

    impl Rig {
        fn mark(&self) -> usize {
            self.host.log().len()
        }

        fn ops_since(&self, mark: usize) -> Vec<HostOp> {
            self.host.log()[mark..]
                .iter()
                .filter_map(|logged| match logged {
                    Logged::Op(op, _) => Some(op.clone()),
                    _ => None,
                })
                .collect()
        }

        fn chrome(&self) -> ChromeModel {
            self.driver.query(|c| c.chrome(WIN))
        }

        fn notes(&self) -> Vec<String> {
            self.driver.query(|c| c.notes(WIN))
        }

        fn step(&self) -> Option<(Step, bool)> {
            self.driver.query(|c| c.onboarding_step(WIN))
        }

        fn key(&self, press: KeyPress) -> Verdict {
            self.driver.key(WIN, &KeyStage::Window, press)
        }

        fn keys(&self, text: &[&str]) {
            for key_text in text {
                assert_eq!(self.key(key(key_text)), Verdict::Consumed, "{key_text}");
            }
        }

        fn native(&self, module: &ModuleId) {
            self.host.post(HostEvent::NativeFocus {
                win: WIN,
                owner: Some(FocusOwner::Module(module.clone())),
                chain: FocusChain(vec![module.clone()]),
                active: true,
                op_index: None,
            });
        }

        fn ask(&self) {
            self.driver.run(WIN, Action::WindowSetup, Origin::Key { keycode: 7 });
        }

        fn post(&self, event: OnboardingEvent) {
            self.driver.post(HostEvent::Onboarding(event));
        }

        /// `window.setup` asked and the check answered: the screen is up, generation 1.
        fn open(&self) {
            self.ask();
            self.post(OnboardingEvent::Open {
                win: WIN,
                opening: opening(1, false),
            });
        }

        fn found(&self, generation: u64, found: Found) {
            self.post(OnboardingEvent::Found {
                win: WIN,
                generation,
                found,
            });
        }

        /// The press test, reached through preset 3 and the key `a`.
        fn press_test(&self) {
            self.keys(&["3"]);
            assert_eq!(self.key(enter()), Verdict::Consumed);
            self.keys(&["a"]);
            assert_eq!(self.key(enter()), Verdict::Consumed);
            assert!(
                matches!(self.step(), Some((Step::Press { .. }, true))),
                "{:?}",
                self.step()
            );
        }
    }

    fn focus_ops(ops: &[HostOp]) -> Vec<FocusTo> {
        ops.iter()
            .filter_map(|op| match op {
                HostOp::Focus { to, .. } => Some(to.clone()),
                _ => None,
            })
            .collect()
    }

    fn chrome_ops(ops: &[HostOp]) -> usize {
        ops.iter().filter(|op| matches!(op, HostOp::Chrome { .. })).count()
    }

    fn onboarding_ops(ops: &[HostOp]) -> Vec<OnboardingOp> {
        ops.iter()
            .filter_map(|op| match op {
                HostOp::Onboarding(op) => Some(op.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn window_setup_runs_check_with_requested_true() {
        let r = driven(two());
        let at = r.mark();
        r.ask();
        assert_eq!(
            onboarding_ops(&r.ops_since(at)),
            [OnboardingOp::Check {
                win: WIN,
                generation: 1,
                requested: true
            }]
        );
        assert!(
            r.chrome().onboarding.is_none(),
            "nothing is shown before the check answers"
        );
    }

    fn hand_over(r: &Rig, id: u64) {
        r.driver.post(HostEvent::OpenFile {
            win: WIN,
            id,
            path: "/project/a.rs".into(),
            line: None,
            col: None,
            token: None,
        });
    }

    #[test]
    fn a_file_handed_over_under_the_screen_leaves_it_the_keys() {
        let r = driven(with_a_second_terminal());
        r.open();
        let at = r.mark();
        hand_over(&r, 7);
        let ops = r.ops_since(at);
        assert!(
            ops.iter()
                .any(|op| matches!(op, HostOp::OpenAnswer { id: 7, accepted: true })),
            "{ops:?}"
        );
        assert_eq!(
            focus_ops(&ops).last(),
            Some(&FocusTo::Onboarding),
            "the editor took the keys in the layout, and the screen has them back in the same batch"
        );
        assert_eq!(r.key(key("j")), Verdict::Consumed, "the screen still answers");
        let at = r.mark();
        assert_eq!(r.key(escape()), Verdict::Consumed);
        assert_eq!(
            focus_ops(&r.ops_since(at)),
            [FocusTo::Module(ModuleId::editor())],
            "the keys go to the file, not to the terminal that held them before it came"
        );
    }

    #[test]
    fn a_file_handed_over_during_a_trust_hold_takes_the_keys_as_it_always_does() {
        let r = driven(with_a_second_terminal());
        r.ask();
        r.post(OnboardingEvent::Open {
            win: WIN,
            opening: opening(1, true),
        });
        let at = r.mark();
        hand_over(&r, 8);
        assert_eq!(
            focus_ops(&r.ops_since(at)),
            [FocusTo::Module(ModuleId::editor())],
            "a held screen does not have the keys"
        );
    }

    #[test]
    fn a_second_setup_while_the_screen_is_up_does_nothing() {
        let r = driven(two());
        r.open();
        assert_eq!(r.driver.onboarding_generation(WIN), Some(1));
        let at = r.mark();
        r.ask();
        assert!(onboarding_ops(&r.ops_since(at)).is_empty());
        assert_eq!(r.driver.onboarding_generation(WIN), Some(1));
    }

    #[test]
    fn focus_onboarding_is_issued_on_show_and_the_module_restored_on_close() {
        let r = driven(two());
        r.native(&ModuleId::agent());
        let at = r.mark();
        r.open();
        let ops = r.ops_since(at);
        assert_eq!(focus_ops(&ops), [FocusTo::Onboarding]);
        let focus_at = ops.iter().position(|op| matches!(op, HostOp::Focus { .. })).unwrap();
        let chrome_at = ops.iter().position(|op| matches!(op, HostOp::Chrome { .. })).unwrap();
        assert!(
            focus_at < chrome_at,
            "the grab comes before the chrome op, as the rename entry's does"
        );
        assert!(onboarding_ops(&ops).contains(&OnboardingOp::Detect {
            win: WIN,
            generation: 1
        }));
        assert!(onboarding_ops(&ops).contains(&OnboardingOp::Rescan {
            win: WIN,
            generation: 1,
            extra: vec![KeySpec::parse("C-b").unwrap()],
        }));
        assert!(r.chrome().onboarding.is_some());
        let at = r.mark();
        assert_eq!(r.key(escape()), Verdict::Consumed);
        let ops = r.ops_since(at);
        assert_eq!(focus_ops(&ops), [FocusTo::Module(ModuleId::agent())]);
        assert!(r.chrome().onboarding.is_none());
        assert_eq!(r.driver.onboarding_generation(WIN), None);
    }

    #[test]
    fn a_scroll_key_on_the_screen_is_an_op_and_reaches_nothing_else() {
        let r = driven(two());
        r.native(&ModuleId::editor());
        r.open();
        let at = r.mark();
        assert_eq!(r.key(key("NPage")), Verdict::Consumed);
        assert_eq!(
            onboarding_ops(&r.ops_since(at)),
            [OnboardingOp::Scroll {
                win: WIN,
                by: crate::onboarding::Scroll::PageDown
            }]
        );
    }

    #[test]
    fn the_screen_answers_after_the_prompt_and_before_the_prefix() {
        let r = driven(two());
        r.native(&ModuleId::editor());
        r.open();
        // With the screen up the prefix key arms nothing.
        assert_eq!(r.key(key("C-b")), Verdict::Consumed);
        assert_eq!(r.driver.query(|c| c.prefix_shown(WIN)).unwrap().0, Waiting::No);
    }

    #[test]
    fn a_close_of_the_window_with_the_screen_up_asks_as_it_always_does() {
        let r = driven(two());
        r.native(&ModuleId::editor());
        r.open();
        // A y/n raised over the screen takes the key first: the `d` that would decline the screen answers no.
        r.host.set_editor_running(true);
        r.host.set_chat_kill_counts(
            crate::tabs::ViewId::BASE,
            ChatKillCounts {
                running: 1,
                ..Default::default()
            },
        );
        r.driver.close_requested(WIN, CloseBy::User);
        assert!(r.chrome().prompt.is_some(), "the close asks as it always does");
        let at = r.mark();
        assert_eq!(r.key(key("d")), Verdict::Consumed);
        assert!(r.chrome().prompt.is_none(), "the y/n heard the key");
        assert!(r.chrome().onboarding.is_some(), "the screen did not");
        assert!(onboarding_ops(&r.ops_since(at)).is_empty());
        assert!(matches!(r.step(), Some((Step::Choose { .. }, false))));
    }

    #[test]
    fn a_bare_modifier_goes_on_to_the_prefix() {
        let r = driven(two());
        r.open();
        assert_eq!(r.key(modifier()), Verdict::PassThrough);
    }

    #[test]
    fn the_press_test_gives_the_keys_back_and_the_stage_still_reads_them() {
        let r = driven(two());
        r.native(&ModuleId::editor());
        r.open();
        let at = r.mark();
        r.press_test();
        assert_eq!(focus_ops(&r.ops_since(at)), [FocusTo::Module(ModuleId::editor())]);
        // The editor has the keys now; the window stage reads the key first.
        let at = r.mark();
        assert_eq!(r.key(key("C-a")), Verdict::Consumed);
        assert_eq!(
            focus_ops(&r.ops_since(at)),
            [FocusTo::Onboarding],
            "the keys are back on the screen"
        );
        assert!(
            matches!(r.step(), Some((Step::Review, false))),
            "the key arrived, and the timer is cancelled: {:?}",
            r.step()
        );
    }

    #[test]
    fn timer_fired_for_the_press_timer_calls_press_timed_out_and_a_stale_token_does_nothing() {
        let r = driven(two());
        r.native(&ModuleId::editor());
        r.open();
        r.press_test();
        let at = r.mark();
        r.driver.post(HostEvent::Timer(TimerToken(987_654)));
        assert!(
            r.ops_since(at).is_empty(),
            "a token that is not the screen's does nothing"
        );
        assert!(matches!(r.step(), Some((Step::Press { timed_out: false, .. }, true))));
        r.scheduler.advance(PRESS_TEST_TIMEOUT);
        assert_eq!(focus_ops(&r.ops_since(at)), [FocusTo::Onboarding]);
        assert!(matches!(r.step(), Some((Step::Press { timed_out: true, .. }, false))));
    }

    #[test]
    fn esc_in_the_press_test_cancels_the_timer() {
        let r = driven(two());
        r.native(&ModuleId::editor());
        r.open();
        r.press_test();
        assert_eq!(r.key(escape()), Verdict::Consumed);
        assert!(matches!(r.step(), Some((Step::Review, false))));
        let at = r.mark();
        r.scheduler.advance(PRESS_TEST_TIMEOUT);
        assert!(r.ops_since(at).is_empty(), "the cancelled timer never fires");
    }

    #[test]
    fn a_trust_hold_gives_the_keys_back_and_the_screen_comes_back() {
        let r = driven(two());
        r.native(&ModuleId::editor());
        r.open();
        let at = r.mark();
        r.found(0, Found::TrustOpen(true));
        assert_eq!(focus_ops(&r.ops_since(at)), [FocusTo::Module(ModuleId::editor())]);
        assert!(r.chrome().onboarding.is_none(), "a held screen shows nothing");
        assert_eq!(
            r.key(key("y")),
            Verdict::PassThrough,
            "the card's key goes on to the window stage's prefix"
        );
        let at = r.mark();
        r.found(0, Found::TrustOpen(false));
        assert_eq!(focus_ops(&r.ops_since(at)), [FocusTo::Onboarding]);
        assert!(r.chrome().onboarding.is_some());
    }

    #[test]
    fn a_question_open_before_the_screen_never_takes_the_keys() {
        let r = driven(two());
        r.native(&ModuleId::editor());
        r.ask();
        let at = r.mark();
        r.post(OnboardingEvent::Open {
            win: WIN,
            opening: opening(1, true),
        });
        assert!(focus_ops(&r.ops_since(at)).is_empty());
        assert!(r.chrome().onboarding.is_none());
        assert_eq!(r.key(key("j")), Verdict::PassThrough);
        let at = r.mark();
        r.found(0, Found::TrustOpen(false));
        assert_eq!(focus_ops(&r.ops_since(at)), [FocusTo::Onboarding]);
        assert!(r.chrome().onboarding.is_some());
    }

    #[test]
    fn found_events_fill_rows() {
        let r = driven(two());
        r.open();
        let before = r.chrome().onboarding.unwrap().rows;
        r.found(1, sway_row());
        r.found(1, tmux_row("C-a"));
        let after = r.chrome().onboarding.unwrap().rows;
        assert_eq!(before.len(), after.len());
        assert_ne!(before, after, "the rows say what was found");
    }

    #[test]
    fn a_change_of_the_view_announces_chrome_once() {
        let r = driven(two());
        r.open();
        let at = r.mark();
        r.found(1, tmux_row("C-a"));
        assert_eq!(chrome_ops(&r.ops_since(at)), 1);
        let at = r.mark();
        r.found(1, tmux_row("C-a"));
        assert_eq!(
            chrome_ops(&r.ops_since(at)),
            0,
            "the same answer changes nothing on screen"
        );
    }

    #[test]
    fn a_write_asks_the_host_and_its_answers_reach_the_screen() {
        let r = driven(two());
        r.open();
        assert_eq!(r.key(enter()), Verdict::Consumed);
        let at = r.mark();
        assert_eq!(r.key(enter()), Verdict::Consumed);
        let ops = onboarding_ops(&r.ops_since(at));
        assert!(
            matches!(
                ops.as_slice(),
                [OnboardingOp::Write {
                    win: WIN,
                    generation: 1,
                    ..
                }]
            ),
            "{ops:?}"
        );
        assert!(matches!(r.step(), Some((Step::Writing, false))));
        r.post(OnboardingEvent::Written {
            win: WIN,
            generation: 1,
            outcome: WriteOutcome::Written(PathBuf::from("/scratch/config/init.lua")),
        });
        assert!(matches!(r.step(), Some((Step::Done { .. }, false))), "{:?}", r.step());

        let again = driven(two());
        again.open();
        again.key(enter());
        again.key(enter());
        again.post(OnboardingEvent::CheckFailed {
            win: WIN,
            generation: 1,
            why: CheckWhy::Unverified,
        });
        assert!(
            matches!(again.step(), Some((Step::NotWritten { .. }, false))),
            "{:?}",
            again.step()
        );
    }

    #[test]
    fn the_screen_the_start_opens_is_up_under_generation_zero_and_hears_its_facts() {
        let r = driven(two());
        r.native(&ModuleId::editor());
        assert_eq!(r.driver.onboarding_generation(WIN), None, "no screen, no generation");
        r.post(OnboardingEvent::Open {
            win: WIN,
            opening: opening(0, false),
        });
        assert_eq!(
            r.driver.onboarding_generation(WIN),
            Some(0),
            "generation 0 is a screen's own"
        );
        let before = r.chrome().onboarding;
        assert!(before.is_some());
        r.found(0, tmux_row("C-a"));
        assert_ne!(
            r.chrome().onboarding,
            before,
            "a fact posted under the screen's generation arrives"
        );
    }

    #[test]
    fn a_late_answer_after_the_screen_closed_changes_nothing() {
        let r = driven(two());
        r.native(&ModuleId::editor());
        r.open();
        assert_eq!(r.key(escape()), Verdict::Consumed);
        let notes = r.notes().len();
        let at = r.mark();
        r.found(1, tmux_row("C-a"));
        r.found(0, Found::TrustOpen(true));
        r.post(OnboardingEvent::Written {
            win: WIN,
            generation: 1,
            outcome: WriteOutcome::Written(PathBuf::from("/x")),
        });
        r.post(OnboardingEvent::CheckFailed {
            win: WIN,
            generation: 1,
            why: CheckWhy::Unverified,
        });
        assert!(
            r.ops_since(at).is_empty(),
            "no op, no focus move: {:?}",
            r.ops_since(at)
        );
        assert!(r.chrome().onboarding.is_none());
        assert_eq!(r.notes().len(), notes, "and nothing said above the log");
        // A screen opened again has another generation: the old screen's answers do not reach it.
        r.ask();
        r.post(OnboardingEvent::Open {
            win: WIN,
            opening: opening(2, false),
        });
        let view = r.chrome().onboarding.unwrap();
        let at = r.mark();
        r.found(1, tmux_row("C-a"));
        r.post(OnboardingEvent::CheckFailed {
            win: WIN,
            generation: 1,
            why: CheckWhy::Unverified,
        });
        assert!(r.ops_since(at).is_empty());
        assert_eq!(r.chrome().onboarding.unwrap(), view);
        assert!(matches!(r.step(), Some((Step::Choose { .. }, false))));
    }

    #[test]
    fn an_open_for_another_generation_is_dropped() {
        let r = driven(two());
        r.ask();
        r.ask();
        r.post(OnboardingEvent::Open {
            win: WIN,
            opening: opening(1, false),
        });
        assert!(
            r.chrome().onboarding.is_none(),
            "the first check was superseded by the second"
        );
        r.post(OnboardingEvent::Open {
            win: WIN,
            opening: opening(2, false),
        });
        assert!(r.chrome().onboarding.is_some());
    }

    #[test]
    fn closing_after_the_held_module_went_away_focuses_what_the_layout_has() {
        let r = driven(with_a_second_terminal());
        r.native(&terminal2());
        r.open();
        r.layout.borrow_mut().remove(&terminal2()).unwrap();
        let now = r.layout.borrow().focus().clone();
        assert_ne!(now, terminal2());
        let at = r.mark();
        assert_eq!(r.key(escape()), Verdict::Consumed);
        assert_eq!(focus_ops(&r.ops_since(at)), [FocusTo::Module(now)]);
    }

    #[test]
    fn eitri_layout_run_refuses_window_setup() {
        assert_eq!(
            crate::lua::layout::run_refusal("window.setup", &Action::WindowSetup).as_deref(),
            Some("window.setup is not a layout action")
        );
    }
}
