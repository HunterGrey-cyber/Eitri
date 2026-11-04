//! The window controller's way to the other window of the pair, and what reaches it from there.
//!
//! [`LinkPort`] is the controller's [`PeerPort`] over the link: a send only queues on the link thread's channel,
//! so the controller calls it directly and nothing calls back. A terminal's handoff crosses as the [`Descriptors`]
//! the terminal pane wrapped in it, which go out as the packet's descriptors.
//!
//! [`inbound_event`] turns what the link thread passed on into the controller's [`PeerEvent`]s: the move protocol's
//! messages and the link's end. The descriptors of a `move` move into the event; any that arrive with another
//! message are closed here and logged, as a move's machine treats a packet out of turn. The pair's other messages
//! (the editor's feeds and review) keep their own handlers on the link. Pair focus and output facts go through
//! [`pair_event`], called by the pair's single handler for each kind.

use std::io;
use std::os::fd::OwnedFd;
use std::rc::Rc;
use std::sync::mpsc::Receiver;

use eitri_core::layout::ModuleId;
use eitri_core::window_link::link::{Inbound, LinkHandle};
use eitri_core::window_link::moves::MoveId;
use eitri_core::window_link::{LinkError, Msg};
use eitri_core::wm::moves::MoveKind;
use eitri_core::wm::{Descriptors, Handoff, HostEvent, PeerEvent, PeerId, PeerPort};

/// The kinds of message the controller hears from the other window: the move protocol's.
pub(crate) const MOVE_KINDS: [&str; 9] = [
    "move_offer",
    "move_ready",
    "move_refused",
    "move",
    "move_done",
    "move_abort",
    "move_ask",
    "workspace_move",
    "swap",
];

/// The longest session id a `move_ask` may name: a provider's session or thread id is far shorter, and anything
/// longer is not one.
const SESSION_MAX: usize = 256;

/// One window's end of the link as the port sends on it: the link's handle, or what wraps it on the GTK thread.
pub(crate) trait LinkSend {
    /// Queues `msg` with `fds`, which are closed once written.
    fn send(&self, msg: Msg, fds: Vec<OwnedFd>) -> Result<(), LinkError>;
    /// As `send`, and the receiver answers once the packet is on the socket, or never will be.
    fn send_confirmed(&self, msg: Msg, fds: Vec<OwnedFd>) -> Result<Receiver<io::Result<()>>, LinkError>;
}

impl LinkSend for LinkHandle {
    fn send(&self, msg: Msg, fds: Vec<OwnedFd>) -> Result<(), LinkError> {
        LinkHandle::send(self, msg, fds)
    }

    fn send_confirmed(&self, msg: Msg, fds: Vec<OwnedFd>) -> Result<Receiver<io::Result<()>>, LinkError> {
        LinkHandle::send_confirmed(self, msg, fds)
    }
}

/// The handoff a terminal pane gives for `fds`: what [`LinkPort`] sends with the `move` that carries it.
pub(crate) fn handoff_of(fds: Vec<OwnedFd>) -> Handoff {
    Handoff::new(Rc::new(Descriptors::new(fds)))
}

/// The controller's [`PeerPort`] over one link.
pub(crate) struct LinkPort<L> {
    link: L,
}

impl<L: LinkSend> LinkPort<L> {
    pub(crate) fn new(link: L) -> LinkPort<L> {
        LinkPort { link }
    }
}

impl<L: LinkSend> PeerPort for LinkPort<L> {
    fn send(&self, msg: Msg, handoff: Option<Handoff>) -> Result<(), String> {
        let fds = match handoff {
            None => Vec::new(),
            Some(handoff) => match handoff.inner().downcast_ref::<Descriptors>() {
                Some(fds) => fds.take(),
                // Nothing is sent rather than a `move` without the descriptors its kind declares.
                None => return Err("the handoff holds no descriptors the link can pass".into()),
            },
        };
        self.link.send(msg, fds).map_err(|e| e.to_string())
    }

    fn send_confirmed(&self, msg: Msg) -> Result<Receiver<io::Result<()>>, String> {
        self.link.send_confirmed(msg, Vec::new()).map_err(|e| e.to_string())
    }
}

