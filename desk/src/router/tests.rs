//! The router's own decisions, over a back end with no window: which collaborator each op reaches, what is told
//! as its own fact, and what is refused when a collaborator is missing.

use super::*;
use std::cell::Cell;
use std::path::PathBuf;

use crate::host_traits::{respawn_pane, RespawnPane};
use eitri_core::wm::{ChatOp, PluginOp, TermOp};

/// A host's back end with no window and nothing on screen, and the editors a test gives it.
#[derive(Default)]
pub(super) struct Bare {
    pub(super) editors: RefCell<std::collections::BTreeMap<WinId, Rc<dyn EditorReads>>>,
}

impl Views for Bare {
    fn attach(&self, _sink: Weak<dyn HostSink>) {}
    fn has_surface(&self, _win: WinId, _module: &ModuleId) -> bool {
        false
    }
    fn grid(&self, _win: WinId) -> Option<Rc<dyn GridPort>> {
        None
    }
    fn editor(&self, win: WinId) -> Option<Rc<dyn EditorReads>> {
        self.editors.borrow().get(&win).cloned()
    }
    fn focus_module(&self, _win: WinId, _module: &ModuleId) -> Result<(), String> {
        Err("the module has no surface in this window".into())
    }
    fn focus_top_bar(&self, _win: WinId, _item: u32) -> Result<(), String> {
        Err(NOT_CARRIED_OUT.into())
    }
    fn focus_rename_entry(&self, _win: WinId) -> Result<(), String> {
        Err(NOT_CARRIED_OUT.into())
    }
    fn focus_onboarding(&self, _win: WinId) -> Result<(), String> {
        Err(NOT_CARRIED_OUT.into())
    }
    fn grab_reported(&self, _win: WinId, grab: &dyn Fn() -> bool) -> bool {
        grab()
    }
    fn release_keys(&self, _win: WinId, _module: &ModuleId) {}
    fn show(&self, _win: WinId, _module: &ModuleId, _shown: bool) {}
    fn chrome(&self, _win: WinId) -> Result<(), String> {
        Err(NOT_CARRIED_OUT.into())
    }
}

impl WindowPort for Bare {
    fn attach(&self, _sink: Weak<dyn HostSink>) {}
    fn is_open(&self, _win: WinId) -> bool {
        false
    }
    fn open(&self, _win: WinId, _spec: &WindowSpec) -> Result<(), String> {
        Err(NOT_CARRIED_OUT.into())
    }
    fn present(&self, _win: WinId, _activation: Option<&ActivationToken>) -> Result<(), String> {
        Err("no such window".into())
    }
    fn close(&self, _win: WinId) -> Result<(), String> {
        Err("no such window".into())
    }
    fn toggle_mode(&self, _win: WinId) -> Result<(), String> {
        Err(NOT_CARRIED_OUT.into())
    }
    fn activation_token(&self, _win: WinId) -> Option<String> {
        None
    }
    fn place(&self, _win: WinId, _connector: Option<&str>, _role: eitri_core::wm::PlacementRole) -> Result<(), String> {
        Err("no such window".into())
    }
    fn compositor_focus(&self, _direction: Direction) -> Result<(), Option<String>> {
        Err(Some(NOT_CARRIED_OUT.into()))
    }
    fn compositor_name(&self) -> &'static str {
        "the compositor"
    }
    fn monitors(&self) -> Vec<Monitor> {
        Vec::new()
    }
}

pub(super) type TestRouter = Router<Bare, Bare>;

/// A router over a bare back end, and the back end.
pub(super) fn router() -> (Rc<TestRouter>, Rc<Bare>) {
    let bare = Rc::new(Bare::default());
    (Router::new(bare.clone(), bare.clone()), bare)
}

mod editor_reads {
    use super::*;
    use eitri_core::wm::{close::NvimAnswer, EditorFacts};
    use std::time::Instant;

    struct EditorSnapshot(RefCell<EditorFacts>);

    impl EditorReads for EditorSnapshot {
        fn cell(&self) -> Option<(f64, f64)> {
            None
        }
        fn running(&self) -> bool {
            self.0.borrow().running
        }
        fn facts(&self) -> EditorFacts {
            self.0.borrow().clone()
        }
    }

