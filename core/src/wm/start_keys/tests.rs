use super::*;
use crate::keymap::prefix::Press;
use crate::keymap::{KeySpec, Keymap};
use crate::layout::{Axis, Layout, ModuleId, ModuleKeys, Node, Size};
use crate::wm::fake::{FakeHost, FakeScheduler};
use crate::wm::verbs::rig::decl;
use crate::wm::*;
use std::cell::RefCell;
use std::rc::Rc;

const WIN: WinId = WinId::MAIN;

fn typed(text: &str) -> KeyPress {
    let spec = KeySpec::parse(text).unwrap();
    KeyPress {
        keycode: 53,
        press: Press::Key(spec),
        key: Some(spec.key),
        mods: Mods {
            ctrl: spec.ctrl,
            alt: spec.meta,
            shift: spec.shift,
        },
        super_or_hyper: false,
        super_or_meta: false,
    }
}

/// A chat window's controller behind a driver over the fake host: `agent` beside `editor`, the keys on the chat.
fn chat_window(role: PlacementRole) -> (Rc<Driver<Controller>>, Rc<FakeHost>, Rc<FakeScheduler>) {
    let host = FakeHost::new();
    host.set_grid_size(Size { w: 1200, h: 800 });
    let scheduler = FakeScheduler::new();
    let mut controller = Controller::new(scheduler.clone());
    controller.set_reads(host.clone());
    let keymap = Keymap::defaults();
    let module_keys = ModuleKeys::build(&[], &keymap).unwrap();
    controller.set_keymap(WIN, Rc::new(keymap), Rc::new(module_keys));
    let layout = Layout::new(
        Node::split(
            Axis::Row,
            0.5,
            Node::Leaf(ModuleId::agent()),
            Node::Leaf(ModuleId::editor()),
        ),
        ModuleId::agent(),
    )
    .unwrap();
    let hosted: Vec<_> = layout.modules().iter().map(decl).collect();
    for d in &hosted {
        host.with_surface(WIN, d.module.clone(), d.kind.clone(), false);
    }
    controller.open_window(
        WIN,
        WindowSpec {
            role: WindowRole::Main,
            presence: EditorPresence::Owned,
            title: "project".into(),
        },
        WindowParts {
            layout: Rc::new(RefCell::new(layout)),
            workspaces: Rc::new(RefCell::new(crate::layout::Workspaces::single())),
            saver: None,
        },
    );
    let driver = Driver::new(controller, host.clone(), scheduler.clone());
    let sink: Rc<dyn HostSink> = driver.clone();
    host.attach(Rc::downgrade(&sink));
    scheduler.attach(Rc::downgrade(&sink));
    driver.first_apply(WIN, hosted);
    driver.set_placement(
        WIN,
        WindowPlacement {
            role,
            remembered: None,
            restore_second: false,
        },
    );
    (driver, host, scheduler)
}

fn key(driver: &Driver<Controller>, text: &str) -> Verdict {
    driver.key(WIN, &KeyStage::Window, typed(text))
}

fn active(driver: &Driver<Controller>, active: bool) {
    driver.post(HostEvent::Active { win: WIN, active });
}

fn mapped_on_an_output(driver: &Driver<Controller>) {
    driver.post(HostEvent::WindowMonitor {
        win: WIN,
        monitor: Some(MonitorId("DP-1".into())),
    });
}

fn notes(driver: &Driver<Controller>) -> Vec<String> {
    driver.query(|c: &Controller| c.window(WIN).map(|w| w.notes().to_vec()).unwrap_or_default())
}

const RESTORED: PlacementRole = PlacementRole::RestoredSecond { first_pid: 101 };

#[test]
fn a_window_started_to_leave_the_keys_elsewhere_drops_what_it_is_typed_until_they_go_back() {
    let (driver, _host, _scheduler) = chat_window(RESTORED);
    assert_eq!(key(&driver, "x"), Verdict::Consumed, "a key before the window maps");
    // The compositor maps the window and gives it the keys; the placement is asked to hand them back.
    mapped_on_an_output(&driver);
    active(&driver, true);
    assert_eq!(key(&driver, "x"), Verdict::Consumed, "typed in window 1, landed here");
    assert_eq!(
        key(&driver, "Enter"),
        Verdict::Consumed,
        "not even Enter reaches the composer"
    );
    assert_eq!(key(&driver, "C-b"), Verdict::Consumed, "nor the prefix");
    // The focus command ran: the keys are back in window 1.
    active(&driver, false);
    // Later the person brings this window forward: its keys are its own.
    active(&driver, true);
    assert_eq!(key(&driver, "x"), Verdict::PassThrough);
    let notes = notes(&driver);
    let dropped: Vec<_> = notes.iter().filter(|n| n.contains("it is dropped")).collect();
    assert_eq!(dropped.len(), 1, "said once: {notes:#?}");
    assert!(
        notes
            .iter()
            .any(|n| n == "[focus] the window takes its keys (they went back); 4 key(s) dropped before"),
        "{notes:#?}"
    );
}

#[test]
fn a_click_in_the_window_ends_the_hold() {
    let (driver, _host, _scheduler) = chat_window(RESTORED);
    active(&driver, true);
    assert_eq!(key(&driver, "x"), Verdict::Consumed);
    driver.post(HostEvent::Pointer {
        win: WIN,
        on: PointerTarget::Window,
        ev: PointerEv::Press {
            x: 3.0,
            y: 4.0,
            button: 1,
        },
    });
    assert_eq!(key(&driver, "x"), Verdict::PassThrough, "the person reached for it");
}

#[test]
fn the_backstop_ends_a_hold_the_compositor_never_answers() {
    // Asked to hand the keys back, but the window never had them (a compositor that does not focus a new window).
    let (driver, _host, scheduler) = chat_window(RESTORED);
    mapped_on_an_output(&driver);
    scheduler.advance(BACKSTOP - Duration::from_millis(1));
    assert_eq!(key(&driver, "x"), Verdict::Consumed);
    scheduler.advance(Duration::from_millis(1));
    assert_eq!(key(&driver, "x"), Verdict::PassThrough);
    // The keys left here by the compositor (a rule that focuses the window): the backstop runs from their arrival.
    let (driver, _host, scheduler) = chat_window(RESTORED);
    active(&driver, true);
    scheduler.advance(BACKSTOP);
    assert_eq!(key(&driver, "x"), Verdict::PassThrough);
}

#[test]
fn a_released_hold_leaves_no_timer_behind() {
    let (driver, _host, scheduler) = chat_window(RESTORED);
    mapped_on_an_output(&driver);
    active(&driver, true);
    let armed = scheduler.armed().len();
    active(&driver, false);
    assert_eq!(scheduler.armed().len(), armed - 1, "the backstop is cancelled");
}

#[test]
fn windows_that_take_the_keys_hold_nothing() {
    for role in [PlacementRole::Main, PlacementRole::NewSecond, PlacementRole::NewModule] {
        let (driver, _host, _scheduler) = chat_window(role);
        mapped_on_an_output(&driver);
        active(&driver, true);
        assert_eq!(key(&driver, "x"), Verdict::PassThrough, "{role:?}");
    }
}