/// What the link thread passed on, as the event the controller hears from `peer`; `None` for what is not the
/// controller's. Takes the packet by value, so a `move`'s descriptors move into its event.
pub(crate) fn inbound_event(peer: PeerId, inbound: Inbound) -> Option<HostEvent> {
    let (msg, fds) = match inbound {
        Inbound::Msg { msg, fds } => (msg, fds),
        Inbound::Closed => return Some(HostEvent::Peer(PeerEvent::LinkClosed { peer })),
        Inbound::Refused(err) => {
            eprintln!("[link] a packet from the other window was refused: {err}");
            return None;
        }
    };
    let kind = msg.kind();
    let event = match msg {
        Msg::Move { mv, record } => {
            return Some(HostEvent::Peer(PeerEvent::Move {
                peer,
                id: MoveId(mv),
                record,
                fds: Descriptors::new(fds),
            }));
        }
        Msg::MoveOffer {
            mv,
            module,
            kind,
            shape,
            size,
            session,
        } => match offered(&module, &kind) {
            Ok((module, kind)) => Some(PeerEvent::MoveOffer {
                peer,
                id: MoveId(mv),
                module,
                kind,
                shape,
                size,
                session,
            }),
            Err(why) => {
                eprintln!("[link] move {mv}: an offer this window cannot read ({why})");
                None
            }
        },
        Msg::WorkspaceMove {
            group,
            name,
            tree,
            moves,
            swap,
        } => Some(PeerEvent::WorkspaceMove {
            peer,
            group,
            name,
            tree,
            moves,
            swap,
        }),
        Msg::Swap { move_a, move_b } => Some(PeerEvent::Swap { peer, move_a, move_b }),
        Msg::MoveReady { mv } => Some(PeerEvent::MoveReady { peer, id: MoveId(mv) }),
        Msg::MoveRefused { mv, why } => Some(PeerEvent::MoveRefused {
            peer,
            id: MoveId(mv),
            why,
        }),
        Msg::MoveDone { mv } => Some(PeerEvent::MoveDone { peer, id: MoveId(mv) }),
        Msg::MoveAbort { mv, why } => Some(PeerEvent::MoveAbort {
            peer,
            id: MoveId(mv),
            why,
        }),
        Msg::MoveAsk {
            module,
            session,
            follow,
        } => asked(peer, module, session, follow),
        _ => None,
    };
    if !fds.is_empty() {
        // Only a `move` carries descriptors; any others close as they drop here.
        eprintln!("[link] `{kind}` came with {} descriptors; closed", fds.len());
    }
    drop(fds);
    event.map(HostEvent::Peer)
}

/// A `move_ask` as the controller hears it; `None`, logged, for one naming a module this build cannot read or a session
/// no provider would have, which is dropped as a key for a missing module is.
fn asked(peer: PeerId, module: Option<String>, session: Option<String>, follow: bool) -> Option<PeerEvent> {
    let module = match module.as_deref().map(ModuleId::parse) {
        None => None,
        Some(Ok(module)) => Some(module),
        Some(Err(err)) => {
            eprintln!("[link] the other window asked for a module this window cannot read: {err}");
            return None;
        }
    };
    if session.as_ref().is_some_and(|s| s.len() > SESSION_MAX) {
        eprintln!("[link] the other window asked for a session id over {SESSION_MAX} bytes; dropped");
        return None;
    }
    Some(PeerEvent::MoveAsk {
        peer,
        module,
        session,
        follow,
    })
}

/// The refusal the other window gets for an offer this window cannot read (a module or a kind this build does not
/// know), so its move ends at once rather than at its deadline; `None` for any other message.
pub(crate) fn unreadable_offer(msg: &Msg) -> Option<Msg> {
    let Msg::MoveOffer { mv, module, kind, .. } = msg else {
        return None;
    };
    offered(module, kind).err().map(|why| Msg::MoveRefused { mv: *mv, why })
}

fn offered(module: &str, kind: &str) -> Result<(ModuleId, MoveKind), String> {
    let module = ModuleId::parse(module).map_err(|e| format!("the module {module:?}: {e}"))?;
    let kind = MoveKind::from_wire(kind).ok_or_else(|| format!("this window does not know a {kind:?} to move"))?;
    Ok((module, kind))
}

/// Pair messages retain their single link handler and report their facts to the controller through it.
pub(crate) fn pair_event(peer: PeerId, msg: &Msg) -> Option<PeerEvent> {
    match msg {
        Msg::Summary(summary) => Some(PeerEvent::Summary {
            peer,
            summary: summary.clone(),
        }),
        Msg::Workspace { name, token } => Some(PeerEvent::Workspace {
            peer,
            name: name.clone(),
            token: token.clone(),
        }),
        Msg::Focus { target, token } => Some(PeerEvent::FocusIntent {
            peer,
            target: target.clone(),
            token: token.clone(),
        }),
        Msg::Away { ids } => Some(PeerEvent::Held {
            peer,
            ids: ids
                .iter()
                .filter_map(|id| match ModuleId::parse(id) {
                    Ok(id) => Some(id),
                    Err(err) => {
                        eprintln!("[link] a held module id {id:?} could not be read: {err}");
                        None
                    }
                })
                .collect(),
        }),
        Msg::Monitor { connector, .. } => Some(PeerEvent::Monitor {
            peer,
            connector: connector.clone(),
        }),
        Msg::HubAsk { ask, token } => Some(PeerEvent::HubAsk {
            peer,
            ask: ask.clone(),
            token: token.clone(),
        }),
        Msg::WindowFocused {} => Some(PeerEvent::WindowFocused { peer }),
        _ => None,
    }
}