    #[test]
    fn editor_facts_copy_the_registered_panes_snapshot() {
        let (host, views) = router();
        let win = WinId::MAIN;
        assert_eq!(host.editor_facts(win), EditorFacts::default());
        let editor = Rc::new(EditorSnapshot(RefCell::new(EditorFacts {
            running: false,
            exited: false,
            exit_pending: true,
            starting: false,
            watch: Some(NvimAnswer {
                sent_at: Instant::now(),
                done: false,
                returned: false,
                last: None,
            }),
        })));
        views.editors.borrow_mut().insert(win, editor.clone());
        assert_eq!(host.editor_facts(win), *editor.0.borrow());
        assert_eq!(host.editor_facts(WinId(2)), EditorFacts::default());
        *editor.0.borrow_mut() = EditorFacts {
            exited: true,
            ..Default::default()
        };
        assert_eq!(host.editor_facts(win), *editor.0.borrow());
    }

    /// The editor's calls reach the window's editor service, the release of an exited nvim among them, which is told
    /// as its own fact. A quit the service could not send is refused as any failed op is, and told again as the
    /// quit's own fact with its generation, since the refusal names no call.
    #[test]
    fn a_quit_that_could_not_be_sent_is_told_with_its_generation() {
        use eitri_core::wm::conformance::RecordingSink;
        struct Services(RefCell<Vec<String>>);
        impl ServiceSet for Services {
            fn editor(&self, op: &EditorCall) -> Result<(), String> {
                self.0.borrow_mut().push(format!("{op:?}"));
                match op {
                    EditorCall::Quit { .. } => Err("nvim is not running; the quit was not sent".into()),
                    _ => Ok(()),
                }
            }
            fn terminal(&self, _: &TermOp) -> Result<(), String> {
                unreachable!()
            }
            fn plugin(&self, _: &PluginOp) -> Result<(), String> {
                unreachable!()
            }
            fn chat(&self, _: &ChatOp) -> Result<(), String> {
                unreachable!()
            }
        }
        let (host, _) = router();
        let sink = Rc::new(RecordingSink::default());
        let erased: Rc<dyn HostSink> = sink.clone();
        host.attach(Rc::downgrade(&erased));
        let services = Rc::new(Services(RefCell::default()));
        host.set_services(services.clone());

        let release = HostOp::Editor(EditorCall::ReleaseExited { win: WinId::MAIN });
        assert_eq!(
            host.apply(&release),
            Applied::Done,
            "the release reaches the editor service"
        );
        let events: Vec<HostEvent> = sink.events().into_iter().map(|(_, event)| event).collect();
        assert_eq!(events, [HostEvent::Editor(EditorEvent::Released { win: WinId::MAIN })]);

        let quit = HostOp::Editor(EditorCall::Quit {
            win: WinId::MAIN,
            lua: "quit".into(),
            generation: 7,
            editor: eitri_core::wm::owned_editor_for_tests(),
        });
        assert_eq!(host.apply(&quit), Applied::Refused);
        let events: Vec<HostEvent> = sink.events().into_iter().skip(1).map(|(_, event)| event).collect();
        assert!(matches!(
            events.as_slice(),
            [
                HostEvent::Refused {
                    what: OpTag::Editor,
                    ..
                },
                HostEvent::Editor(EditorEvent::QuitNotSent {
                    win: WinId::MAIN,
                    generation: 7
                })
            ]
        ));
        assert_eq!(services.0.borrow().len(), 2, "both calls reached the service");
    }

    /// An open's answer goes to the service that holds the socket; with none, it is refused as any op is.
    #[test]
    fn an_open_answer_reaches_the_service() {
        use eitri_core::wm::conformance::RecordingSink;
        #[derive(Default)]
        struct Services(RefCell<Vec<(u64, bool)>>);
        impl ServiceSet for Services {
            fn editor(&self, _: &EditorCall) -> Result<(), String> {
                unreachable!()
            }
            fn terminal(&self, _: &TermOp) -> Result<(), String> {
                unreachable!()
            }
            fn plugin(&self, _: &PluginOp) -> Result<(), String> {
                unreachable!()
            }
            fn chat(&self, _: &ChatOp) -> Result<(), String> {
                unreachable!()
            }
            fn open_answer(&self, id: u64, accepted: bool) -> Result<(), String> {
                self.0.borrow_mut().push((id, accepted));
                Ok(())
            }
        }
        let (host, _) = router();
        let sink = Rc::new(RecordingSink::default());
        let erased: Rc<dyn HostSink> = sink.clone();
        host.attach(Rc::downgrade(&erased));
        let answer = HostOp::OpenAnswer { id: 7, accepted: true };
        assert_eq!(host.apply(&answer), Applied::Refused, "no services yet");
        let services = Rc::new(Services::default());
        host.set_services(services.clone());
        assert_eq!(host.apply(&answer), Applied::Done);
        assert_eq!(*services.0.borrow(), [(7, true)]);
    }

