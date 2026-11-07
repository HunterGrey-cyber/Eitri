use super::super::rig::*;
use super::super::tests::{one_of_each, three};
use crate::agent_backend::AgentId;
use crate::keymap::action::{Action, TabAction};
use crate::keymap::compositor::off_hint;
use crate::keymap::prefix::{Press, Waiting};
use crate::keymap::{KeySpec, Keymap};
use crate::layout::{Axis, Direction, Layout, ModuleId, ModuleKeys, ModuleKind, Node};
use crate::lua::layout::LayoutRequest;
use crate::plugin::follow_feed::FollowCmd;
use crate::tabs::{TabId, ViewId};
use crate::window_link::names::WindowName;
use crate::window_link::HubAsk;
use crate::wm::fake::FakePeerPort;
use crate::wm::feedback::{BESIDE_NO_CHAT_TEXT, BESIDE_OFF_TEXT, BESIDE_TMUX_TEXT};
use crate::wm::window::{ModuleWindowKind, Tiling};
use crate::wm::*;
use std::rc::Rc;

fn agent() -> ModuleId {
    ModuleId::agent()
}

/// `editor | terminal`, the keys on the editor.
fn editor_and_terminal() -> Layout {
    Layout::new(
        Node::split(
            Axis::Row,
            0.5,
            Node::Leaf(ModuleId::editor()),
            Node::Leaf(ModuleId::terminal()),
        ),
        ModuleId::editor(),
    )
    .unwrap()
}

fn rig_in(tiling: Tiling, layout: Layout) -> Rig {
    let mut r = rig(layout);
    r.host.set_compositor_name("niri");
    r.host.set_grid_size(crate::layout::Size { w: 1200, h: 800 });
    r.controller.window_mut(WIN).unwrap().tiling = tiling;
    r
}

#[test]
fn a_hubs_top_bar_sums_its_chat_windows_under_one_word() {
    let mut r = hub();
    for (peer, name) in [(16, WindowName::Chat(1)), (17, WindowName::Chat(2))] {
        attach(&mut r, peer, name);
        r.controller.handle(HostEvent::Peer(PeerEvent::Summary {
            peer: PeerId(peer),
            summary: crate::window_link::Summary {
                waiting: 1,
                running: 2,
                ..Default::default()
            },
        }));
    }
    assert_eq!(r.controller.chrome(WIN).top_bar.other.as_deref(), Some("chats: ⚑2 ●4"));
}

#[test]
fn the_terminal_key_shows_the_bottom_terminal_in_window_1() {
    use crate::wm::start::hub_layout;
    let first_launch = crate::wm::persist::first_launch_layout(&[]).unwrap();
    let mut r = rig_in(Tiling::Hub, hub_layout(&first_launch, false, None).0);
    let terminal = ModuleId::terminal();
    assert!(!r.layout.borrow().is_shown(&terminal));
    r.controller
        .run(WIN, Action::Module(terminal.clone()), Origin::Key { keycode: 1 });
    assert!(r.layout.borrow().is_shown(&terminal));
    assert!(r.layout.borrow().is_visible(&terminal));
}

fn hub() -> Rig {
    rig_in(Tiling::Hub, editor_and_terminal())
}

fn module_window(name: WindowName) -> Rig {
    let mut r = rig_in(Tiling::ModuleWindow, Layout::new(Node::Leaf(agent()), agent()).unwrap());
    r.controller.set_module_name(WIN, name);
    r
}

/// Attaches the module window `name` as `peer`; the port records what is sent to it.
fn attach(r: &mut Rig, peer: u32, name: WindowName) -> Batch {
    r.controller.attach_peer(WIN, PeerId(peer), name, FakePeerPort::new())
}

fn open(name: WindowName, token: Option<&str>) -> HostOp {
    HostOp::OpenModuleWindow {
        win: WIN,
        name,
        token: token.map(str::to_owned),
        keep_keys: false,
    }
}

fn peer_event(r: &mut Rig, event: PeerEvent) -> Batch {
    r.controller.handle(HostEvent::Peer(event))
}

fn said(r: &Rig, line: &str) -> usize {
    r.controller
        .notes(WIN)
        .iter()
        .filter(|note| note.as_str() == line)
        .count()
}

fn is_new_op(op: &HostOp) -> bool {
    matches!(
        op,
        HostOp::OpenModuleWindow { .. }
            | HostOp::ForwardRaise { .. }
            | HostOp::PeerRun { .. }
            | HostOp::CompositorFocus { .. }
            | HostOp::HubAsk { .. }
    )
}

#[test]
fn a_layout_key_in_window_1_hints_once_then_only_logs() {
    let mut r = hub();
    let batch = r.run(Action::Zoom);
    assert!(ops(&batch).is_empty(), "nothing moves: {:?}", ops(&batch));
    let first = r.said().toast.expect("the first press shows the hint");
    assert_eq!(first.1, off_hint("niri"));
    r.run(Action::MarkToggle);
    r.run(Action::Zoom);
    assert_eq!(r.said().toast, Some(first), "the hint is shown once, whatever the key");
    assert_eq!(said(&r, "[keys] zoom: off in compositor mode"), 2);
    assert_eq!(said(&r, "[keys] mark.toggle: off in compositor mode"), 1);
}

#[test]
fn a_layout_key_in_a_module_window_hints_once_too() {
    let mut r = module_window(WindowName::Chat(2));
    let split = Action::Split {
        axis: Axis::Row,
        cwd: Default::default(),
    };
    let batch = r.run(split.clone());
    assert!(ops(&batch).is_empty());
    let first = r.said().toast.expect("the first press shows the hint");
    assert_eq!(first.1, off_hint("niri"));
    r.run(split);
    assert_eq!(r.said().toast, Some(first));
    assert_eq!(said(&r, "[keys] split.right: off in compositor mode"), 2);
}