/// The event posted once a wake of `peer`'s link has been drained: a confirmed send may have been answered.
pub(crate) fn woke(peer: PeerId) -> HostEvent {
    HostEvent::Peer(PeerEvent::Woke { peer })
}

#[cfg(test)]
mod tests {
    use super::*;
    use eitri_core::window_link::{fdpass, Shape, Summary};
    use std::cell::RefCell;
    use std::os::fd::{AsRawFd, RawFd};

    const PEER: PeerId = PeerId(2);

    fn msg(msg: Msg) -> Inbound {
        Inbound::Msg { msg, fds: Vec::new() }
    }

    fn peer_event(inbound: Inbound) -> Option<PeerEvent> {
        match inbound_event(PEER, inbound) {
            Some(HostEvent::Peer(event)) => Some(event),
            None => None,
            Some(other) => panic!("not a peer event: {other:?}"),
        }
    }

    fn is_open(fd: RawFd) -> bool {
        // SAFETY: `F_GETFD` only reads the descriptor table's flags for `fd`.
        unsafe { libc::fcntl(fd, libc::F_GETFD) != -1 }
    }

    /// Whether the other end of `fd`'s socket pair has closed: read by the end still held, so no descriptor number
    /// that another test may reuse is looked at once closed.
    fn other_end_closed(fd: &OwnedFd) -> bool {
        other_end_closes_within(fd, 0)
    }

    /// [`other_end_closed`], waiting up to `ms` for it: a child another test thread is starting holds a copy of every
    /// descriptor until it execs, so the other end can outlive this process's close by a moment.
    fn other_end_closes_within(fd: &OwnedFd, ms: i32) -> bool {
        let mut pfd = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one `pollfd` that lives for the call.
        let ready = unsafe { libc::poll(&mut pfd, 1, ms) };
        ready == 1 && pfd.revents & libc::POLLHUP != 0
    }