    /// A release nvim's living process refused, and a restart the pane refused from the state it was in, are each
    /// refused as any failed op is and told as their own facts, the restart's with the state, since a refusal names
    /// no call. A restart carried out is told nothing until its nvim is ready.
    #[test]
    fn a_refused_release_and_a_refused_restart_are_told_as_their_own_facts() {
        use eitri_core::wm::conformance::RecordingSink;
        use eitri_core::wm::EditorSpawn;
        struct Pane {
            exited: Cell<bool>,
            refused_from: RefCell<Option<String>>,
            restarts: RefCell<Vec<EditorSpawn>>,
        }
        impl ServiceSet for Pane {
            fn editor(&self, op: &EditorCall) -> Result<(), String> {
                match op {
                    EditorCall::ReleaseExited { .. } if self.exited.get() => Ok(()),
                    EditorCall::ReleaseExited { .. } => Err("nvim has not exited; the pane was not released".into()),
                    _ => unreachable!("{op:?}"),
                }
            }
            fn restart_editor(&self, spawn: &EditorSpawn) -> Result<(), RestartRefused> {
                self.restarts.borrow_mut().push(spawn.clone());
                let refused_from = self.refused_from.borrow().clone();
                match refused_from {
                    Some(state) => Err(RestartRefused::Pane { state }),
                    None => Ok(()),
                }
            }
            fn terminal(&self, _: &TermOp) -> Result<(), String> {
                unreachable!()
            }
            fn plugin(&self, _: &PluginOp) -> Result<(), String> {
                unreachable!()
            }
            fn chat(&self, _: &ChatOp) -> Result<(), String> {
                unreachable!()
            }
        }
        let (host, _) = router();
        let sink = Rc::new(RecordingSink::default());
        let erased: Rc<dyn HostSink> = sink.clone();
        host.attach(Rc::downgrade(&erased));
        let pane = Rc::new(Pane {
            exited: Cell::new(false),
            refused_from: RefCell::new(Some("Ready".into())),
            restarts: RefCell::default(),
        });
        host.set_services(pane.clone());
        let events = || -> Vec<HostEvent> { sink.events().into_iter().map(|(_, event)| event).collect() };

        let release = HostOp::Editor(EditorCall::ReleaseExited { win: WinId::MAIN });
        assert_eq!(host.apply(&release), Applied::Refused);
        assert!(matches!(
            events().as_slice(),
            [
                HostEvent::Refused {
                    what: OpTag::Editor,
                    ..
                },
                HostEvent::Editor(EditorEvent::ReleaseRefused { win: WinId::MAIN })
            ]
        ));

        let spawn = EditorSpawn {
            env: vec![("TMUX".into(), "x".into())],
            args: vec!["--cmd".into(), "let g:x = 1".into()],
            cwd: None,
            clean: false,
            background: None,
            open: vec![],
        };
        let restart = HostOp::Editor(EditorCall::Restart {
            win: WinId::MAIN,
            spawn: spawn.clone(),
            editor: eitri_core::wm::owned_editor_for_tests(),
        });
        assert_eq!(host.apply(&restart), Applied::Refused);
        let told = events();
        let [_, _, HostEvent::Refused {
            what: OpTag::Editor,
            why,
            ..
        }, HostEvent::Editor(EditorEvent::RestartRefused {
            win: WinId::MAIN,
            state,
        })] = told.as_slice()
        else {
            panic!("the restart's refusal and its fact: {told:?}");
        };
        assert!(why.contains("Ready"), "the refusal says the state: {why}");
        assert_eq!(state, "Ready");

        *pane.refused_from.borrow_mut() = None;
        assert_eq!(host.apply(&restart), Applied::Done);
        assert_eq!(events().len(), 4, "a restart carried out is told nothing yet");
        assert_eq!(
            *pane.restarts.borrow(),
            [spawn.clone(), spawn],
            "each restart reached the pane with its spawn"
        );
    }
}

// Only the grid's probe places hosts by hand; the decision is the router's whatever the build.
mod probe {
    use super::*;

