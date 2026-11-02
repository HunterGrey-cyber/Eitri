//! Reusable operation scripts for native hosts and their display-free doubles.

use super::{host::*, window::*};
use crate::layout::{Direction, ModuleId, ModuleKind};
use crate::tabs::ViewId;
use crate::window_link::names::WindowName;
use crate::window_link::HubAsk;
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

/// The complete service teardown, for hosts whose cleanup and shutdown services are attached.
pub fn teardown_script(win: WinId) -> Script {
    teardown_script_for(win, None)
}

/// The teardown of a window that keeps the editor's record, `record` being its op: right after nvim's shutdown, ahead
/// of the channel directory's removal.
pub fn teardown_script_for(win: WinId, record: Option<CleanupOp>) -> Script {
    let mut ops = vec![
        HostOp::Cleanup(CleanupOp::PairOnClose { layout_written: true }),
        HostOp::Editor(EditorCall::Shutdown { win }),
    ];
    ops.extend(record.map(HostOp::Cleanup));
    ops.extend([
        HostOp::Terminal(TermOp::ShutdownAll { win }),
        HostOp::Cleanup(CleanupOp::ScratchDir),
        HostOp::Cleanup(CleanupOp::PaneSwitchDir),
        HostOp::Cleanup(CleanupOp::Feeds),
        HostOp::Plugin(PluginOp::ShutdownAll { win }),
        HostOp::Chat(ChatOp::ShutdownAll { win, hold: true }),
        HostOp::CloseWindow { win },
    ]);
    let mut expect: Vec<_> = (0..ops.len())
        .map(|op| Expect::Answer {
            op,
            applied: Applied::Done,
        })
        .collect();
    expect.push(Expect::NoCloseRequest);
    Script {
        name: "the_teardown_ops_are_answered_done_in_order",
        ops,
        expect,
    }
}

#[derive(Debug)]
pub enum Expect {
    Answer {
        op: usize,
        applied: Applied,
    },
    PostedDuring {
        op: usize,
        event: EventMatch,
    },
    NotPosted(EventMatch),
    NoCloseRequest,
    /// The script's ops asked the window's grid for this many allocations.
    AllocationsAsked(usize),
    /// The window's module hosts are stacked so once the ops have applied, bottom first.
    Stacking(Vec<ModuleId>),
    /// The window's grid holds this many hosts of the module once the ops have applied.
    HostsOf(ModuleId, usize),
    /// The script's window is no longer open once the ops have applied: its close went through.
    WindowClosed,
}

/// What a host's grid did that no event reports, read around a script: the expectations on allocations and
/// stacking need one ([`run_observed`]).
pub trait GridObserve {
    /// How many allocations `win`'s grid has been asked for since the host was set up.
    fn allocations_asked(&self, win: WinId) -> usize;
    /// `win`'s module hosts as its grid stacks them, bottom first.
    fn stacking(&self, win: WinId) -> Vec<ModuleId>;
    /// Whether `win` is still open: shown, not closed by an op or a close the host let through.
    fn window_open(&self, win: WinId) -> bool;
}