    #[test]
    fn inbound_event_names_every_message_the_controller_hears() {
        assert_eq!(
            peer_event(msg(Msg::MoveOffer {
                mv: 7,
                module: "terminal#2".into(),
                kind: "terminal".into(),
                shape: Shape::Tiled,
                size: (640, 480),
                session: Some("s".into()),
            })),
            Some(PeerEvent::MoveOffer {
                peer: PEER,
                id: MoveId(7),
                module: ModuleId::parse("terminal#2").unwrap(),
                kind: MoveKind::Terminal,
                shape: Shape::Tiled,
                size: (640, 480),
                session: Some("s".into()),
            })
        );
        assert_eq!(
            peer_event(msg(Msg::MoveReady { mv: 7 })),
            Some(PeerEvent::MoveReady {
                peer: PEER,
                id: MoveId(7)
            })
        );
        assert_eq!(
            peer_event(msg(Msg::MoveRefused {
                mv: 7,
                why: "full".into()
            })),
            Some(PeerEvent::MoveRefused {
                peer: PEER,
                id: MoveId(7),
                why: "full".into()
            })
        );
        let Some(PeerEvent::Move { peer, id, record, fds }) = peer_event(msg(Msg::Move {
            mv: 7,
            record: serde_json::json!({ "cols": 80 }),
        })) else {
            panic!("a move is the controller's");
        };
        assert_eq!((peer, id, record), (PEER, MoveId(7), serde_json::json!({ "cols": 80 })));
        assert!(fds.is_empty());
        assert_eq!(
            peer_event(msg(Msg::MoveDone { mv: 7 })),
            Some(PeerEvent::MoveDone {
                peer: PEER,
                id: MoveId(7)
            })
        );
        assert_eq!(
            peer_event(msg(Msg::MoveAbort {
                mv: 7,
                why: "gone".into()
            })),
            Some(PeerEvent::MoveAbort {
                peer: PEER,
                id: MoveId(7),
                why: "gone".into()
            })
        );
        assert_eq!(
            peer_event(msg(Msg::MoveAsk {
                module: Some("terminal#2".into()),
                session: None,
                follow: true,
            })),
            Some(PeerEvent::MoveAsk {
                peer: PEER,
                module: Some(ModuleId::parse("terminal#2").unwrap()),
                session: None,
                follow: true,
            })
        );
        for unreadable in [
            Msg::MoveAsk {
                module: Some("terminal#1".into()),
                session: None,
                follow: false,
            },
            Msg::MoveAsk {
                module: None,
                session: Some("s".repeat(SESSION_MAX + 1)),
                follow: false,
            },
        ] {
            assert_eq!(
                peer_event(msg(unreadable)),
                None,
                "an ask this window cannot read is dropped"
            );
        }
        assert_eq!(peer_event(Inbound::Closed), Some(PeerEvent::LinkClosed { peer: PEER }));
        assert_eq!(
            peer_event(Inbound::Refused(LinkError::UnknownKind("helo".into()))),
            None,
            "a refused packet is logged, not heard"
        );
        assert_eq!(woke(PEER), HostEvent::Peer(PeerEvent::Woke { peer: PEER }));
        // What the pair's own handlers take is not the controller's.
        for other in [
            Msg::Summary(Summary::default()),
            Msg::Focus {
                target: eitri_core::window_link::FocusTarget::Window,
                token: None,
            },
            Msg::Bye { why: "closed".into() },
            Msg::Away { ids: vec![] },
            Msg::Presence { held: true },
        ] {
            let kind = other.kind();
            assert_eq!(peer_event(msg(other)), None, "`{kind}` is not the controller's");
        }
        // An offer of a kind or module this build does not know never becomes an event; the other window is told.
        let unknown = Msg::MoveOffer {
            mv: 8,
            module: "terminal".into(),
            kind: "spreadsheet".into(),
            shape: Shape::Tiled,
            size: (1, 1),
            session: None,
        };
        assert!(matches!(
            unreadable_offer(&unknown),
            Some(Msg::MoveRefused { mv: 8, .. })
        ));
        assert_eq!(peer_event(msg(unknown)), None);
        let bad_module = Msg::MoveOffer {
            mv: 9,
            module: "terminal#1".into(),
            kind: "terminal".into(),
            shape: Shape::Tiled,
            size: (1, 1),
            session: None,
        };
        assert!(matches!(
            unreadable_offer(&bad_module),
            Some(Msg::MoveRefused { mv: 9, .. })
        ));
        assert_eq!(unreadable_offer(&Msg::MoveReady { mv: 9 }), None);
    }

    #[test]
    fn a_moves_descriptors_reach_the_event_and_others_are_closed() {
        let (a, b) = fdpass::seqpacket_pair().unwrap();
        let raw = [a.as_raw_fd(), b.as_raw_fd()];
        let Some(PeerEvent::Move { fds, .. }) = peer_event(Inbound::Msg {
            msg: Msg::Move {
                mv: 1,
                record: serde_json::Value::Null,
            },
            fds: vec![a, b],
        }) else {
            panic!("a move is the controller's");
        };
        let taken = fds.take();
        let held: Vec<RawFd> = taken.iter().map(AsRawFd::as_raw_fd).collect();
        assert_eq!(held, raw, "the event holds the very descriptors that arrived");
        assert!(raw.iter().all(|fd| is_open(*fd)), "and they are still open");

        let (c, c_peer) = fdpass::seqpacket_pair().unwrap();
        let (d, d_peer) = fdpass::seqpacket_pair().unwrap();
        assert!(!other_end_closed(&c_peer) && !other_end_closed(&d_peer));
        let event = peer_event(Inbound::Msg {
            msg: Msg::Summary(Summary::default()),
            fds: vec![c, d],
        });
        assert_eq!(event, None);
        assert!(
            other_end_closes_within(&c_peer, 20_000) && other_end_closes_within(&d_peer, 20_000),
            "descriptors that came with another message are closed"
        );
        drop(taken);
    }

    /// Records what the port sent, with the descriptors' numbers.
    #[derive(Default)]
    struct Sent(RefCell<Vec<(Msg, Vec<RawFd>)>>);

    impl LinkSend for Rc<Sent> {
        fn send(&self, msg: Msg, fds: Vec<OwnedFd>) -> Result<(), LinkError> {
            self.0
                .borrow_mut()
                .push((msg, fds.iter().map(AsRawFd::as_raw_fd).collect()));
            Ok(())
        }

        fn send_confirmed(&self, msg: Msg, fds: Vec<OwnedFd>) -> Result<Receiver<io::Result<()>>, LinkError> {
            self.send(msg, fds)?;
            let (tx, rx) = std::sync::mpsc::channel();
            tx.send(Ok(())).unwrap();
            Ok(rx)
        }
    }