/// A key as the shell translates it.
fn key(text: &str, keycode: u32) -> KeyPress {
    let spec = KeySpec::parse(text).unwrap();
    KeyPress {
        keycode,
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

/// The default keymap, on `r`'s window.
fn with_keys(mut r: Rig) -> Rig {
    let keymap = Keymap::defaults();
    let module_keys = ModuleKeys::build(&[], &keymap).unwrap();
    r.controller.set_keymap(WIN, Rc::new(keymap), Rc::new(module_keys));
    r
}

fn release(r: &mut Rig, keycode: u32) {
    r.controller.handle(HostEvent::KeyReleased { win: WIN, keycode });
}

/// The prefix, then `text` as a real key press reaches them: what the prefix itself decides is part of this.
fn prefix_then(r: &mut Rig, text: &str) -> Batch {
    let (_, armed) = r.controller.key(WIN, &KeyStage::Window, &key("C-b", 56));
    assert!(ops(&armed).is_empty());
    release(r, 56);
    let (_, batch) = r.controller.key(WIN, &KeyStage::Window, &key(text, 57));
    release(r, 57);
    batch
}

fn waiting(r: &Rig) -> Waiting {
    r.controller
        .window(WIN)
        .unwrap()
        .keys
        .prefix
        .as_ref()
        .unwrap()
        .waiting()
}

#[test]
fn a_split_key_is_off_at_once_and_waits_for_no_module_key() {
    for mut r in [with_keys(hub()), with_keys(module_window(WindowName::Chat(1)))] {
        let before = r.layout.borrow().clone();
        let batch = prefix_then(&mut r, "%");
        assert_eq!(waiting(&r), Waiting::No, "the prefix is done");
        assert!(ops(&batch).is_empty(), "nothing moves: {:?}", ops(&batch));
        assert_eq!(r.said().toast.map(|t| t.1), Some(off_hint("niri")));
        assert_eq!(said(&r, "[keys] split.right: off in compositor mode"), 1);
        // The key after it is no module key: it is the page's, and nothing is placed.
        let (verdict, batch) = r.controller.key(WIN, &KeyStage::Window, &key("t", 58));
        assert_eq!(verdict, Verdict::PassThrough);
        assert!(ops(&batch).is_empty());
        assert_eq!(*r.layout.borrow(), before);
    }
}

#[test]
fn the_hide_key_hides_the_terminal_in_window_1_and_only_logs_for_a_window() {
    let mut r = with_keys(hub());
    assert!(r.layout.borrow().is_shown(&ModuleId::terminal()));
    let before = r.said();
    prefix_then(&mut r, "d");
    assert_eq!(waiting(&r), Waiting::Hide);
    let (_, batch) = r.controller.key(WIN, &KeyStage::Window, &key("a", 57));
    assert!(ops(&batch).is_empty(), "{:?}", ops(&batch));
    assert_eq!(r.said(), before, "no refusal and no toast");
    assert_eq!(
        said(&r, "[keys] module.hide-by-key: agent is a window of the compositor"),
        1
    );
    release(&mut r, 57);
    prefix_then(&mut r, "d");
    r.controller.key(WIN, &KeyStage::Window, &key("t", 57));
    assert!(
        !r.layout.borrow().is_shown(&ModuleId::terminal()),
        "the terminal is window 1's to hide"
    );

    // A module window holds one module, so its hide key waits for nothing.
    let mut r = with_keys(module_window(WindowName::Chat(1)));
    prefix_then(&mut r, "d");
    assert_eq!(waiting(&r), Waiting::No);
    assert_eq!(
        said(&r, "[keys] module.hide-by-key: nothing to act on in this window"),
        1
    );
}

#[test]
fn the_strip_lists_no_verb_that_is_off_in_the_window() {
    fn verbs(mut r: Rig) -> String {
        let _ = r.controller.key(WIN, &KeyStage::Window, &key("C-b", 56));
        let pieces = r.controller.chrome(WIN).prefix_strip.expect("the strip is up");
        pieces
            .iter()
            .filter_map(|piece| match piece {
                crate::wm::prefix_strip::StripPiece::Run { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" | ")
    }
    let hub_strip = verbs(with_keys(hub()));
    let chat_strip = verbs(with_keys(module_window(WindowName::Chat(1))));
    for strip in [&hub_strip, &chat_strip] {
        for off in ["right", "below", "even", "swap"] {
            assert!(!strip.contains(off), "{strip}");
        }
        assert!(strip.contains("? keys"), "{strip}");
    }
    assert!(hub_strip.contains("d hide"), "{hub_strip}");
    assert!(!chat_strip.contains("hide"), "{chat_strip}");
}

#[test]
fn the_chat_key_goes_to_the_chat_window_waiting_on_the_user() {
    fn waits(r: &mut Rig, peer: u32, count: u32) {
        peer_event(
            r,
            PeerEvent::Summary {
                peer: PeerId(peer),
                summary: crate::window_link::Summary {
                    waiting: count,
                    ..Default::default()
                },
            },
        );
    }
    let raised = |batch: &Batch| {
        ops(batch)
            .into_iter()
            .filter(|op| matches!(op, HostOp::RaisePeer { .. }))
            .collect::<Vec<_>>()
    };
    let raise = |peer: u32| HostOp::RaisePeer {
        win: WIN,
        peer: PeerId(peer),
    };
    let mut r = hub();
    attach(&mut r, 16, WindowName::Chat(1));
    attach(&mut r, 17, WindowName::Chat(2));
    attach(&mut r, 18, WindowName::Chat(3));
    assert_eq!(
        raised(&r.run(Action::Module(agent()))),
        vec![raise(16)],
        "nothing waits"
    );
    waits(&mut r, 18, 1);
    assert_eq!(raised(&r.run(Action::Module(agent()))), vec![raise(18)]);
    // Among several, the one last used; a chat with a number of its own is asked for by name.
    waits(&mut r, 17, 1);
    assert_eq!(raised(&r.run(Action::Module(agent()))), vec![raise(17)], "the lowest");
    peer_event(&mut r, PeerEvent::WindowFocused { peer: PeerId(18) });
    assert_eq!(
        raised(&r.run(Action::Module(agent()))),
        vec![raise(18)],
        "the last used"
    );
    assert_eq!(
        raised(&r.run(Action::Module(ModuleId::parse("agent#2").unwrap()))),
        vec![raise(17)]
    );
    // From a chat window the key takes the same way through window 1.
    let token = "tok".to_string();
    let batch = peer_event(
        &mut r,
        PeerEvent::HubAsk {
            peer: PeerId(16),
            ask: HubAsk::Module { id: "agent".into() },
            token: Some(token.clone()),
        },
    );
    assert_eq!(
        ops(&batch),
        vec![HostOp::ForwardRaise {
            win: WIN,
            peer: PeerId(18),
            token: Some(token),
        }]
    );
}

#[test]
fn a_chat_key_in_a_page_window_runs_in_the_last_chat_window() {
    let page = WindowName::for_module(&ModuleId::lua("notes")).unwrap();
    for action in [
        Action::PanelKeymap,
        Action::PanelCommandLine,
        Action::PanelReload,
        Action::TabBreak,
        Action::Tab(TabAction::New),
    ] {
        let mut r = module_window(page.clone());
        let before = r.said();
        let batch = r.run(action.clone());
        assert_eq!(r.said(), before, "{action:?}: nothing is refused");
        assert_eq!(
            ops(&batch),
            vec![HostOp::HubAsk {
                win: WIN,
                ask: HubAsk::ChatKey {
                    name: action.name(),
                    opts: action.wire_options(),
                },
            }],
            "{action:?}"
        );
    }
    // What a chat window has of its own stays there.
    let mut r = module_window(WindowName::Chat(1));
    assert!(ops(&r.run(Action::PanelReload))
        .iter()
        .all(|op| !matches!(op, HostOp::HubAsk { .. })));

    // Window 1 takes it as a key of its own, and the asker's token goes with the raise.
    let mut r = hub();
    attach(&mut r, 16, WindowName::Chat(1));
    let action = Action::Tab(TabAction::New);
    let ask = |action: &Action| PeerEvent::HubAsk {
        peer: PeerId(16),
        ask: HubAsk::ChatKey {
            name: action.name(),
            opts: action.wire_options(),
        },
        token: Some("tok".into()),
    };
    assert_eq!(
        ops(&peer_event(&mut r, ask(&action))),
        vec![
            HostOp::PeerRun {
                win: WIN,
                peer: PeerId(16),
                name: action.name(),
                opts: action.wire_options(),
            },
            HostOp::ForwardRaise {
                win: WIN,
                peer: PeerId(16),
                token: Some("tok".into()),
            },
        ]
    );
    // Nothing but a chat key is taken from that door.
    assert!(ops(&peer_event(&mut r, ask(&Action::Zoom))).is_empty());
}

#[test]
fn a_follow_show_in_window_1_opens_the_pages_window_and_leaves_the_keys() {
    let page = ModuleId::lua("markdown");
    let name = WindowName::for_module(&page).unwrap();
    let mut r = hub();
    let batch = r.controller.follow(WIN, &page, true);
    assert_eq!(
        ops(&batch),
        vec![HostOp::OpenModuleWindow {
            win: WIN,
            name: name.clone(),
            token: None,
            keep_keys: true,
        }]
    );
    assert!(ops(&r.controller.follow(WIN, &page, true)).is_empty(), "it is starting");
    let peer = 16;
    attach(&mut r, peer, name.clone());
    assert!(ops(&r.controller.follow(WIN, &page, true)).is_empty(), "it is open");
    assert!(
        ops(&r.controller.follow(WIN, &page, false)).is_empty(),
        "the compositor closes it"
    );

    // In the page's own window a follow command leaves the window as it is.
    let mut r = rig_in(
        Tiling::ModuleWindow,
        Layout::new(Node::Leaf(page.clone()), page.clone()).unwrap(),
    );
    r.controller.set_module_name(WIN, name);
    r.controller.follow(WIN, &page, false);
    assert!(r.layout.borrow().is_shown(&page));
}

#[test]
fn a_module_shown_while_agents_wait_opens_no_window_where_pages_are_windows() {
    let page = ModuleId::lua("agents-pass");
    let mut r = hub();
    assert!(
        ops(&r.controller.wait_follow(WIN, &page, true)).is_empty(),
        "no page window opens by itself: no hide would close it"
    );
    assert!(ops(&r.controller.wait_follow(WIN, &page, false)).is_empty());
    let mut r = rig_in(
        Tiling::ModuleWindow,
        Layout::new(Node::Leaf(page.clone()), page.clone()).unwrap(),
    );
    r.controller
        .set_module_name(WIN, WindowName::for_module(&page).unwrap());
    assert!(ops(&r.controller.wait_follow(WIN, &page, false)).is_empty());
    assert!(r.layout.borrow().is_shown(&page), "its own window stays as it is");

    // Where modules are tiles it is the follow command itself.
    let terminal = ModuleId::terminal();
    let mut by_follow = rig_in(Tiling::Eitri, editor_and_terminal());
    let mut by_wait = rig_in(Tiling::Eitri, editor_and_terminal());
    assert_eq!(
        ops(&by_wait.controller.wait_follow(WIN, &terminal, false)),
        ops(&by_follow.controller.follow(WIN, &terminal, false))
    );
    assert!(!by_wait.layout.borrow().is_shown(&terminal));
}

#[test]
fn a_chat_window_a_hand_over_is_starting_is_not_free() {
    let mut r = hub();
    attach(&mut r, 16, WindowName::Chat(1));
    r.host.set_open_module_windows(vec![WindowName::Chat(2)]);
    assert_eq!(
        ops(&r.run(Action::InstanceNew {
            kind: ModuleKind::Agent
        })),
        vec![open(WindowName::Chat(3), None)]
    );
}

#[test]
fn the_chat_key_raises_an_open_chat_window_or_spawns_it() {
    let mut r = hub();
    let batch = r.run(Action::Module(agent()));
    assert_eq!(ops(&batch), vec![open(WindowName::Chat(1), None)]);
    // Pressed again while the window starts: no second window.
    let batch = r.run(Action::Module(agent()));
    assert!(ops(&batch).is_empty(), "{:?}", ops(&batch));
    attach(&mut r, 16, WindowName::Chat(1));
    let batch = r.run(Action::Module(agent()));
    assert_eq!(
        ops(&batch),
        vec![HostOp::RaisePeer {
            win: WIN,
            peer: PeerId(16)
        }]
    );
    // A page is its own window, named by its id.
    let batch = r.run(Action::Module(ModuleId::lua("notes")));
    assert_eq!(ops(&batch), vec![open(WindowName::Page("notes".into()), None)]);
}

#[test]
fn a_page_with_an_id_no_window_can_carry_opens_nothing_and_says_why() {
    let mut r = hub();
    let batch = r.run(Action::Module(ModuleId::lua("My Panel")));
    assert!(ops(&batch).is_empty());
    assert_eq!(said(&r, "[window] My Panel: no page window for this id"), 1);
    let toast = r.said().toast.expect("one toast names the id and the rule");
    assert!(toast.1.starts_with("My Panel: no page window for this id"), "{toast:?}");
}

#[test]
fn instance_new_picks_the_lowest_free_chat_window() {
    let mut r = hub();
    attach(&mut r, 16, WindowName::Chat(2));
    attach(&mut r, 17, WindowName::Chat(4));
    let new = Action::InstanceNew {
        kind: ModuleKind::Agent,
    };
    assert_eq!(ops(&r.run(new.clone())), vec![open(WindowName::Chat(3), None)]);
    // c3 is starting, so it is not free: the next is c5.
    assert_eq!(ops(&r.run(new)), vec![open(WindowName::Chat(5), None)]);
}

#[test]
fn the_tenth_chat_window_is_refused_with_a_toast() {
    let mut r = hub();
    for n in 2..=9u8 {
        attach(&mut r, 15 + u32::from(n), WindowName::Chat(n));
    }
    let before = r.said();
    let batch = r.run(Action::InstanceNew {
        kind: ModuleKind::Agent,
    });
    r.assert_refused(&batch, &before, "all nine chat windows are open");
}

#[test]
fn a_tab_key_in_window_1_runs_in_the_last_used_chat_window() {
    let mut r = hub();
    attach(&mut r, 16, WindowName::Chat(1));
    attach(&mut r, 17, WindowName::Chat(2));
    let run = |peer: u32| {
        vec![
            HostOp::PeerRun {
                win: WIN,
                peer: PeerId(peer),
                name: "tab.select".into(),
                opts: vec![("n".into(), 3.into())],
            },
            HostOp::RaisePeer {
                win: WIN,
                peer: PeerId(peer),
            },
        ]
    };
    // No chat window has held the keyboard yet: the base one.
    assert_eq!(ops(&r.run(Action::Tab(TabAction::Select(3)))), run(16));
    peer_event(&mut r, PeerEvent::WindowFocused { peer: PeerId(17) });
    assert_eq!(ops(&r.run(Action::Tab(TabAction::Select(3)))), run(17));
    // A page window never becomes the last chat.
    attach(&mut r, 18, WindowName::Page("notes".into()));
    peer_event(&mut r, PeerEvent::WindowFocused { peer: PeerId(18) });
    assert_eq!(ops(&r.run(Action::Tab(TabAction::Select(3)))), run(17));
}

#[test]
fn a_tab_key_with_no_chat_window_spawns_c1_and_runs_after_it_links() {
    let mut r = hub();
    let batch = r.run(Action::Tab(TabAction::Next));
    assert_eq!(ops(&batch), vec![open(WindowName::Chat(1), None)]);
    let batch = r.run(Action::Tab(TabAction::Select(2)));
    assert!(ops(&batch).is_empty(), "c1 is already starting: {:?}", ops(&batch));
    let linked = attach(&mut r, 16, WindowName::Chat(1));
    assert_eq!(
        ops(&linked),
        vec![
            HostOp::PeerRun {
                win: WIN,
                peer: PeerId(16),
                name: "tab.next".into(),
                opts: vec![],
            },
            HostOp::PeerRun {
                win: WIN,
                peer: PeerId(16),
                name: "tab.select".into(),
                opts: vec![("n".into(), 2.into())],
            },
        ]
    );
    // What waited is sent once.
    r.controller.detach_peer(WIN, PeerId(16));
    assert!(ops(&attach(&mut r, 17, WindowName::Chat(1))).is_empty());
}

#[test]
fn a_chat_window_that_never_starts_drops_what_waited_for_it() {
    let mut r = hub();
    r.run(Action::Tab(TabAction::Next));
    let batch = r.controller.handle(HostEvent::Refused {
        what: OpTag::OpenModuleWindow,
        why: "no compositor to place it".into(),
        surface: None,
    });
    assert!(ops(&batch).is_empty());
    assert_eq!(r.said().toast.map(|t| t.1), Some("no compositor to place it".into()));
    assert!(
        r.controller
            .notes(WIN)
            .iter()
            .any(|n| n.contains("c1 did not start") && n.contains("1 waiting action(s) dropped")),
        "{:?}",
        r.controller.notes(WIN)
    );
    assert!(ops(&attach(&mut r, 16, WindowName::Chat(1))).is_empty());
    // It can be asked for again.
    r.controller.detach_peer(WIN, PeerId(16));
    assert_eq!(
        ops(&r.run(Action::Module(agent()))),
        vec![open(WindowName::Chat(1), None)]
    );
}

#[test]
fn a_chat_window_that_misses_its_handshake_can_be_opened_again() {
    let mut r = hub();
    assert_eq!(
        ops(&r.run(Action::Module(agent()))),
        vec![open(WindowName::Chat(1), None)]
    );
    // The key waits for the window that is starting; nothing is started twice.
    assert!(ops(&r.run(Action::Module(agent()))).is_empty());
    assert_eq!(said(&r, "[window] c1 is starting"), 1);
    // Another window's failure leaves it starting.
    let batch = r.controller.handle(HostEvent::ModuleWindowFailed {
        win: WIN,
        name: WindowName::Chat(2),
        why: "gone".into(),
    });
    assert!(ops(&batch).is_empty());
    assert!(ops(&r.run(Action::Module(agent()))).is_empty());
    // Its own failure frees the name, says so, and the key starts it again.
    let batch = r.controller.handle(HostEvent::ModuleWindowFailed {
        win: WIN,
        name: WindowName::Chat(1),
        why: "its handshake did not finish in time".into(),
    });
    assert!(ops(&batch).is_empty());
    assert_eq!(
        r.said().toast.map(|t| t.1),
        Some("window c1 did not start: its handshake did not finish in time".into())
    );
    assert_eq!(
        ops(&r.run(Action::Module(agent()))),
        vec![open(WindowName::Chat(1), None)]
    );
}

#[test]
fn a_chat_key_waiting_for_a_window_that_never_links_is_dropped_and_asks_again() {
    let mut r = hub();
    r.run(Action::Tab(TabAction::Next));
    r.controller.handle(HostEvent::ModuleWindowFailed {
        win: WIN,
        name: WindowName::Chat(1),
        why: "its link ended".into(),
    });
    assert!(
        r.controller
            .notes(WIN)
            .iter()
            .any(|n| n.contains("c1 did not start") && n.contains("1 waiting action(s) dropped")),
        "{:?}",
        r.controller.notes(WIN)
    );
    // The next chat key starts c1 again and waits for it afresh; the dropped action does not run when it links.
    let batch = r.run(Action::Tab(TabAction::Prev));
    assert_eq!(ops(&batch), vec![open(WindowName::Chat(1), None)]);
    let linked = attach(&mut r, 20, WindowName::Chat(1));
    let ran: Vec<_> = ops(&linked)
        .into_iter()
        .filter_map(|op| match op {
            HostOp::PeerRun { name, .. } => Some(name),
            _ => None,
        })
        .collect();
    assert_eq!(ran, vec!["tab.prev".to_string()]);
}

#[test]
fn a_refused_compositor_verb_shows_its_notice_and_is_logged() {
    for what in [OpTag::CompositorFocus, OpTag::PeerRun, OpTag::HubAsk] {
        let mut r = hub();
        let batch = r.controller.handle(HostEvent::Refused {
            what: what.clone(),
            why: crate::compositor::NO_ADAPTER_NOTICE.into(),
            surface: None,
        });
        assert!(ops(&batch).is_empty(), "{what:?}: {:?}", ops(&batch));
        assert_eq!(
            r.said().toast.map(|t| t.1),
            Some(crate::compositor::NO_ADAPTER_NOTICE.into()),
            "{what:?}"
        );
        assert!(
            r.controller
                .notes(WIN)
                .iter()
                .any(|n| n.contains(&format!("{what:?} refused"))),
            "{what:?}: {:?}",
            r.controller.notes(WIN)
        );
    }
}

#[test]
fn the_editor_edge_goes_to_the_compositor_in_hub_mode_and_not_in_eitri_mode() {
    let at_the_left = |tiling| {
        let mut r = rig_in(tiling, editor_and_terminal());
        let batch = r.run(Action::Select(Direction::Left));
        (ops(&batch), r.controller.notes(WIN))
    };
    let (hub_ops, _) = at_the_left(Tiling::Hub);
    assert_eq!(
        hub_ops,
        vec![HostOp::CompositorFocus {
            win: WIN,
            direction: Direction::Left
        }]
    );
    let (eitri_ops, notes) = at_the_left(Tiling::Eitri);
    assert!(eitri_ops.is_empty(), "{eitri_ops:?}");
    assert!(notes.iter().any(|n| n.contains("no module that way")), "{notes:?}");
}

#[test]
fn up_at_the_top_goes_to_the_compositor_in_hub_mode() {
    let up = |tiling| {
        let mut r = rig_in(tiling, editor_and_terminal());
        ops(&r.run(Action::Select(Direction::Up)))
    };
    assert_eq!(
        up(Tiling::Hub),
        vec![HostOp::CompositorFocus {
            win: WIN,
            direction: Direction::Up
        }]
    );
    assert!(
        up(Tiling::Eitri).iter().any(|op| matches!(
            op,
            HostOp::Focus {
                to: FocusTo::TopBarItem(_),
                ..
            }
        )),
        "eitri mode still reaches the top bar"
    );
}

#[test]
fn a_module_windows_chat_key_asks_window_1() {
    let mut r = module_window(WindowName::Chat(2));
    let ask = |ask| vec![HostOp::HubAsk { win: WIN, ask }];
    assert_eq!(
        ops(&r.run(Action::Module(ModuleId::parse("agent#3").unwrap()))),
        ask(HubAsk::Module { id: "agent#3".into() })
    );
    assert_eq!(
        ops(&r.run(Action::Module(ModuleId::terminal()))),
        ask(HubAsk::Module { id: "terminal".into() })
    );
    assert_eq!(
        ops(&r.run(Action::InstanceNew {
            kind: ModuleKind::Agent
        })),
        ask(HubAsk::InstanceNew)
    );
    assert_eq!(
        ops(&r.run(Action::Select(Direction::Right))),
        vec![HostOp::CompositorFocus {
            win: WIN,
            direction: Direction::Right
        }]
    );
    let before = ops(&r.run(Action::SelectLast));
    assert!(before.is_empty(), "a one-module window has nothing to select");
    assert_eq!(said(&r, "[keys] select.last: nothing to act on in this window"), 1);
}

#[test]
fn a_tab_break_in_a_module_window_names_the_session_and_the_direction() {
    let facts = |session: Option<&str>| ChatMoveFacts {
        tab: TabId(1),
        session: session.map(str::to_owned),
        live: true,
        running: false,
    };
    for (name, back) in [(WindowName::Chat(1), false), (WindowName::Chat(3), true)] {
        let mut r = module_window(name);
        r.host.set_base_tabs(vec![facts(Some("sess-1"))], Some(TabId(1)));
        assert_eq!(
            ops(&r.run(Action::TabBreak)),
            vec![HostOp::HubAsk {
                win: WIN,
                ask: HubAsk::TabBreak {
                    agent: AgentId::Claude,
                    session: "sess-1".into(),
                    back
                }
            }]
        );
    }
}

#[test]
fn a_tab_break_on_an_empty_tab_asks_for_a_new_chat_window() {
    for name in [WindowName::Chat(1), WindowName::Chat(3)] {
        let mut r = module_window(name);
        let tab = ChatMoveFacts {
            tab: TabId(1),
            session: None,
            live: true,
            running: false,
        };
        r.host.set_base_tabs(vec![tab], Some(TabId(1)));
        assert_eq!(
            ops(&r.run(Action::TabBreak)),
            vec![HostOp::HubAsk {
                win: WIN,
                ask: HubAsk::InstanceNew,
            }]
        );
    }
}

/// `tab.break` as window 1 sends it to the chat window that last held the keys.
fn run_from_hub(r: &mut Rig) -> Batch {
    r.controller.run(WIN, Action::TabBreak, Origin::Hub)
}

#[test]
fn a_tab_break_window_1_sends_asks_for_the_hand_over_as_the_key_does() {
    for (name, back) in [(WindowName::Chat(1), false), (WindowName::Chat(3), true)] {
        let mut r = module_window(name);
        r.host
            .set_base_tabs(vec![tab_facts(Some("sess-1"), false)], Some(TabId(1)));
        assert_eq!(
            ops(&run_from_hub(&mut r)),
            vec![HostOp::HubAsk {
                win: WIN,
                ask: HubAsk::TabBreak {
                    agent: AgentId::Claude,
                    session: "sess-1".into(),
                    back
                }
            }]
        );
    }
}

#[test]
fn a_tab_break_names_the_tabs_agent_as_well_as_its_session() {
    let mut r = module_window(WindowName::Chat(3));
    r.host.set_base_tabs(vec![tab_facts(Some("s1"), false)], Some(TabId(1)));
    r.host.set_tab_agent(AgentId::Acp("two".into()));
    assert_eq!(
        ops(&r.run(Action::TabBreak)),
        vec![HostOp::HubAsk {
            win: WIN,
            ask: HubAsk::TabBreak {
                agent: AgentId::Acp("two".into()),
                session: "s1".into(),
                back: true
            }
        }]
    );
    let mut r = hub();
    attach(&mut r, 16, WindowName::Chat(3));
    let ask = HubAsk::TabBreak {
        agent: AgentId::Acp("two".into()),
        session: "s1".into(),
        back: true,
    };
    let peer = PeerId(16);
    assert_eq!(
        ops(&peer_event(&mut r, PeerEvent::HubAsk { peer, ask, token: None })),
        vec![HostOp::HandOverTab {
            win: WIN,
            from: peer,
            agent: AgentId::Acp("two".into()),
            session: "s1".into(),
            to: WindowName::Chat(1),
            token: None,
        }]
    );
}

#[test]
fn a_tab_break_window_1_sends_waits_for_the_turn_and_a_second_calls_it_off() {
    let mut r = module_window(WindowName::Chat(1));
    r.host
        .set_base_tabs(vec![tab_facts(Some("sess-1"), true)], Some(TabId(1)));
    assert!(ops(&run_from_hub(&mut r)).is_empty(), "the turn runs");
    assert_eq!(toast(&r).as_deref(), Some(feedback::TAB_BREAK_WAITS_TEXT));
    assert!(ops(&run_from_hub(&mut r)).is_empty());
    assert_eq!(toast(&r).as_deref(), Some(feedback::TAB_BREAK_CANCELLED_TEXT));

    assert!(ops(&run_from_hub(&mut r)).is_empty(), "it waits again");
    r.host
        .set_base_tabs(vec![tab_facts(Some("sess-1"), false)], Some(TabId(1)));
    assert!(
        ops(&busy(&mut r, false))
            .iter()
            .any(|op| matches!(op, HostOp::HubAsk { .. })),
        "its turn ended"
    );
}

#[test]
fn window_1_answers_what_a_module_window_asks() {
    let mut r = hub();
    attach(&mut r, 16, WindowName::Chat(2));
    let ask = |r: &mut Rig, ask, token: Option<&str>| {
        peer_event(
            r,
            PeerEvent::HubAsk {
                peer: PeerId(16),
                ask,
                token: token.map(str::to_owned),
            },
        )
    };
    let chat = |n: u8| HubAsk::Module {
        id: format!("agent#{n}"),
    };
    // A window that exists comes forward with the asker's token; one that does not is started with it.
    assert_eq!(
        ops(&ask(&mut r, chat(2), Some("t1"))),
        vec![HostOp::ForwardRaise {
            win: WIN,
            peer: PeerId(16),
            token: Some("t1".into())
        }]
    );
    assert_eq!(
        ops(&ask(&mut r, chat(4), Some("t2"))),
        vec![open(WindowName::Chat(4), Some("t2"))]
    );
    assert_eq!(
        ops(&ask(&mut r, HubAsk::InstanceNew, Some("t3"))),
        vec![open(WindowName::Chat(3), Some("t3"))]
    );
    // A terminal is window 1's own: it takes the keys and its window comes forward.
    let batch = ask(&mut r, HubAsk::Module { id: "terminal".into() }, Some("t4"));
    let all = ops(&batch);
    assert!(all.iter().any(|op| matches!(op, HostOp::Focus { .. })), "{all:?}");
    assert_eq!(
        all.last(),
        Some(&HostOp::Present {
            win: WIN,
            activation: Some(ActivationToken("t4".into()))
        })
    );
}

fn toast(r: &Rig) -> Option<String> {
    r.said().toast.map(|toast| toast.1)
}

fn busy(r: &mut Rig, busy: bool) -> Batch {
    r.controller.handle(HostEvent::Chat(ChatEvent::Busy {
        win: WIN,
        view: ViewId::BASE,
        busy,
    }))
}

fn tab_facts(session: Option<&str>, running: bool) -> ChatMoveFacts {
    ChatMoveFacts {
        tab: TabId(1),
        session: session.map(str::to_owned),
        live: true,
        running,
    }
}

#[test]
fn a_hub_hands_a_tab_to_the_next_free_chat_window_or_back_to_c1() {
    let mut r = hub();
    attach(&mut r, 16, WindowName::Chat(1));
    attach(&mut r, 17, WindowName::Chat(2));
    let source = PeerId(16);
    let tab_break = |r: &mut Rig, back: bool, token: Option<&str>| {
        peer_event(
            r,
            PeerEvent::HubAsk {
                peer: source,
                ask: HubAsk::TabBreak {
                    agent: AgentId::Claude,
                    session: "s".into(),
                    back,
                },
                token: token.map(str::to_owned),
            },
        )
    };
    assert_eq!(
        ops(&tab_break(&mut r, false, Some("t"))),
        vec![HostOp::HandOverTab {
            win: WIN,
            from: source,
            agent: AgentId::Claude,
            session: "s".into(),
            to: WindowName::Chat(3),
            token: Some("t".into()),
        }],
        "c2 is taken"
    );
    assert_eq!(
        ops(&tab_break(&mut r, true, None)),
        vec![HostOp::HandOverTab {
            win: WIN,
            from: source,
            agent: AgentId::Claude,
            session: "s".into(),
            to: WindowName::Chat(1),
            token: None,
        }]
    );
    // The hand-over starts its window: a key pressed meanwhile counts nothing as starting that is not, and a refused
    // hand-over leaves no mark. The next ask names the same free window.
    assert!(matches!(
        ops(&tab_break(&mut r, false, None))[0],
        HostOp::HandOverTab {
            to: WindowName::Chat(3),
            ..
        }
    ));
    for (peer, n) in (18..).zip(3..=9) {
        attach(&mut r, peer, WindowName::Chat(n));
    }
    assert_eq!(
        ops(&tab_break(&mut r, false, None)),
        vec![HostOp::KeepTab {
            win: WIN,
            peer: source,
            session: "s".into(),
            why: "all nine chat windows are open".into(),
        }],
        "the window that asked is told, on its own screen"
    );
}

#[test]
fn a_tab_break_during_a_running_turn_waits_for_the_turns_end() {
    let mut r = module_window(WindowName::Chat(3));
    r.host
        .set_base_tabs(vec![tab_facts(Some("sess-1"), true)], Some(TabId(1)));
    assert!(ops(&r.run(Action::TabBreak)).is_empty(), "nothing is asked yet");
    assert_eq!(
        toast(&r).as_deref(),
        Some("the tab moves to its own window when this turn ends")
    );
    assert!(ops(&busy(&mut r, true)).is_empty(), "its turn still runs");

    // Its turn ends while the window's other tab still runs: the break goes without waiting for that one.
    r.host
        .set_base_tabs(vec![tab_facts(Some("sess-1"), false)], Some(TabId(1)));
    assert_eq!(
        ops(&busy(&mut r, true)),
        vec![HostOp::HubAsk {
            win: WIN,
            ask: HubAsk::TabBreak {
                agent: AgentId::Claude,
                session: "sess-1".into(),
                back: true
            }
        }]
    );
    assert!(ops(&busy(&mut r, false)).is_empty(), "it asked once");
}

#[test]
fn a_first_turn_with_no_session_yet_waits_and_names_it_at_the_end() {
    let mut r = module_window(WindowName::Chat(1));
    r.host.set_base_tabs(vec![tab_facts(None, true)], Some(TabId(1)));
    assert!(ops(&r.run(Action::TabBreak)).is_empty());
    r.host
        .set_base_tabs(vec![tab_facts(Some("sess-1"), false)], Some(TabId(1)));
    assert_eq!(
        ops(&busy(&mut r, false)),
        vec![HostOp::HubAsk {
            win: WIN,
            ask: HubAsk::TabBreak {
                agent: AgentId::Claude,
                session: "sess-1".into(),
                back: false
            }
        }]
    );
}

#[test]
fn a_second_tab_break_calls_off_the_wait() {
    let mut r = module_window(WindowName::Chat(1));
    r.host
        .set_base_tabs(vec![tab_facts(Some("sess-1"), true)], Some(TabId(1)));
    r.run(Action::TabBreak);
    assert!(ops(&r.run(Action::TabBreak)).is_empty());
    assert_eq!(toast(&r).as_deref(), Some("the tab stays here"));
    r.host
        .set_base_tabs(vec![tab_facts(Some("sess-1"), false)], Some(TabId(1)));
    assert!(ops(&busy(&mut r, false)).is_empty(), "the turn's end asks nothing now");
}

#[test]
fn a_break_whose_tab_can_no_longer_go_says_why_and_stops_waiting() {
    let mut r = module_window(WindowName::Chat(2));
    r.host
        .set_base_tabs(vec![tab_facts(Some("sess-1"), true)], Some(TabId(1)));
    r.run(Action::TabBreak);
    r.host.set_tab_moves(TabId(1), Err("the tab is gone".into()));
    assert!(ops(&busy(&mut r, false)).is_empty());
    assert_eq!(toast(&r).as_deref(), Some("the tab is gone"));
    r.host
        .set_base_tabs(vec![tab_facts(Some("sess-1"), false)], Some(TabId(1)));
    assert!(ops(&busy(&mut r, false)).is_empty(), "the wait has ended");
}

#[test]
fn a_tab_that_cannot_go_is_refused_at_the_key_with_its_reason() {
    let mut r = module_window(WindowName::Chat(1));
    r.host
        .set_base_tabs(vec![tab_facts(Some("sess-1"), false)], Some(TabId(1)));
    r.host.set_tab_moves(TabId(1), Err("it is still starting".into()));
    let before = r.said();
    let batch = r.run(Action::TabBreak);
    r.assert_refused(&batch, &before, "it is still starting");
}

#[test]
fn a_kept_tab_is_said_in_the_window_that_offered_it() {
    let mut r = module_window(WindowName::Chat(2));
    r.controller.handle(HostEvent::Chat(ChatEvent::TabKept {
        win: WIN,
        why: "a tab is already moving".into(),
    }));
    assert_eq!(toast(&r).as_deref(), Some("a tab is already moving"));
}

#[test]
fn an_emptied_extra_chat_window_closes_and_c1_stays() {
    const CLOSES: &str = "[window] its last tab has left; this chat window closes";
    let left = |r: &mut Rig| r.controller.handle(HostEvent::Chat(ChatEvent::TabLeft { win: WIN }));
    let mut r = module_window(WindowName::Chat(3));
    r.host.set_base_tabs(vec![tab_facts(None, false)], Some(TabId(1)));
    left(&mut r);
    assert_eq!(said(&r, CLOSES), 1, "c3 holds nothing now");
    let (idle, _) = r.scheduler.armed()[0];
    let closes = r.controller.handle(HostEvent::Timer(idle));
    assert!(
        ops(&closes).contains(&HostOp::CloseWindow { win: WIN }),
        "it asks nothing and closes: {:?}",
        ops(&closes)
    );

    let mut r = module_window(WindowName::Chat(1));
    r.host.set_base_tabs(vec![tab_facts(None, false)], Some(TabId(1)));
    left(&mut r);
    assert_eq!(said(&r, CLOSES), 0, "c1 keeps its empty view");

    let mut r = module_window(WindowName::Chat(3));
    r.host
        .set_base_tabs(vec![tab_facts(Some("sess-2"), false)], Some(TabId(1)));
    left(&mut r);
    assert_eq!(said(&r, CLOSES), 0, "it still holds another tab");
}

#[test]
fn a_hub_ask_is_heard_only_by_window_1_of_a_compositor_group() {
    let mut r = rig_in(Tiling::Eitri, editor_and_terminal());
    attach(&mut r, 2, WindowName::Second);
    let batch = peer_event(
        &mut r,
        PeerEvent::HubAsk {
            peer: PeerId(2),
            ask: HubAsk::InstanceNew,
            token: None,
        },
    );
    assert!(ops(&batch).is_empty());
}

#[test]
fn a_lua_split_of_a_chat_in_a_hub_opens_its_window_and_logs() {
    let mut r = hub();
    let batch = r.controller.lua_request(WIN, LayoutRequest::Split(agent(), Axis::Row));
    assert_eq!(ops(&batch), vec![open(WindowName::Chat(1), None)]);
    assert_eq!(said(&r, "[layout] split agent: placement is the compositor's"), 1);
    let batch = r
        .controller
        .lua_request(WIN, LayoutRequest::Float(ModuleId::lua("notes"), None));
    assert_eq!(ops(&batch), vec![open(WindowName::Page("notes".into()), None)]);
    let batch = r
        .controller
        .lua_request(WIN, LayoutRequest::Open(ModuleKind::Agent, None));
    assert_eq!(ops(&batch), vec![open(WindowName::Chat(2), None)]);
    assert_eq!(said(&r, "[layout] open agent: placement is the compositor's"), 1);
    // The editor and the terminal are placed as ever.
    let batch = r
        .controller
        .lua_request(WIN, LayoutRequest::Focus(ModuleId::terminal()));
    assert!(!ops(&batch).iter().any(is_new_op));
}

#[test]
fn a_hide_of_a_chat_in_a_hub_only_logs() {
    let mut r = hub();
    let batch = r.controller.lua_request(WIN, LayoutRequest::Hide(agent()));
    assert!(ops(&batch).is_empty());
    assert_eq!(said(&r, "[layout] hide agent: the window is the compositor's"), 1);
}

#[test]
fn an_eitri_window_never_reaches_the_module_window_verbs() {
    for action in one_of_each() {
        let mut r = rig_in(Tiling::Eitri, three(ModuleId::terminal()));
        attach(&mut r, 2, WindowName::Second);
        let batch = r.run(action.clone());
        assert!(!ops(&batch).iter().any(is_new_op), "{action:?}: {:?}", ops(&batch));
        let notes = r.controller.notes(WIN);
        assert!(
            !notes.iter().any(|n| n.contains("compositor")),
            "{action:?} met a compositor verb: {notes:?}"
        );
        assert!(
            !r.controller
                .chrome(WIN)
                .toast
                .is_some_and(|toast| toast.text.contains("arranges windows here")),
            "{action:?}"
        );
    }
}

/// Window 1 still names the module that held its keys when it lost the focus, so a terminal key from another window
/// must not read that as a press on the terminal: it brings window 1 forward with the terminal on screen, whichever
/// module had the keys there.
#[test]
fn a_terminal_key_from_another_window_never_hides_the_terminal() {
    let mut r = hub();
    attach(&mut r, 16, WindowName::Chat(1));
    let terminal = ModuleId::terminal();
    r.layout.borrow_mut().set_focus(&terminal).unwrap();
    r.controller.handle(HostEvent::NativeFocus {
        win: WIN,
        owner: Some(FocusOwner::Module(terminal.clone())),
        chain: FocusChain(vec![terminal.clone()]),
        active: true,
        op_index: None,
    });
    r.controller.handle(HostEvent::Active {
        win: WIN,
        active: false,
    });
    let batch = peer_event(
        &mut r,
        PeerEvent::HubAsk {
            peer: PeerId(16),
            ask: HubAsk::Module { id: "terminal".into() },
            token: Some("t1".into()),
        },
    );
    assert!(r.layout.borrow().is_shown(&terminal), "the key has nothing to hide");
    assert_eq!(
        ops(&batch).last(),
        Some(&HostOp::Present {
            win: WIN,
            activation: Some(ActivationToken("t1".into()))
        })
    );

    // Hidden, it is shown and given the keys, as its key does in window 1.
    r.run(Action::ModuleHide);
    assert!(!r.layout.borrow().is_shown(&terminal));
    peer_event(
        &mut r,
        PeerEvent::HubAsk {
            peer: PeerId(16),
            ask: HubAsk::Module { id: "terminal".into() },
            token: None,
        },
    );
    assert!(r.layout.borrow().is_shown(&terminal));
}

/// `:Eitri workspace`, `open terminal` and `split` of the terminal, and what `eitri.layout` calls the same, stand for
/// keys that are off in window 1. They change nothing and say so as those keys do, so the window keeps its one shape.
#[test]
fn a_request_for_what_is_off_in_a_hub_changes_nothing_and_hints_once() {
    let requests = [
        LayoutRequest::Switch("study".into()),
        LayoutRequest::Open(ModuleKind::Terminal, None),
        LayoutRequest::Split(ModuleId::terminal(), Axis::Row),
        LayoutRequest::Float(ModuleId::terminal(), None),
        LayoutRequest::Mark(ModuleId::terminal()),
    ];
    let mut r = hub();
    let shape = format!("{:?}", r.layout.borrow());
    for request in requests {
        let batch = r.controller.lua_request(WIN, request);
        assert!(ops(&batch).is_empty(), "{:?}", ops(&batch));
        assert_eq!(format!("{:?}", r.layout.borrow()), shape);
    }
    assert_eq!(toast(&r).as_deref(), Some(off_hint("niri").as_str()));
    for line in [
        "workspace",
        "instance.new",
        "split.right",
        "float.toggle",
        "mark.toggle",
    ] {
        assert_eq!(said(&r, &format!("[keys] {line}: off in compositor mode")), 1, "{line}");
    }
}

/// The page a beside window shows in these tests.
fn own_page() -> ModuleId {
    ModuleId::lua("notes")
}

/// A page window beside the person's own nvim: a module window of no group, showing `notes`.
fn beside() -> Rig {
    let page = own_page();
    let mut r = rig_in(
        Tiling::ModuleWindow,
        Layout::new(Node::Leaf(page.clone()), page.clone()).unwrap(),
    );
    r.controller.window_mut(WIN).unwrap().spec.role = WindowRole::Companion;
    r.controller
        .set_module_name(WIN, WindowName::for_module(&page).unwrap());
    r.controller.set_module_kind(WIN, ModuleWindowKind::Beside);
    r
}

/// Each variant of `Action` by its own number. No catch-all: a new action does not compile until it is numbered
/// here, and [`every_action`]'s count then fails until it has a sample.
fn action_number(action: &Action) -> usize {
    match action {
        Action::Move(_) => 0,
        Action::ContainerTabbed => 1,
        Action::ContainerStacked => 2,
        Action::FocusParent => 3,
        Action::FocusChild => 4,
        Action::InstanceNew { .. } => 5,
        Action::FloatToggle => 6,
        Action::FocusModeToggle => 7,
        Action::ScratchpadShow { .. } => 8,
        Action::ScratchpadMove => 9,
        Action::WorkspaceStep { .. } => 10,
        Action::WorkspaceLast => 11,
        Action::WorkspaceChoose => 12,
        Action::ProjectChoose => 13,
        Action::WorkspaceRename => 14,
        Action::WorkspaceSwitch { .. } => 15,
        Action::WorkspaceNew => 16,
        Action::TabBreak => 17,
        Action::WindowNew => 18,
        Action::WindowOther => 19,
        Action::MoveToWindow { .. } => 20,
        Action::WorkspaceToWindow { .. } => 21,
        Action::WorkspaceSwapWindow => 22,
        Action::MoveFar(_) => 23,
        Action::ToggleSplit => 24,
        Action::Rotate { .. } => 25,
        Action::Flip(_) => 26,
        Action::MainHorizontal => 27,
        Action::MainVertical => 28,
        Action::Tiled => 29,
        Action::CycleLayout { .. } => 30,
        Action::Spread => 31,
        Action::LayoutUndo => 32,
        Action::MarkToggle => 33,
        Action::MarkClear => 34,
        Action::SwapMarked => 35,
        Action::SelectNumber => 36,
        Action::SendPrefix => 37,
        Action::SendKeys(_) => 38,
        Action::Split { .. } => 39,
        Action::Zoom => 40,
        Action::Resize { .. } => 41,
        Action::Select(_) => 42,
        Action::SelectLast => 43,
        Action::SelectNext => 44,
        Action::Swap(_) => 45,
        Action::Even(_) => 46,
        Action::ModuleHide => 47,
        Action::ModuleHideByKey => 48,
        Action::ModuleKill => 49,
        Action::Module(_) => 50,
        Action::Tab(_) => 51,
        Action::Hint => 52,
        Action::PanelReload => 53,
        Action::PanelKeymap => 54,
        Action::PanelCommandLine => 55,
        Action::WindowImmersive => 56,
        Action::WindowSetup => 57,
        Action::Text(_) => 58,
        Action::TextAll(_) => 59,
        Action::CopyMode { .. } => 60,
        Action::AgentsNext => 61,
        Action::AgentsBack => 62,
        Action::PluginLoad { .. } => 63,
    }
}

const ACTION_VARIANTS: usize = 64;

/// One action of every variant, and every kind of module key and `instance.new`, which a beside window tells apart.
fn every_action() -> Vec<Action> {
    let mut all = one_of_each();
    all.extend([
        Action::TextAll(crate::keymap::action::TextChange::Larger),
        Action::Tab(TabAction::New),
        Action::Select(Direction::Right),
        Action::InstanceNew {
            kind: ModuleKind::Agent,
        },
        Action::Module(ModuleId::editor()),
        Action::Module(ModuleId::terminal()),
        Action::Module(ModuleId::parse("canvas").unwrap()),
        Action::Module(ModuleId::parse("agent#2").unwrap()),
        Action::Module(own_page()),
        Action::Module(ModuleId::lua("sketch")),
    ]);
    let numbers: std::collections::BTreeSet<usize> = all.iter().map(action_number).collect();
    assert_eq!(numbers.len(), ACTION_VARIANTS, "a sample of every action");
    all
}

/// An op that reaches another window of a group or starts one: a beside window has neither.
fn reaches_a_group(op: &HostOp) -> bool {
    matches!(
        op,
        HostOp::HubAsk { .. }
            | HostOp::PeerRun { .. }
            | HostOp::RaisePeer { .. }
            | HostOp::ForwardRaise { .. }
            | HostOp::HandOverTab { .. }
            | HostOp::KeepTab { .. }
            | HostOp::OpenModuleWindow { .. }
            | HostOp::OpenLinkedWindow { .. }
            | HostOp::RestoreLinkedWindow { .. }
            | HostOp::StartEditorWindow { .. }
    )
}

#[test]
fn a_beside_window_emits_no_hub_ask_peer_run_raise_peer_or_window_start() {
    for action in every_action() {
        let mut r = beside();
        let all = ops(&r.run(action.clone()));
        assert!(!all.iter().any(reaches_a_group), "{action:?}: {all:?}");
        let notes = r.controller.notes(WIN);
        assert!(!notes.iter().any(|n| n.contains("BUG")), "{action:?}: {notes:?}");
    }
}

#[test]
fn beside_agent_editor_and_page_keys_map_to_beside_ops() {
    let beside_ops = |action: Action| {
        let mut r = beside();
        ops(&r.run(action))
    };
    assert_eq!(
        beside_ops(Action::Module(agent())),
        vec![HostOp::Beside(BesideOp::Agent)]
    );
    assert_eq!(
        beside_ops(Action::Module(ModuleId::parse("agent#2").unwrap())),
        vec![HostOp::Beside(BesideOp::Agent)],
        "one agent window answers for every chat key"
    );
    assert_eq!(
        beside_ops(Action::Module(ModuleId::editor())),
        vec![HostOp::Beside(BesideOp::RaiseEditor)]
    );
    assert_eq!(
        beside_ops(Action::Module(ModuleId::lua("sketch"))),
        vec![HostOp::Beside(BesideOp::Page("sketch".into()))]
    );
    let mut r = beside();
    assert!(
        ops(&r.run(Action::Module(own_page()))).is_empty(),
        "its own page is here"
    );
    assert_eq!(toast(&r), None);
    // Past the window's edge the compositor moves the focus, as in any module window.
    assert_eq!(
        beside_ops(Action::Select(Direction::Left)),
        vec![HostOp::CompositorFocus {
            win: WIN,
            direction: Direction::Left
        }]
    );
}

#[test]
fn beside_chat_keys_toast_once_then_log() {
    let mut r = beside();
    for action in [
        Action::Tab(TabAction::New),
        Action::PanelReload,
        Action::PanelCommandLine,
        Action::Tab(TabAction::Next),
    ] {
        assert!(ops(&r.run(action.clone())).is_empty(), "{action:?}");
    }
    let first = r.said().toast.expect("the first chat key toasts");
    assert_eq!(first.1, BESIDE_NO_CHAT_TEXT);
    let serial = first.0;
    r.run(Action::PanelReload);
    assert_eq!(r.said().toast.map(|t| t.0), Some(serial), "toasted once in the window");
    assert_eq!(said(&r, "[keys] panel.reload: no chat in a window beside nvim"), 2);
    // Its own text: an off key after it still says why once.
    r.run(Action::Zoom);
    assert_eq!(toast(&r).as_deref(), Some(BESIDE_OFF_TEXT));
}

/// `<prefix> ?` in a beside window: the keys sheet lives in a chat's page, which this window has none of, and the
/// host has no call that opens a page's own sheet, so every press says where the keys are listed instead: the
/// page's own `?`. Never the no-chat text, which would send the person to the agent for the page's keys.
#[test]
fn prefix_question_mark_reaches_the_pages_keys_sheet_or_toasts() {
    use crate::wm::feedback::BESIDE_PAGE_KEYS_TEXT;
    assert_eq!(BESIDE_PAGE_KEYS_TEXT, "the page's own ? lists its keys");
    let mut r = beside();
    assert!(
        ops(&r.run(Action::PanelKeymap)).is_empty(),
        "nothing to open, nothing to ask"
    );
    let first = r.said().toast.expect("the press is answered");
    assert_eq!(first.1, BESIDE_PAGE_KEYS_TEXT);
    r.run(Action::PanelKeymap);
    let second = r.said().toast.expect("and again");
    assert_eq!(second.1, BESIDE_PAGE_KEYS_TEXT);
    assert_ne!(second.0, first.0, "each press is answered, as a question is");
    assert_eq!(said(&r, "[keys] panel.keymap: the page lists its own keys on its ?"), 2);
    // A chat key after it still says there is no chat, once.
    r.run(Action::PanelReload);
    assert_eq!(toast(&r).as_deref(), Some(BESIDE_NO_CHAT_TEXT));
}

#[test]
fn beside_off_keys_toast_once_then_log() {
    let mut r = beside();
    let off = [
        Action::Zoom,
        Action::WindowNew,
        Action::WindowOther,
        Action::MoveToWindow {
            module: None,
            follow: false,
        },
        Action::WorkspaceNew,
        Action::FloatToggle,
        Action::ProjectChoose,
        Action::InstanceNew {
            kind: ModuleKind::Agent,
        },
        Action::InstanceNew {
            kind: ModuleKind::Terminal,
        },
        Action::TabBreak,
        Action::Module(ModuleId::terminal()),
    ];
    let mut pressed = std::collections::BTreeMap::<String, usize>::new();
    for action in &off {
        assert!(ops(&r.run(action.clone())).is_empty(), "{action:?}");
        let times = pressed.entry(action.name()).or_default();
        *times += 1;
        assert_eq!(
            said(&r, &format!("[keys] {}: off in a window beside nvim", action.name())),
            *times,
            "{action:?} is logged"
        );
    }
    let first = r.said().toast.expect("the first off key toasts");
    assert_eq!(first.1, BESIDE_OFF_TEXT);
    r.run(Action::Zoom);
    assert_eq!(r.said().toast, Some(first), "toasted once in the window");
    assert_eq!(said(&r, "[keys] zoom: off in a window beside nvim"), 2);
    assert_ne!(
        BESIDE_OFF_TEXT,
        off_hint("niri"),
        "not the hub's text: no Eitri layout comes back here"
    );
}

/// The host's refusal to reach the editor's window inside tmux is said once, then logged; any other refusal of a
/// beside op is shown each time.
#[test]
fn a_refused_raise_of_the_editor_inside_tmux_toasts_once_then_logs() {
    let mut r = beside();
    let refused = |why: &str| HostEvent::Refused {
        what: OpTag::Beside,
        why: why.into(),
        surface: None,
    };
    r.controller.handle(refused(BESIDE_TMUX_TEXT));
    let first = r.said();
    assert_eq!(first.toast.as_ref().map(|t| t.1.as_str()), Some(BESIDE_TMUX_TEXT));
    r.controller.handle(refused(BESIDE_TMUX_TEXT));
    assert_eq!(r.said(), first, "nothing more on screen");
    assert_eq!(said(&r, &format!("[window] beside: {BESIDE_TMUX_TEXT}")), 2);
    r.controller.handle(refused("eitri could not be started"));
    r.controller.handle(refused("eitri could not be started"));
    assert_eq!(r.said().flash, first.flash + 2, "a refusal flashes each time");

    // A window of a group never hears it.
    let mut r = module_window(WindowName::Chat(2));
    let before = r.said();
    r.controller.handle(refused("eitri could not be started"));
    assert_eq!(r.said(), before);
}

#[test]
fn in_group_and_hub_follow_results_are_unchanged() {
    let page = ModuleId::lua("notes");
    let name = WindowName::for_module(&page).unwrap();
    let in_group = || {
        let mut r = rig_in(
            Tiling::ModuleWindow,
            Layout::new(Node::Leaf(page.clone()), page.clone()).unwrap(),
        );
        r.controller.set_module_name(WIN, name.clone());
        r
    };
    let eitri = || {
        let r = rig_in(Tiling::Eitri, three(ModuleId::terminal()));
        r.host
            .with_surface(WIN, page.clone(), crate::wm::verbs::rig::decl(&page).kind, false);
        r
    };
    let rigs: [(&str, &dyn Fn() -> Rig); 3] = [("hub", &hub), ("in a group", &in_group), ("eitri", &eitri)];
    for (what, make) in rigs {
        for show in [true, false, true] {
            let (mut by_bool, mut by_cmd) = (make(), make());
            let cmd = if show { FollowCmd::Show } else { FollowCmd::Hide };
            let a = ops(&by_bool.controller.follow(WIN, &page, show));
            let b = ops(&by_cmd.controller.follow_cmd(WIN, &page, cmd));
            assert_eq!(a, b, "{what}: {cmd:?}");
            assert_eq!(
                by_bool.controller.notes(WIN),
                by_cmd.controller.notes(WIN),
                "{what}: {cmd:?}"
            );
            assert!(!a.iter().any(|op| matches!(op, HostOp::Beside(_))), "{what}");
        }
        // The shell sorts these for a window of a group: one that arrives is dropped, never read as a show.
        for cmd in [FollowCmd::Toggle, FollowCmd::Auto] {
            let mut r = make();
            assert!(
                ops(&r.controller.follow_cmd(WIN, &page, cmd)).is_empty(),
                "{what}: {cmd:?}"
            );
        }
    }
    // Window 1 still opens the page's window on a show, and only then.
    let mut r = hub();
    assert_eq!(
        ops(&r.controller.follow_cmd(WIN, &page, FollowCmd::Show)),
        vec![HostOp::OpenModuleWindow {
            win: WIN,
            name,
            token: None,
            keep_keys: true,
        }]
    );
}

#[test]
fn another_processs_new_starts_the_next_chat_window_with_its_token_in_window_1() {
    let mut r = hub();
    attach(&mut r, 16, WindowName::Chat(1));
    let batch = r.controller.agents_new(WIN, Some("tok".into()));
    assert_eq!(
        ops(&batch).into_iter().filter(is_new_op).collect::<Vec<_>>(),
        vec![open(WindowName::Chat(2), Some("tok"))],
        "the asking window's token goes with the start"
    );

    // A window holding chats opens an empty tab of its own: nothing is started or asked of window 1.
    let mut r = module_window(WindowName::Chat(1));
    let batch = r.controller.agents_new(WIN, Some("tok".into()));
    assert!(ops(&batch).iter().all(|op| !is_new_op(op)), "{:?}", ops(&batch));
}

/// A controller holding editor | terminal, the keys on the terminal, with The pass declared start-hidden, its own
/// rule `above` at 28 px without the keys, and its page deferred until its first show.
fn with_the_pass() -> (Rig, ModuleId) {
    let mut layout = editor_and_terminal();
    layout.set_focus(&ModuleId::terminal()).unwrap();
    with_the_pass_in(layout)
}

/// [`with_the_pass`] over `layout`, its keys where `layout` has them.
fn with_the_pass_in(layout: Layout) -> (Rig, ModuleId) {
    use crate::layout::rules::{At, OpenRule, RuleSize};
    use crate::layout::Workspaces;
    use crate::wm::module::SurfaceDecl;
    use std::cell::RefCell;
    let pass = ModuleId::lua("agents-pass");
    let host = crate::wm::fake::FakeHost::new();
    let scheduler = crate::wm::fake::FakeScheduler::new();
    let mut controller = Controller::new(scheduler.clone());
    controller.set_reads(host.clone());
    host.set_grid_size(crate::layout::Size { w: 1200, h: 800 });
    let mut hosted: Vec<SurfaceDecl> = layout.modules().iter().map(decl).collect();
    hosted.push(SurfaceDecl {
        module: pass.clone(),
        kind: SurfaceKind::PluginPage {
            plugin: "agents-pass".into(),
        },
        deferred: true,
    });
    let layout = Rc::new(RefCell::new(layout));
    controller.open_window(
        WIN,
        WindowSpec {
            role: WindowRole::Main,
            presence: EditorPresence::Owned,
            title: "project".into(),
        },
        WindowParts {
            layout: layout.clone(),
            workspaces: Rc::new(RefCell::new(Workspaces::single())),
            saver: None,
        },
    );
    controller.set_module_setup(
        WIN,
        crate::wm::verbs::modules::ModuleSetup {
            start_hidden: vec![(
                pass.clone(),
                Some(OpenRule {
                    at: Some(At::Beside(Direction::Up)),
                    size: Some(RuleSize::Pixels(28)),
                    focus: false,
                    ..OpenRule::default()
                }),
            )],
            ..Default::default()
        },
    );
    let first = controller.first_apply(WIN, &hosted);
    controller.window_mut(WIN).unwrap().tiling = Tiling::Eitri;
    (
        Rig {
            host,
            scheduler,
            controller,
            layout,
            first,
        },
        pass,
    )
}

/// Shown because an agent waits, a module that was never placed opens by its rule beside the editor, as if the editor
/// held the keys: The pass's `above` is a line above the editor's column, whichever module had the keys, so a
/// sidebar and the chat beside it never move; the keys stay where they were. The editor's follow command, which the
/// person's buffer asked for, still opens it beside the module with the keys.
#[test]
fn a_module_shown_while_agents_wait_opens_beside_the_editor_whoever_has_the_keys() {
    let (mut r, pass) = with_the_pass();
    r.controller.wait_follow(WIN, &pass, true);
    let layout = r.layout.borrow();
    assert!(layout.is_shown(&pass));
    assert_eq!(*layout.focus(), ModuleId::terminal(), "the keys stay");
    let root = layout.root();
    assert_eq!(root.layout, crate::layout::ConLayout::SplitH, "{root:?}");
    let Node::Con(column) = &root.children[0].node else {
        panic!("the editor's place holds a column: {root:?}")
    };
    assert_eq!(column.layout, crate::layout::ConLayout::SplitV);
    assert_eq!(column.children[0].node, Node::Leaf(pass.clone()));
    assert_eq!(column.children[0].fixed_px, Some(28));
    assert_eq!(column.children[1].node, Node::Leaf(ModuleId::editor()));
    assert_eq!(
        root.children[1].node,
        Node::Leaf(ModuleId::terminal()),
        "the terminal is untouched"
    );
    drop(layout);

    // Hidden when nothing waits, and shown again in that same place, the keys now on the editor.
    r.controller.wait_follow(WIN, &pass, false);
    assert!(!r.layout.borrow().is_shown(&pass));
    r.layout.borrow_mut().set_focus(&ModuleId::editor()).unwrap();
    r.controller.wait_follow(WIN, &pass, true);
    assert!(r.layout.borrow().is_shown(&pass));
    assert_eq!(
        r.layout.borrow().root().children[1].node,
        Node::Leaf(ModuleId::terminal())
    );

    let (mut by_editor, pass) = with_the_pass();
    by_editor.controller.follow(WIN, &pass, true);
    let layout = by_editor.layout.borrow();
    let Node::Con(column) = &layout.root().children[1].node else {
        panic!(
            "a follow command opens it above the terminal, which has the keys: {:?}",
            layout.root()
        )
    };
    assert_eq!(column.children[0].node, Node::Leaf(pass.clone()));
    assert_eq!(column.children[1].node, Node::Leaf(ModuleId::terminal()));
}

/// An editor on a tab behind the chat is not on screen: placed beside it, The pass would sit inside the hidden tab, and
/// no follow could draw it without hiding the chat that has the keys. It goes where the rule goes with no module
/// holding the keys instead: `above`, the window's top edge, on screen, the keys still on the chat.
#[test]
fn a_wait_show_does_not_anchor_on_an_editor_tabbed_away() {
    let mut layout = editor_and_terminal();
    crate::layout::place_as_tab(&mut layout, &agent(), &ModuleId::editor(), true).unwrap();
    layout.set_focus(&agent()).unwrap();
    assert!(
        layout.is_tabbed_away(&ModuleId::editor()),
        "the setup: the chat's tab is drawn"
    );
    let (mut r, pass) = with_the_pass_in(layout);
    r.controller.wait_follow(WIN, &pass, true);
    let layout = r.layout.borrow();
    assert!(layout.is_visible(&pass), "on screen: {:?}", layout.root());
    assert!(!layout.is_tabbed_away(&pass));
    assert_eq!(
        layout.root().children[0].node,
        Node::Leaf(pass.clone()),
        "the window's top edge"
    );
    assert_eq!(*layout.focus(), agent(), "the keys stay on the chat");
    assert!(!layout.is_tabbed_away(&agent()));
}
