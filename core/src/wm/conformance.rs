//! Reusable operation scripts for native hosts and their display-free doubles.

use super::{host::*, window::*};
use crate::layout::{ModuleId, ModuleKind};
use crate::tabs::ViewId;
use std::cell::{Cell, RefCell};

#[derive(Default)]
pub struct RecordingSink {
    applying: Cell<Option<usize>>,
    events: RefCell<Vec<(Option<usize>, HostEvent)>>,
    close_requests: Cell<usize>,
}

impl RecordingSink {
    pub fn applying(&self, i: Option<usize>) {
        self.applying.set(i);
    }
    pub fn events(&self) -> Vec<(Option<usize>, HostEvent)> {
        self.events.borrow().clone()
    }
    pub fn close_requests(&self) -> usize {
        self.close_requests.get()
    }
}

impl HostSink for RecordingSink {
    fn post(&self, event: HostEvent) {
        self.events.borrow_mut().push((self.applying.get(), event));
    }
    fn post_deferred(&self, event: HostEvent) {
        self.post(event);
    }
    fn close_requested(&self, _: WinId, _: CloseBy) -> CloseVerdict {
        self.close_requests.set(self.close_requests.get() + 1);
        CloseVerdict::Proceed
    }
}

pub struct Fixture {
    pub win: WinId,
    pub a: ModuleId,
    pub b: ModuleId,
    pub hidden: ModuleId,
}

pub struct Script {
    pub name: &'static str,
    pub ops: Vec<HostOp>,
    pub expect: Vec<Expect>,
}

#[derive(Debug)]
pub enum Expect {
    Answer { op: usize, applied: Applied },
    PostedDuring { op: usize, event: EventMatch },
    NotPosted(EventMatch),
    NoCloseRequest,
}

#[derive(Debug)]
pub enum EventMatch {
    NativeFocusOn(Option<ModuleId>),
    /// An unmap may clear focus or move it directly to another native destination.
    NativeFocusAwayFrom(ModuleId),
    Refused(OpTag),
    PageShown(ViewId),
}

impl EventMatch {
    fn matches(&self, event: &HostEvent) -> bool {
        match (self, event) {
            (Self::NativeFocusOn(expected), HostEvent::NativeFocus { owner, .. }) => match (expected, owner) {
                (Some(a), Some(FocusOwner::Module(b))) => a == b,
                (None, None) => true,
                _ => false,
            },
            (Self::NativeFocusAwayFrom(module), HostEvent::NativeFocus { owner, .. }) => {
                owner.as_ref() != Some(&FocusOwner::Module(module.clone()))
            }
            (Self::Refused(expected), HostEvent::Refused { what, .. }) => expected == what,
            (Self::PageShown(expected), HostEvent::Chat(ChatEvent::PageShown { view, .. })) => expected == view,
            _ => false,
        }
    }
}

/// One new module of every surface kind, none of them the fixture's: a host builds each, off screen.
fn one_of_each_kind() -> Vec<(ModuleId, SurfaceKind)> {
    let chat = ModuleId::instance(ModuleKind::Agent, 2).expect("a chat view's number");
    vec![
        (
            ModuleId::instance(ModuleKind::Terminal, 2).expect("a terminal's number"),
            SurfaceKind::Terminal { cwd: ".".into() },
        ),
        (
            chat.clone(),
            SurfaceKind::Chat {
                view: ViewId::of_module(&chat).expect("a chat module"),
            },
        ),
        (
            ModuleId::lua("panel"),
            SurfaceKind::LuaPanel {
                url: "about:blank".into(),
                title: "panel".into(),
            },
        ),
        (
            ModuleId::lua("plugin"),
            SurfaceKind::PluginPage {
                plugin: "plugin".into(),
            },
        ),
        (
            ModuleId::editor(),
            SurfaceKind::Editor {
                spawn: EditorSpawn {
                    env: vec![],
                    args: vec![],
                    cwd: None,
                    clean: true,
                },
            },
        ),
    ]
}