    /// The probe's hosts stay where it put them: a map or an unmap of one reaches nothing, every other host's
    /// op is carried out as it came, and no other op is a map.
    #[test]
    fn map_and_unmap_skip_a_host_the_probe_places() {
        let placed = ModuleId::terminal();
        let laid_out = ModuleId::agent();
        let win = WinId::MAIN;
        let ops = vec![
            HostOp::Map {
                win,
                module: placed.clone(),
            },
            HostOp::Map {
                win,
                module: laid_out.clone(),
            },
            HostOp::Allocate { win },
            HostOp::Unmap {
                win,
                module: placed.clone(),
            },
            HostOp::Unmap {
                win,
                module: laid_out.clone(),
            },
        ];
        assert_eq!(
            hosts_to_map(&ops, |module| *module == placed),
            vec![(win, laid_out.clone(), true), (win, laid_out.clone(), false)]
        );
        assert_eq!(
            hosts_to_map(&ops, |_| false),
            vec![
                (win, placed.clone(), true),
                (win, laid_out.clone(), true),
                (win, placed, false),
                (win, laid_out, false),
            ],
            "without the probe every map and unmap is carried out"
        );
    }
}

mod terminal_at {
    use super::*;
    use eitri_core::wm::conformance::RecordingSink;
    use eitri_core::wm::ShellStart;

    #[derive(Clone)]
    struct Pane {
        shell: ShellStart,
        calls: Rc<RefCell<Vec<String>>>,
    }

    impl RespawnPane for Pane {
        fn shell_start(&self) -> ShellStart {
            self.shell
        }
        fn close(&self) {
            self.calls.borrow_mut().push("close".into());
        }
        fn set_start_dir(&self, dir: PathBuf) {
            self.calls.borrow_mut().push(format!("dir {}", dir.display()));
        }
        fn start(&self) {
            self.calls.borrow_mut().push("start".into());
        }
    }

    #[test]
    fn a_server_restart_without_a_server_op_is_refused_and_touches_nothing() {
        let shell = ShellStart::identified(
            1,
            Some(eitri_core::wm::ShellIdentity::Server {
                generation: 7,
                terminal_id: 1,
            }),
        );
        let calls = Rc::new(RefCell::new(Vec::new()));
        let pane = Pane {
            shell,
            calls: calls.clone(),
        };
        assert_eq!(
            respawn_pane(Some(pane), shell, "/srv/project".into()),
            Err("server terminal restart is not available yet".into())
        );
        assert!(calls.borrow().is_empty());
    }