    #[test]
    fn the_port_sends_a_handoffs_descriptors_with_its_message_and_refuses_one_it_cannot_read() {
        let sent: Rc<Sent> = Rc::default();
        let port = LinkPort::new(sent.clone());
        let (a, b) = fdpass::seqpacket_pair().unwrap();
        let raw = vec![a.as_raw_fd(), b.as_raw_fd()];
        let record = serde_json::json!({ "cols": 80 });
        port.send(
            Msg::Move {
                mv: 3,
                record: record.clone(),
            },
            Some(handoff_of(vec![a, b])),
        )
        .unwrap();
        port.send(Msg::MoveReady { mv: 3 }, None).unwrap();
        let answer = port.send_confirmed(Msg::MoveDone { mv: 3 }).unwrap();
        assert!(
            answer.recv().unwrap().is_ok(),
            "the link's own answer is returned as it is"
        );
        assert_eq!(
            *sent.0.borrow(),
            vec![
                (Msg::Move { mv: 3, record }, raw),
                (Msg::MoveReady { mv: 3 }, vec![]),
                (Msg::MoveDone { mv: 3 }, vec![]),
            ]
        );
        let foreign = Handoff::new(Rc::new(42_u8));
        assert!(port
            .send(Msg::MoveAbort { mv: 3, why: "x".into() }, Some(foreign))
            .is_err());
        assert_eq!(sent.0.borrow().len(), 3, "nothing goes out without the descriptors");
    }
    #[test]
    fn pair_messages_reach_the_controller_as_peer_events() {
        use eitri_core::window_link::{FocusTarget, Side};
        let summary = Summary {
            waiting: 1,
            unseen: 2,
            running: 3,
            queued: 4,
            background: 5,
            drafts: 6,
            workspaces: vec!["<peer>".into()],
        };
        assert_eq!(
            pair_event(PEER, &Msg::Summary(summary.clone())),
            Some(PeerEvent::Summary { peer: PEER, summary })
        );
        assert_eq!(
            pair_event(
                PEER,
                &Msg::Workspace {
                    name: "<peer>".into(),
                    token: Some("token".into())
                }
            ),
            Some(PeerEvent::Workspace {
                peer: PEER,
                name: "<peer>".into(),
                token: Some("token".into())
            })
        );
        let workspace = Msg::WorkspaceMove {
            group: 9,
            name: "work".into(),
            tree: serde_json::json!({"tiled": "agent"}),
            moves: vec![1, 3],
            swap: Some(8),
        };
        assert_eq!(
            peer_event(msg(workspace)),
            Some(PeerEvent::WorkspaceMove {
                peer: PEER,
                group: 9,
                name: "work".into(),
                tree: serde_json::json!({"tiled": "agent"}),
                moves: vec![1, 3],
                swap: Some(8)
            })
        );
        assert_eq!(
            peer_event(msg(Msg::Swap { move_a: 9, move_b: 8 })),
            Some(PeerEvent::Swap {
                peer: PEER,
                move_a: 9,
                move_b: 8
            })
        );
        assert!(MOVE_KINDS.contains(&"workspace_move") && MOVE_KINDS.contains(&"swap"));
        let targets = [
            FocusTarget::Window,
            FocusTarget::Module { id: "editor".into() },
            FocusTarget::Edge { side: Side::Left },
            FocusTarget::Edge { side: Side::Right },
            FocusTarget::Edge { side: Side::Up },
            FocusTarget::Edge { side: Side::Down },
        ];
        for target in targets {
            for token in [None, Some("activation".to_string())] {
                assert_eq!(
                    pair_event(
                        PEER,
                        &Msg::Focus {
                            target: target.clone(),
                            token: token.clone()
                        }
                    ),
                    Some(PeerEvent::FocusIntent {
                        peer: PEER,
                        target: target.clone(),
                        token
                    })
                );
            }
        }
        assert_eq!(
            pair_event(
                PEER,
                &Msg::Away {
                    ids: vec!["terminal#2".into(), "terminal#1".into(), "editor".into()]
                }
            ),
            Some(PeerEvent::Held {
                peer: PEER,
                ids: vec![ModuleId::parse("terminal#2").unwrap(), ModuleId::editor()]
            })
        );
        assert_eq!(
            pair_event(
                PEER,
                &Msg::Monitor {
                    connector: "DP-1".into(),
                    description: "Left".into()
                }
            ),
            Some(PeerEvent::Monitor {
                peer: PEER,
                connector: "DP-1".into()
            })
        );
        assert_eq!(pair_event(PEER, &Msg::MoveReady { mv: 1 }), None);
    }
}