pub fn scripts(f: &Fixture) -> Vec<Script> {
    let focus = |module: &ModuleId| HostOp::Focus {
        win: f.win,
        to: FocusTo::Module(module.clone()),
        on_grab: vec![],
    };
    let made = one_of_each_kind();
    // Each module is created, then mapped and given the keys: a surface the host did not register refuses both.
    let mut create_ops: Vec<HostOp> = made
        .iter()
        .map(|(module, kind)| HostOp::CreateSurface {
            win: f.win,
            module: module.clone(),
            kind: kind.clone(),
        })
        .collect();
    let mut create_expect: Vec<Expect> = (0..made.len())
        .map(|op| Expect::Answer {
            op,
            applied: Applied::Done,
        })
        .collect();
    for (module, _) in &made {
        create_ops.push(HostOp::Map {
            win: f.win,
            module: module.clone(),
        });
        create_ops.push(focus(module));
        create_expect.push(Expect::Answer {
            op: create_ops.len() - 1,
            applied: Applied::Done,
        });
        create_expect.push(Expect::PostedDuring {
            op: create_ops.len() - 1,
            event: EventMatch::NativeFocusOn(Some(module.clone())),
        });
    }
    create_expect.push(Expect::NotPosted(EventMatch::Refused(OpTag::CreateSurface)));
    let panel = ModuleId::lua("panel");
    vec![
        Script {
            name: "create_surface_of_each_kind_answers_done_and_registers_it",
            ops: create_ops,
            expect: create_expect,
        },
        // The kill already took it off screen; the drop ends what the kill ends, and the widget stays the module's
        // surface, so its next show maps it and gives it the keys again.
        Script {
            name: "drop_surface_of_a_killed_web_module_keeps_its_widget",
            ops: vec![
                HostOp::CreateSurface {
                    win: f.win,
                    module: panel.clone(),
                    kind: SurfaceKind::LuaPanel {
                        url: "about:blank".into(),
                        title: "panel".into(),
                    },
                },
                HostOp::Map {
                    win: f.win,
                    module: panel.clone(),
                },
                HostOp::Unmap {
                    win: f.win,
                    module: panel.clone(),
                },
                HostOp::DropSurface {
                    win: f.win,
                    module: panel.clone(),
                    why: DropWhy::Killed,
                },
                HostOp::Map {
                    win: f.win,
                    module: panel.clone(),
                },
                focus(&panel),
            ],
            expect: vec![
                Expect::Answer {
                    op: 3,
                    applied: Applied::Done,
                },
                Expect::Answer {
                    op: 4,
                    applied: Applied::Done,
                },
                Expect::Answer {
                    op: 5,
                    applied: Applied::Done,
                },
                Expect::PostedDuring {
                    op: 5,
                    event: EventMatch::NativeFocusOn(Some(panel.clone())),
                },
                Expect::NotPosted(EventMatch::Refused(OpTag::DropSurface)),
            ],
        },
        Script {
            name: "focus_posts_native_focus_while_it_applies",
            ops: vec![focus(&f.a)],
            expect: vec![
                Expect::Answer {
                    op: 0,
                    applied: Applied::Done,
                },
                Expect::PostedDuring {
                    op: 0,
                    event: EventMatch::NativeFocusOn(Some(f.a.clone())),
                },
            ],
        },
        // A window's start: shown, then its first module given the keys, the grab's tail (the chat's arrival)
        // carried with it for the driver to run once the grab took.
        Script {
            name: "present_then_focus_gives_the_first_module_the_keys",
            ops: vec![
                HostOp::Present {
                    win: f.win,
                    activation: None,
                },
                HostOp::Focus {
                    win: f.win,
                    to: FocusTo::Module(f.a.clone()),
                    on_grab: vec![HostOp::Chat(ChatOp::Arrive {
                        win: f.win,
                        view: ViewId::BASE,
                    })],
                },
            ],
            expect: vec![
                Expect::Answer {
                    op: 0,
                    applied: Applied::Done,
                },
                Expect::Answer {
                    op: 1,
                    applied: Applied::Done,
                },
                Expect::PostedDuring {
                    op: 1,
                    event: EventMatch::NativeFocusOn(Some(f.a.clone())),
                },
                Expect::NotPosted(EventMatch::Refused(OpTag::Present)),
            ],
        },
        Script {
            name: "focus_on_a_hidden_module_is_refused",
            ops: vec![focus(&f.hidden)],
            expect: vec![
                Expect::Answer {
                    op: 0,
                    applied: Applied::Refused,
                },
                Expect::PostedDuring {
                    op: 0,
                    event: EventMatch::Refused(OpTag::Focus),
                },
                Expect::NotPosted(EventMatch::NativeFocusOn(Some(f.hidden.clone()))),
            ],
        },
        Script {
            name: "unmapping_the_focus_owner_reports_focus_moving",
            ops: vec![
                focus(&f.a),
                HostOp::Unmap {
                    win: f.win,
                    module: f.a.clone(),
                },
            ],
            expect: vec![Expect::PostedDuring {
                op: 1,
                event: EventMatch::NativeFocusAwayFrom(f.a.clone()),
            }],
        },
        Script {
            name: "close_window_never_asks",
            ops: vec![HostOp::CloseWindow { win: f.win }],
            expect: vec![Expect::NoCloseRequest],
        },
    ]
}