    struct Services(Option<Pane>);
    impl ServiceSet for Services {
        fn editor(&self, _: &EditorCall) -> Result<(), String> {
            unreachable!()
        }
        fn plugin(&self, _: &PluginOp) -> Result<(), String> {
            unreachable!()
        }
        fn chat(&self, _: &ChatOp) -> Result<(), String> {
            unreachable!()
        }
        fn terminal(&self, op: &TermOp) -> Result<(), String> {
            match op {
                TermOp::Respawn { expect, dir, .. } => respawn_pane(self.0.clone(), *expect, dir.clone()),
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn a_respawn_for_a_pane_no_longer_running_expect_or_typed_into_since_is_refused_and_touches_nothing() {
        let expect = ShellStart::of(3, Some(42));
        let calls = Rc::new(RefCell::new(Vec::new()));
        let (host, _) = router();
        let sink = Rc::new(RecordingSink::default());
        let erased: Rc<dyn HostSink> = sink.clone();
        host.attach(Rc::downgrade(&erased));
        let op = HostOp::Terminal(TermOp::Respawn {
            win: WinId::MAIN,
            module: ModuleId::terminal(),
            dir: PathBuf::from("/new"),
            expect,
        });
        for shell in [
            None,
            Some(ShellStart::of(4, Some(42))),
            Some(ShellStart::of(3, Some(43))),
            Some(ShellStart::of(3, None)),
            Some(ShellStart::of(3, Some(42)).typed(1)),
        ] {
            host.set_services(Rc::new(Services(shell.map(|shell| Pane {
                shell,
                calls: calls.clone(),
            }))));
            assert_eq!(host.apply(&op), Applied::Refused);
            assert!(calls.borrow().is_empty());
            assert!(
                matches!(sink.events().last(), Some((_, HostEvent::Refused { what: OpTag::Respawn, surface: Some((WinId::MAIN, module)), .. })) if module == &ModuleId::terminal())
            );
        }
        host.set_services(Rc::new(Services(Some(Pane {
            shell: expect,
            calls: calls.clone(),
        }))));
        assert_eq!(host.apply(&op), Applied::Done);
        assert_eq!(*calls.borrow(), ["close", "dir /new", "start"]);
    }
}

mod module_window_ops {
    use super::*;
    use eitri_core::window_link::HubAsk;
    use eitri_core::wm::conformance::RecordingSink;

    #[derive(Default)]
    struct Linked {
        opened: RefCell<Vec<(WindowName, Option<String>, bool)>>,
        sent: RefCell<Vec<(PeerId, Msg)>>,
        to_hub: RefCell<Vec<Msg>>,
        handed: RefCell<Vec<(PeerId, String, WindowName, Option<String>)>>,
        open_windows: RefCell<Vec<WindowName>>,
    }

    impl LinkedWindows for Linked {
        fn open(&self, _: WinId, _: &WindowName, _: Option<String>) -> Result<(), String> {
            unreachable!()
        }
        fn restore(&self, _: WinId, _: &WindowName) -> Result<(), String> {
            unreachable!()
        }
        fn start_editor(&self, _: WinId) -> Result<(), String> {
            unreachable!()
        }
        fn module_window_open(&self, name: &WindowName) -> bool {
            self.open_windows.borrow().contains(name)
        }
        fn open_module(&self, _: WinId, name: &WindowName, token: Option<String>, keep: bool) -> Result<(), String> {
            self.opened.borrow_mut().push((name.clone(), token, keep));
            Ok(())
        }
        fn send(&self, _: WinId, peer: PeerId, msg: Msg) -> Result<(), String> {
            self.sent.borrow_mut().push((peer, msg));
            Ok(())
        }
        fn send_to_hub(&self, _: WinId, msg: Msg) -> Result<(), String> {
            self.to_hub.borrow_mut().push(msg);
            Ok(())
        }
        fn hand_over(
            &self,
            _: WinId,
            from: PeerId,
            _: &eitri_core::agent_backend::AgentId,
            session: &str,
            to: &WindowName,
            token: Option<String>,
        ) -> Result<(), String> {
            let handed = (from, session.to_string(), to.clone(), token);
            self.handed.borrow_mut().push(handed);
            Ok(())
        }
    }

    fn host() -> (Rc<TestRouter>, Rc<RecordingSink>, Rc<Linked>) {
        let (host, _) = router();
        let sink = Rc::new(RecordingSink::default());
        let erased: Rc<dyn HostSink> = sink.clone();
        host.attach(Rc::downgrade(&erased));
        let linked = Rc::new(Linked::default());
        host.set_linked_windows(linked.clone());
        (host, sink, linked)
    }

    fn chat(n: u8) -> WindowName {
        WindowName::Chat(n)
    }

    #[test]
    fn opening_a_module_window_passes_the_key_s_token_and_a_start_s_window_takes_none() {
        let (host, _sink, linked) = host();
        for (token, keep_keys) in [(Some("key"), false), (Some("dropped"), true), (None, false)] {
            let op = HostOp::OpenModuleWindow {
                win: WinId::MAIN,
                name: chat(2),
                token: token.map(str::to_string),
                keep_keys,
            };
            assert_eq!(host.apply(&op), Applied::Done);
        }
        assert_eq!(
            *linked.opened.borrow(),
            vec![
                (chat(2), Some("key".to_string()), false),
                (chat(2), None, true),
                // No window of this host to make a token from: it goes without one.
                (chat(2), None, false),
            ]
        );
    }

    #[derive(Default)]
    struct Pages(Cell<u32>);

    impl BasePageMaker for Pages {
        fn build(&self) {
            self.0.set(self.0.get() + 1);
        }
    }

    #[test]
    fn the_base_page_is_built_by_the_maker_and_refused_without_one() {
        let (host, sink, _) = host();
        let op = HostOp::BuildBasePage { win: WinId::MAIN };
        assert_eq!(host.apply(&op), Applied::Refused);
        assert!(sink.events().iter().any(|(_, event)| matches!(
            event,
            HostEvent::Refused {
                what: OpTag::BuildBasePage,
                ..
            }
        )));
        let pages = Rc::new(Pages::default());
        host.set_base_page_maker(pages.clone());
        assert_eq!(host.apply(&op), Applied::Done);
        assert_eq!(pages.0.get(), 1);
    }

    #[test]
    fn a_host_with_no_pair_refuses_the_ops_that_reach_another_window() {
        let (host, _) = router();
        let sink = Rc::new(RecordingSink::default());
        let erased: Rc<dyn HostSink> = sink.clone();
        host.attach(Rc::downgrade(&erased));
        let ops = [
            HostOp::OpenModuleWindow {
                win: WinId::MAIN,
                name: chat(1),
                token: None,
                keep_keys: false,
            },
            HostOp::PeerRun {
                win: WinId::MAIN,
                peer: PeerId(16),
                name: "zoom".into(),
                opts: vec![],
            },
            HostOp::HubAsk {
                win: WinId::MAIN,
                ask: HubAsk::InstanceNew,
            },
            HostOp::CompositorFocus {
                win: WinId::MAIN,
                direction: Direction::Left,
            },
            HostOp::HandOverTab {
                win: WinId::MAIN,
                from: PeerId(16),
                agent: eitri_core::agent_backend::AgentId::Claude,
                session: "s".into(),
                to: chat(2),
                token: None,
            },
            HostOp::KeepTab {
                win: WinId::MAIN,
                peer: PeerId(16),
                session: "s".into(),
                why: "no".into(),
            },
        ];
        for op in &ops {
            assert_eq!(host.apply(op), Applied::Refused, "{op:?}");
        }
        let refused: Vec<OpTag> = sink
            .events()
            .into_iter()
            .filter_map(|(_, event)| match event {
                HostEvent::Refused { what, .. } => Some(what),
                _ => None,
            })
            .collect();
        assert_eq!(
            refused,
            vec![
                OpTag::OpenModuleWindow,
                OpTag::PeerRun,
                OpTag::HubAsk,
                OpTag::CompositorFocus,
                OpTag::HandOverTab,
                OpTag::KeepTab
            ]
        );
    }

    #[test]
    fn a_hand_over_goes_to_the_pair_and_a_kept_tab_over_the_askers_link() {
        let (host, _sink, linked) = host();
        let hand_over = HostOp::HandOverTab {
            win: WinId::MAIN,
            from: PeerId(16),
            agent: eitri_core::agent_backend::AgentId::Claude,
            session: "s".into(),
            to: chat(3),
            token: Some("theirs".into()),
        };
        assert_eq!(host.apply(&hand_over), Applied::Done);
        assert_eq!(
            *linked.handed.borrow(),
            vec![(PeerId(16), "s".to_string(), chat(3), Some("theirs".to_string()))]
        );
        let keep = HostOp::KeepTab {
            win: WinId::MAIN,
            peer: PeerId(16),
            session: "s".into(),
            why: "all nine chat windows are open".into(),
        };
        assert_eq!(host.apply(&keep), Applied::Done);
        assert_eq!(
            *linked.sent.borrow(),
            vec![(
                PeerId(16),
                Msg::TabKept {
                    session: "s".into(),
                    why: "all nine chat windows are open".into()
                }
            )]
        );
    }

    #[test]
    fn a_forwarded_raise_is_answered_with_the_token_it_carries() {
        let (host, sink, _linked) = host();
        let op = HostOp::ForwardRaise {
            win: WinId::MAIN,
            peer: PeerId(17),
            token: Some("theirs".into()),
        };
        assert_eq!(host.apply(&op), Applied::Done);
        assert_eq!(
            sink.events().into_iter().map(|(_, event)| event).collect::<Vec<_>>(),
            vec![HostEvent::Peer(PeerEvent::Raised {
                peer: PeerId(17),
                token: Some("theirs".into())
            })]
        );
    }

    #[test]
    fn a_run_and_an_ask_go_over_the_link_as_the_messages_the_other_window_reads() {
        let (host, _sink, linked) = host();
        let run = HostOp::PeerRun {
            win: WinId::MAIN,
            peer: PeerId(17),
            name: "tab.select".into(),
            opts: vec![("n".into(), 3.into())],
        };
        assert_eq!(host.apply(&run), Applied::Done);
        let ask = HostOp::HubAsk {
            win: WinId::MAIN,
            ask: HubAsk::InstanceNew,
        };
        assert_eq!(host.apply(&ask), Applied::Done);
        assert_eq!(
            *linked.sent.borrow(),
            vec![(
                PeerId(17),
                Msg::RunAction {
                    name: "tab.select".into(),
                    opts: vec![("n".into(), 3.into())]
                }
            )]
        );
        assert_eq!(
            *linked.to_hub.borrow(),
            vec![Msg::HubAsk {
                ask: HubAsk::InstanceNew,
                token: None
            }]
        );
    }

    #[test]
    fn a_window_a_hand_over_starts_reads_as_open_to_the_controller() {
        let (host, _sink, linked) = host();
        assert!(!host.module_window_open(&chat(2)));
        linked.open_windows.borrow_mut().push(chat(2));
        assert!(host.module_window_open(&chat(2)));
        assert!(!host.module_window_open(&chat(3)));
    }
}