#[derive(Debug)]
pub enum EventMatch {
    NativeFocusOn(Option<ModuleId>),
    /// An unmap may clear focus or move it directly to another native destination.
    NativeFocusAwayFrom(ModuleId),
    Refused(OpTag),
    RefusedWithText(OpTag, String),
    /// A refusal of an op on `module`'s surface, naming it, so the controller knows which module it was for.
    RefusedSurface(OpTag, ModuleId),
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
            (Self::RefusedWithText(expected, text), HostEvent::Refused { what, why, .. }) => {
                expected == what && text == why
            }
            (Self::Refused(expected), HostEvent::Refused { what, .. }) => expected == what,
            (Self::RefusedSurface(expected, module), HostEvent::Refused { what, surface, .. }) => {
                expected == what && surface.as_ref().is_some_and(|(_, m)| m == module)
            }
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
                    background: None,
                    open: vec![],
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
    let instance = ModuleId::instance(ModuleKind::Terminal, 3).expect("a terminal's number");
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
        // A new instance made at run time, then made again under its number (a kill left its host idle): one host,
        // mapped and given the keys each time. What this holds is the host's side of a re-create: its maps and its
        // focus. That a held host is kept and wired with its controllers only once is the maker's, which a host's
        // conformance stage stands in for, so the GTK shell holds it in the maker's own tests and in the scan that
        // every host's controllers come from `wire_host`.
        Script {
            name: "create_surface_of_a_runtime_instance_is_built_and_wired_once",
            ops: vec![
                HostOp::CreateSurface {
                    win: f.win,
                    module: instance.clone(),
                    kind: SurfaceKind::Terminal { cwd: ".".into() },
                },
                HostOp::Map {
                    win: f.win,
                    module: instance.clone(),
                },
                focus(&instance),
                HostOp::Unmap {
                    win: f.win,
                    module: instance.clone(),
                },
                HostOp::CreateSurface {
                    win: f.win,
                    module: instance.clone(),
                    kind: SurfaceKind::Terminal { cwd: "..".into() },
                },
                HostOp::Map {
                    win: f.win,
                    module: instance.clone(),
                },
                focus(&instance),
            ],
            expect: vec![
                Expect::Answer {
                    op: 0,
                    applied: Applied::Done,
                },
                Expect::PostedDuring {
                    op: 2,
                    event: EventMatch::NativeFocusOn(Some(instance.clone())),
                },
                Expect::Answer {
                    op: 4,
                    applied: Applied::Done,
                },
                Expect::Answer {
                    op: 6,
                    applied: Applied::Done,
                },
                Expect::PostedDuring {
                    op: 6,
                    event: EventMatch::NativeFocusOn(Some(instance.clone())),
                },
                Expect::HostsOf(instance.clone(), 1),
                Expect::NotPosted(EventMatch::Refused(OpTag::CreateSurface)),
                Expect::NotPosted(EventMatch::Refused(OpTag::Focus)),
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
                        card_first: true,
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
        // A change that moves the keys off a module leaving the screen: the arriving module is mapped and given
        // them before the leaving one is unmapped, so the keys never sit on a host going off screen.
        Script {
            name: "map_then_focus_then_unmap_moves_the_keys_to_the_mapped_module",
            ops: vec![
                focus(&f.a),
                HostOp::Map {
                    win: f.win,
                    module: f.hidden.clone(),
                },
                focus(&f.hidden),
                HostOp::Unmap {
                    win: f.win,
                    module: f.a.clone(),
                },
            ],
            expect: vec![
                Expect::Answer {
                    op: 1,
                    applied: Applied::Done,
                },
                Expect::Answer {
                    op: 2,
                    applied: Applied::Done,
                },
                Expect::PostedDuring {
                    op: 2,
                    event: EventMatch::NativeFocusOn(Some(f.hidden.clone())),
                },
                Expect::Answer {
                    op: 3,
                    applied: Applied::Done,
                },
                Expect::NotPosted(EventMatch::Refused(OpTag::Focus)),
                Expect::NotPosted(EventMatch::NativeFocusOn(None)),
            ],
        },
        // A module on a tab its container does not draw: its tab is chosen as a layout change, so its host is
        // mapped, given the keys and only then is the tab it replaces unmapped. The grab comes right after the map,
        // in the same batch, and answers as a grab of any module on screen does, its tail carried for the driver.
        Script {
            name: "a_focus_on_a_tab_not_drawn_after_its_map_is_answered_done",
            ops: vec![
                focus(&f.a),
                HostOp::Map {
                    win: f.win,
                    module: f.hidden.clone(),
                },
                HostOp::Focus {
                    win: f.win,
                    to: FocusTo::Module(f.hidden.clone()),
                    on_grab: vec![HostOp::Chat(ChatOp::Arrive {
                        win: f.win,
                        view: ViewId::BASE,
                        card_first: true,
                    })],
                },
                HostOp::Unmap {
                    win: f.win,
                    module: f.a.clone(),
                },
            ],
            expect: vec![
                Expect::Answer {
                    op: 2,
                    applied: Applied::Done,
                },
                Expect::PostedDuring {
                    op: 2,
                    event: EventMatch::NativeFocusOn(Some(f.hidden.clone())),
                },
                Expect::Answer {
                    op: 3,
                    applied: Applied::Done,
                },
                Expect::NotPosted(EventMatch::Refused(OpTag::Focus)),
            ],
        },
        // The grid allocates once per op, whatever else is queued: the toolkit coalesces what it is asked.
        Script {
            name: "allocate_queues_one_allocation",
            ops: vec![HostOp::Allocate { win: f.win }],
            expect: vec![
                Expect::Answer {
                    op: 0,
                    applied: Applied::Done,
                },
                Expect::AllocationsAsked(1),
                Expect::NotPosted(EventMatch::Refused(OpTag::Allocate)),
            ],
        },
        // Two floats put on top in the op's order, bottom first; the tiled host keeps its place below them.
        Script {
            name: "restack_orders_the_float_band_only",
            ops: vec![HostOp::Restack {
                win: f.win,
                moves: vec![
                    RestackMove {
                        module: f.b.clone(),
                        after: None,
                    },
                    RestackMove {
                        module: f.a.clone(),
                        after: Some(f.b.clone()),
                    },
                ],
            }],
            expect: vec![
                Expect::Answer {
                    op: 0,
                    applied: Applied::Done,
                },
                Expect::Stacking(vec![f.hidden.clone(), f.b.clone(), f.a.clone()]),
                Expect::NotPosted(EventMatch::Refused(OpTag::Restack)),
            ],
        },
        // The chrome's announcement is carried out by reading the window's model and redrawing from it: a host
        // that holds the window's chrome answers it done, and never refuses it.
        Script {
            name: "chrome_reads_the_model_and_answers_done",
            ops: vec![HostOp::Chrome { win: f.win, rev: 1 }],
            expect: vec![
                Expect::Answer {
                    op: 0,
                    applied: Applied::Done,
                },
                Expect::NotPosted(EventMatch::Refused(OpTag::Chrome)),
            ],
        },
        // The screen is no module, so the host reports no owner for it. The controller's batch puts the grab before
        // the chrome op that shows the screen: the host makes the screen visible itself in order to take the keys.
        Script {
            name: "focus_onboarding_before_the_chrome_op_grabs_and_reports_no_owner",
            ops: vec![
                HostOp::Focus {
                    win: f.win,
                    to: FocusTo::Onboarding,
                    on_grab: vec![],
                },
                HostOp::Chrome { win: f.win, rev: 1 },
            ],
            expect: vec![
                Expect::Answer {
                    op: 0,
                    applied: Applied::Done,
                },
                Expect::PostedDuring {
                    op: 0,
                    event: EventMatch::NativeFocusOn(None),
                },
                Expect::Answer {
                    op: 1,
                    applied: Applied::Done,
                },
                Expect::NotPosted(EventMatch::Refused(OpTag::Focus)),
            ],
        },
        Script {
            name: "close_window_never_asks",
            ops: vec![HostOp::CloseWindow { win: f.win }],
            expect: vec![Expect::NoCloseRequest],
        },
        // A terminal offered by the other window gets its place made off screen and is answered as carried out, so
        // the driver tells the controller it is prepared and the source may freeze it; nothing is refused.
        Script {
            name: "a_terminal_prepare_is_carried_out_and_answered_prepared",
            ops: vec![HostOp::PrepareSurface {
                win: f.win,
                module: ModuleId::instance(ModuleKind::Terminal, 3).expect("a terminal's number"),
                shape: crate::window_link::Shape::Tiled,
                size: (640, 480),
            }],
            expect: vec![
                Expect::Answer {
                    op: 0,
                    applied: Applied::Done,
                },
                Expect::NotPosted(EventMatch::Refused(OpTag::PrepareSurface)),
            ],
        },
        // A chat view offered by the other window gets its page made off screen and is answered as carried out, as a
        // terminal's place is.
        Script {
            name: "a_chat_view_prepare_is_carried_out_and_answered_prepared",
            ops: vec![HostOp::PrepareSurface {
                win: f.win,
                module: ModuleId::instance(ModuleKind::Agent, 3).expect("a chat view's number"),
                shape: crate::window_link::Shape::Tiled,
                size: (640, 480),
            }],
            expect: vec![
                Expect::Answer {
                    op: 0,
                    applied: Applied::Done,
                },
                Expect::NotPosted(EventMatch::Refused(OpTag::PrepareSurface)),
            ],
        },
        // The teardown's last op is the close itself: the host's own close never asks the sink (whose answer to an
        // asked close is always to stop it), and the close request it causes goes through, so the window is gone.
        Script {
            name: "close_window_closes_without_asking_and_proceeds",
            ops: vec![HostOp::CloseWindow { win: f.win }],
            expect: vec![
                Expect::Answer {
                    op: 0,
                    applied: Applied::Done,
                },
                Expect::NoCloseRequest,
                Expect::WindowClosed,
            ],
        },
    ]
}

/// A failed join reports the service's exact refusal while its op applies.
pub fn join_back_refusal_script(win: WinId, text: &str) -> Script {
    Script {
        name: "a_join_back_that_fails_is_posted_refused_with_its_text",
        ops: vec![HostOp::Chat(ChatOp::JoinBack {
            win,
            view: ViewId::of_module(&ModuleId::instance(ModuleKind::Agent, 2).unwrap()).unwrap(),
        })],
        expect: vec![
            Expect::PostedDuring {
                op: 0,
                event: EventMatch::RefusedSurface(OpTag::Chat, ViewId::new(2).unwrap().module()),
            },
            Expect::Answer {
                op: 0,
                applied: Applied::Refused,
            },
            Expect::PostedDuring {
                op: 0,
                event: EventMatch::RefusedWithText(OpTag::Chat, text.into()),
            },
        ],
    }
}

/// What a host that can start no second window answers the pair's ops, and the ones a module window's start and keys
/// add: each refused while it applies, so the
/// controller hears why. A host whose shell has no pair at all runs these; a host that carries the ops out does not.
pub fn pair_refusal_scripts(f: &Fixture) -> Vec<Script> {
    vec![
        // The toast says why no second window opened; nothing else of the window changes.
        Script {
            name: "a_refused_open_linked_window_is_posted_refused",
            ops: vec![HostOp::OpenLinkedWindow { win: f.win }],
            expect: vec![
                Expect::Answer {
                    op: 0,
                    applied: Applied::Refused,
                },
                Expect::PostedDuring {
                    op: 0,
                    event: EventMatch::Refused(OpTag::OpenLinkedWindow),
                },
            ],
        },
        Script {
            name: "a_refused_open_module_window_is_posted_refused",
            ops: vec![HostOp::OpenModuleWindow {
                win: f.win,
                name: WindowName::Chat(2),
                token: None,
                keep_keys: false,
            }],
            expect: vec![
                Expect::Answer {
                    op: 0,
                    applied: Applied::Refused,
                },
                Expect::PostedDuring {
                    op: 0,
                    event: EventMatch::Refused(OpTag::OpenModuleWindow),
                },
            ],
        },
        Script {
            name: "a_refused_peer_run_is_posted_refused",
            ops: vec![HostOp::PeerRun {
                win: f.win,
                peer: PeerId(16),
                name: "zoom".into(),
                opts: vec![],
            }],
            expect: vec![
                Expect::Answer {
                    op: 0,
                    applied: Applied::Refused,
                },
                Expect::PostedDuring {
                    op: 0,
                    event: EventMatch::Refused(OpTag::PeerRun),
                },
            ],
        },
        Script {
            name: "a_refused_hub_ask_is_posted_refused",
            ops: vec![HostOp::HubAsk {
                win: f.win,
                ask: HubAsk::InstanceNew,
            }],
            expect: vec![
                Expect::Answer {
                    op: 0,
                    applied: Applied::Refused,
                },
                Expect::PostedDuring {
                    op: 0,
                    event: EventMatch::Refused(OpTag::HubAsk),
                },
            ],
        },
        Script {
            name: "a_refused_compositor_focus_is_posted_refused",
            ops: vec![HostOp::CompositorFocus {
                win: f.win,
                direction: Direction::Left,
            }],
            expect: vec![
                Expect::Answer {
                    op: 0,
                    applied: Applied::Refused,
                },
                Expect::PostedDuring {
                    op: 0,
                    event: EventMatch::Refused(OpTag::CompositorFocus),
                },
            ],
        },
    ]
}

/// The window an op closes.
fn closed_win(op: &HostOp) -> Option<WinId> {
    match op {
        HostOp::CloseWindow { win } => Some(*win),
        _ => None,
    }
}

/// The window whose grid an op acts on.
fn grid_win(op: &HostOp) -> Option<WinId> {
    match op {
        HostOp::Allocate { win }
        | HostOp::Restack { win, .. }
        | HostOp::Map { win, .. }
        | HostOp::Unmap { win, .. } => Some(*win),
        _ => None,
    }
}

pub fn run(host: &dyn Host, sink: &RecordingSink, script: &Script) -> Result<(), String> {
    run_observed(host, sink, script, None)
}

/// [`run`], with what the host's grid did read through `grid`; a script that expects allocations or a
/// stacking fails without one.
pub fn run_observed(
    host: &dyn Host,
    sink: &RecordingSink,
    script: &Script,
    grid: Option<&dyn GridObserve>,
) -> Result<(), String> {
    let start = sink.events.borrow().len();
    let close_requests = sink.close_requests();
    let win = script.ops.iter().find_map(grid_win);
    let allocations = |grid: &dyn GridObserve| win.map_or(0, |win| grid.allocations_asked(win));
    let allocations_before = grid.map(&allocations);
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
            Expect::AllocationsAsked(count) => match (grid, allocations_before) {
                (Some(grid), Some(before)) => allocations(grid) - before == *count,
                _ => return Err(format!("{}: {expect:?} needs a grid observer", script.name)),
            },
            Expect::Stacking(order) => match (grid, win) {
                (Some(grid), Some(win)) => grid.stacking(win) == *order,
                _ => {
                    return Err(format!(
                        "{}: {expect:?} needs a grid observer and a window",
                        script.name
                    ))
                }
            },
            Expect::WindowClosed => match (grid, script.ops.iter().find_map(closed_win)) {
                (Some(grid), Some(win)) => !grid.window_open(win),
                _ => return Err(format!("{}: {expect:?} needs an observer and a window", script.name)),
            },
            Expect::HostsOf(module, count) => match (grid, win) {
                (Some(grid), Some(win)) => grid.stacking(win).iter().filter(|host| *host == module).count() == *count,
                _ => {
                    return Err(format!(
                        "{}: {expect:?} needs a grid observer and a window",
                        script.name
                    ))
                }
            },
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
            run_observed(host.as_ref(), &sink, &script, Some(host.as_ref())).unwrap();
        }
    }

    #[test]
    fn a_join_back_that_fails_is_posted_refused_with_its_text() {
        let host = FakeHost::new();
        host.refuse_op(|op| matches!(op, HostOp::Chat(ChatOp::JoinBack { .. })));
        let sink = Rc::new(RecordingSink::default());
        let erased: Rc<dyn HostSink> = sink.clone();
        host.attach(Rc::downgrade(&erased));
        run(
            host.as_ref(),
            &sink,
            &join_back_refusal_script(WinId::MAIN, "the native model cannot apply this operation"),
        )
        .unwrap();
    }

    /// The fake stands for a host with no pair by refusing its ops, as such a host does.
    #[test]
    fn the_fake_host_refusing_the_pairs_ops_passes_their_scripts() {
        let f = Fixture {
            win: WinId::MAIN,
            a: ModuleId::agent(),
            b: ModuleId::terminal(),
            hidden: ModuleId::editor(),
        };
        for script in pair_refusal_scripts(&f) {
            let host = FakeHost::new();
            host.refuse_op(|op| {
                matches!(
                    op,
                    HostOp::OpenLinkedWindow { .. }
                        | HostOp::OpenModuleWindow { .. }
                        | HostOp::PeerRun { .. }
                        | HostOp::HubAsk { .. }
                        | HostOp::CompositorFocus { .. }
                )
            });
            let sink = Rc::new(RecordingSink::default());
            let erased: Rc<dyn HostSink> = sink.clone();
            host.attach(Rc::downgrade(&erased));
            host.with_surface(f.win, f.a.clone(), SurfaceKind::Chat { view: ViewId::BASE }, true);
            run_observed(host.as_ref(), &sink, &script, Some(host.as_ref())).unwrap_or_else(|e| panic!("{e}"));
        }
    }

    #[test]
    fn a_refusal_naming_another_module_does_not_match_the_surface_expected() {
        let refused = |module: ModuleId| HostEvent::Refused {
            what: OpTag::PrepareSurface,
            why: "no".into(),
            surface: Some((WinId::MAIN, module)),
        };
        let expected = EventMatch::RefusedSurface(OpTag::PrepareSurface, ModuleId::terminal());
        assert!(expected.matches(&refused(ModuleId::terminal())));
        assert!(!expected.matches(&refused(ModuleId::agent())));
        assert!(!expected.matches(&HostEvent::Refused {
            what: OpTag::PrepareSurface,
            why: "no".into(),
            surface: None,
        }));
    }
}