pub fn run(host: &dyn Host, sink: &RecordingSink, script: &Script) -> Result<(), String> {
    let start = sink.events.borrow().len();
    let close_requests = sink.close_requests();
    let answers: Vec<_> = script
        .ops
        .iter()
        .enumerate()
        .map(|(i, op)| {
            sink.applying(Some(i));
            let answer = host.apply(op);
            sink.applying(None);
            answer
        })
        .collect();
    let events = sink.events();
    let events = &events[start..];
    for expect in &script.expect {
        let met = match expect {
            Expect::Answer { op, applied } => answers.get(*op) == Some(applied),
            Expect::PostedDuring { op, event } => events.iter().any(|(i, e)| *i == Some(*op) && event.matches(e)),
            Expect::NotPosted(event) => !events.iter().any(|(_, e)| event.matches(e)),
            Expect::NoCloseRequest => sink.close_requests() == close_requests,
        };
        if !met {
            return Err(format!("{}: unmet expectation {expect:?}", script.name));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wm::fake::FakeHost;
    use std::rc::Rc;

    #[test]
    fn conformance_reports_the_first_unmet_expectation_and_ignores_previous_events() {
        let h = FakeHost::new();
        h.set_model(false);
        let sink = RecordingSink::default();
        sink.post(HostEvent::Wake);
        sink.close_requested(WinId::MAIN, CloseBy::User);
        let script = Script {
            name: "missing-focus",
            ops: vec![HostOp::Quit],
            expect: vec![
                Expect::NoCloseRequest,
                Expect::PostedDuring {
                    op: 0,
                    event: EventMatch::NativeFocusOn(None),
                },
                Expect::Answer {
                    op: 0,
                    applied: Applied::Refused,
                },
            ],
        };
        let error = run(h.as_ref(), &sink, &script).unwrap_err();
        assert!(
            error.starts_with("missing-focus: unmet expectation PostedDuring"),
            "{error}"
        );
        assert_eq!(sink.applying.get(), None);
        assert!(
            EventMatch::NativeFocusAwayFrom(ModuleId::agent()).matches(&HostEvent::NativeFocus {
                win: WinId::MAIN,
                owner: Some(FocusOwner::Module(ModuleId::terminal())),
                chain: FocusChain(vec![ModuleId::terminal()]),
                active: true,
                op_index: None,
            })
        );
        assert!(
            EventMatch::PageShown(ViewId::BASE).matches(&HostEvent::Chat(ChatEvent::PageShown {
                win: WinId::MAIN,
                view: ViewId::BASE,
                op_index: None,
            }))
        );
    }

    #[test]
    fn the_fake_host_passes_the_conformance_suite() {
        let f = Fixture {
            win: WinId::MAIN,
            a: ModuleId::agent(),
            b: ModuleId::terminal(),
            hidden: ModuleId::editor(),
        };
        for script in scripts(&f) {
            let host = FakeHost::new();
            let sink = Rc::new(RecordingSink::default());
            let erased: Rc<dyn HostSink> = sink.clone();
            host.attach(Rc::downgrade(&erased));
            for (m, mapped) in [(&f.a, true), (&f.b, true), (&f.hidden, false)] {
                host.with_surface(f.win, m.clone(), SurfaceKind::Chat { view: ViewId::BASE }, mapped);
            }
            run(host.as_ref(), &sink, &script).unwrap();
        }
    }
}
