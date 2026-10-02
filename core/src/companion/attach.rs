//! Where the panel stands with a user's nvim: a pure state machine with no clock, no socket and no
//! thread. A worker does the connecting and the install and reports each step as an
//! [`AttachEvent`]; the machine answers with the [`Effect`]s the worker and the window carry out.
//!
//! Every attach starts a new generation. A worker stamps its events with the generation it was
//! started for, and an event from an older one changes nothing, so a slow connect to an editor the
//! user has since left cannot turn the panel back into attached.

use std::path::PathBuf;

use serde::Serialize;

use super::InstallReport;

/// Why a panel that was attached no longer is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Detach {
    EditorWentAway,
    /// Another Eitri panel installed itself into the same nvim.
    Replaced,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkState {
    NoEditor,
    Connecting {
        addr: PathBuf,
    },
    /// Connected and installing. `waiting_for_key` is true while nvim sits in a state that holds
    /// the install back until the user answers (a pending `f`, a hit-enter prompt).
    Attaching {
        addr: PathBuf,
        waiting_for_key: bool,
    },
    Attached {
        addr: PathBuf,
        channel: u64,
        nvim_pid: u32,
        in_tmux: bool,
    },
    Detached {
        why: Detach,
    },
    Failed {
        addr: PathBuf,
        why: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachEvent {
    /// Connect to this nvim, starting a new generation even when it is the one the panel is at.
    Attach(PathBuf),
    /// Someone asked for this nvim (`:EitriPanel`, `eitri panel --nvim`). The plugin sends its own
    /// nvim's address every time, so asking from the editor the panel already follows is how a user
    /// brings the panel forward: that changes nothing here. Any other address, or the same one
    /// after the link was lost or failed, is an [`AttachEvent::Attach`].
    Requested(PathBuf),
    Connected {
        gen: u64,
        channel: u64,
    },
    Blocking {
        gen: u64,
        blocking: bool,
    },
    Installed {
        gen: u64,
        report: InstallReport,
    },
    InstallFailed {
        gen: u64,
        why: String,
    },
    ConnectFailed {
        gen: u64,
        why: String,
    },
    /// The connection ended.
    Closed {
        gen: u64,
    },
    /// nvim told this panel that another panel replaced it.
    Replaced {
        gen: u64,
    },
}

impl AttachEvent {
    /// The generation the event was stamped with; `None` for [`AttachEvent::Attach`], which starts one.
    fn gen(&self) -> Option<u64> {
        match self {
            AttachEvent::Attach(_) | AttachEvent::Requested(_) => None,
            AttachEvent::Connected { gen, .. }
            | AttachEvent::Blocking { gen, .. }
            | AttachEvent::Installed { gen, .. }
            | AttachEvent::InstallFailed { gen, .. }
            | AttachEvent::ConnectFailed { gen, .. }
            | AttachEvent::Closed { gen }
            | AttachEvent::Replaced { gen } => Some(*gen),
        }
    }
}

/// What the caller must do, in the order given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Start a worker for this generation: connect, set the client info, send the install.
    Connect { gen: u64, addr: PathBuf },
    /// Ask the nvim of this generation to remove the glue now, rather than when it notices the
    /// channel is gone.
    ExplicitTeardown { gen: u64 },
    /// Close the connection of this generation.
    Close { gen: u64 },
    /// End the scratch edits that were waiting on this nvim: their answer can no longer arrive.
    CancelDraftEdits,
}

/// The band above the panel's transcript.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BandLink {
    pub state: &'static str,
    pub text: String,
}

#[derive(Debug)]
pub struct Attacher {
    state: LinkState,
    gen: u64,
    /// The channel of the connection of the current generation, once it is known. `Attaching` has
    /// no field for it, and `Attached` needs it.
    channel: Option<u64>,
}

impl Attacher {
    /// A machine for a window started with an editor address, or without one.
    pub fn new(initial: Option<PathBuf>) -> (Attacher, Vec<Effect>) {
        let mut attacher = Attacher {
            state: LinkState::NoEditor,
            gen: 0,
            channel: None,
        };
        let effects = match initial {
            Some(addr) => attacher.handle(AttachEvent::Attach(addr)),
            None => Vec::new(),
        };
        (attacher, effects)
    }

    pub fn state(&self) -> &LinkState {
        &self.state
    }

    /// The generation events must carry to count.
    pub fn gen(&self) -> u64 {
        self.gen
    }

    pub fn handle(&mut self, event: AttachEvent) -> Vec<Effect> {
        let event = match event {
            AttachEvent::Attach(addr) => return self.attach(addr),
            AttachEvent::Requested(addr) if self.follows(&addr) => return Vec::new(),
            AttachEvent::Requested(addr) => return self.attach(addr),
            event => event,
        };
        if event.gen() != Some(self.gen) {
            return Vec::new();
        }
        let gen = self.gen;
        // Anything the table does not name leaves the state alone: events from a worker and from
        // nvim reach the window by different channels, so one can arrive in a state that no longer
        // expects it.
        match (&self.state, event) {
            (LinkState::Connecting { addr }, AttachEvent::Connected { channel, .. }) => {
                self.state = LinkState::Attaching {
                    addr: addr.clone(),
                    waiting_for_key: false,
                };
                self.channel = Some(channel);
                Vec::new()
            }
            (LinkState::Attaching { addr, .. }, AttachEvent::Blocking { blocking, .. }) => {
                self.state = LinkState::Attaching {
                    addr: addr.clone(),
                    waiting_for_key: blocking,
                };
                Vec::new()
            }
            (LinkState::Attaching { addr, .. }, AttachEvent::Installed { report, .. }) => {
                self.state = LinkState::Attached {
                    addr: addr.clone(),
                    channel: self.channel.unwrap_or_default(),
                    nvim_pid: report.nvim_pid,
                    in_tmux: report.in_tmux,
                };
                Vec::new()
            }
            (
                LinkState::Connecting { addr } | LinkState::Attaching { addr, .. },
                AttachEvent::ConnectFailed { why, .. } | AttachEvent::InstallFailed { why, .. },
            ) => {
                self.state = LinkState::Failed {
                    addr: addr.clone(),
                    why,
                };
                vec![Effect::Close { gen }]
            }
            (LinkState::Connecting { addr } | LinkState::Attaching { addr, .. }, AttachEvent::Closed { .. }) => {
                self.state = LinkState::Failed {
                    addr: addr.clone(),
                    why: "the editor went away before attaching".to_owned(),
                };
                vec![Effect::Close { gen }]
            }
            // Another panel's install can be heard before our own answer is read; its glue is
            // then the other panel's, so there is nothing of ours to tear down.
            (LinkState::Connecting { .. } | LinkState::Attaching { .. }, AttachEvent::Replaced { .. }) => {
                self.state = LinkState::Detached { why: Detach::Replaced };
                vec![Effect::Close { gen }]
            }
            (LinkState::Attached { .. }, AttachEvent::Closed { .. }) => {
                self.state = LinkState::Detached {
                    why: Detach::EditorWentAway,
                };
                vec![Effect::CancelDraftEdits]
            }
            (LinkState::Attached { .. }, AttachEvent::Replaced { .. }) => {
                self.state = LinkState::Detached { why: Detach::Replaced };
                vec![Effect::Close { gen }, Effect::CancelDraftEdits]
            }
            _ => Vec::new(),
        }
    }

    /// Whether the panel is connecting to, installing into or attached to `addr` right now.
    fn follows(&self, addr: &std::path::Path) -> bool {
        match &self.state {
            LinkState::Connecting { addr: at }
            | LinkState::Attaching { addr: at, .. }
            | LinkState::Attached { addr: at, .. } => at == addr,
            LinkState::NoEditor | LinkState::Detached { .. } | LinkState::Failed { .. } => false,
        }
    }

    fn attach(&mut self, addr: PathBuf) -> Vec<Effect> {
        let old = self.gen;
        let mut effects = match self.state {
            LinkState::Attached { .. } => {
                vec![
                    Effect::ExplicitTeardown { gen: old },
                    Effect::Close { gen: old },
                    Effect::CancelDraftEdits,
                ]
            }
            // An install still queued in nvim for the old connection removes itself within its
            // liveness interval, so no explicit teardown is sent.
            LinkState::Connecting { .. } | LinkState::Attaching { .. } => vec![Effect::Close { gen: old }],
            _ => Vec::new(),
        };
        self.gen += 1;
        self.channel = None;
        self.state = LinkState::Connecting { addr: addr.clone() };
        effects.push(Effect::Connect { gen: self.gen, addr });
        effects
    }

    pub fn band(&self) -> BandLink {
        let (state, text) = match &self.state {
            LinkState::NoEditor => ("none", "no editor attached: run :EitriPanel in nvim".to_owned()),
            LinkState::Connecting { .. } => ("attaching", "attaching\u{2026}".to_owned()),
            LinkState::Attaching {
                waiting_for_key: false, ..
            } => ("attaching", "attaching\u{2026}".to_owned()),
            LinkState::Attaching {
                waiting_for_key: true, ..
            } => ("attaching", "attaching\u{2026} (nvim is waiting for a key)".to_owned()),
            LinkState::Attached { .. } => ("attached", String::new()),
            LinkState::Detached {
                why: Detach::EditorWentAway,
            } => (
                "detached",
                "editor detached: run :EitriPanel to attach again".to_owned(),
            ),
            LinkState::Detached { why: Detach::Replaced } => (
                "detached",
                "editor detached: another Eitri panel attached to it".to_owned(),
            ),
            LinkState::Failed { why, .. } => ("failed", format!("could not attach: {why}")),
        };
        BandLink { state, text }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(name: &str) -> PathBuf {
        PathBuf::from(format!("/r/{name}"))
    }

    fn report() -> InstallReport {
        InstallReport {
            installed: vec!["theme".into()],
            failed: vec![],
            nvim_pid: 77,
            in_tmux: true,
        }
    }

    /// A machine in `Attaching` for `/r/a`, generation 1, channel 5.
    fn attaching() -> Attacher {
        let (mut a, _) = Attacher::new(Some(addr("a")));
        a.handle(AttachEvent::Connected { gen: 1, channel: 5 });
        a
    }

    fn attached() -> Attacher {
        let mut a = attaching();
        a.handle(AttachEvent::Installed {
            gen: 1,
            report: report(),
        });
        a
    }

    fn is_attaching(a: &Attacher, waiting: bool) -> bool {
        *a.state()
            == LinkState::Attaching {
                addr: addr("a"),
                waiting_for_key: waiting,
            }
    }

    #[test]
    fn the_transition_table() {
        let (mut a, effects) = Attacher::new(None);
        assert_eq!((a.state(), effects), (&LinkState::NoEditor, vec![]));

        // any + Attach
        assert_eq!(
            a.handle(AttachEvent::Attach(addr("a"))),
            vec![Effect::Connect {
                gen: 1,
                addr: addr("a")
            }]
        );
        assert_eq!(*a.state(), LinkState::Connecting { addr: addr("a") });

        // Connecting + Connected
        assert_eq!(a.handle(AttachEvent::Connected { gen: 1, channel: 5 }), vec![]);
        assert!(is_attaching(&a, false));

        // Attaching + Blocking
        assert_eq!(a.handle(AttachEvent::Blocking { gen: 1, blocking: true }), vec![]);
        assert!(is_attaching(&a, true));
        assert_eq!(
            a.handle(AttachEvent::Blocking {
                gen: 1,
                blocking: false
            }),
            vec![]
        );
        assert!(is_attaching(&a, false));

        // Attaching + Installed
        assert_eq!(
            a.handle(AttachEvent::Installed {
                gen: 1,
                report: report()
            }),
            vec![]
        );
        assert_eq!(
            *a.state(),
            LinkState::Attached {
                addr: addr("a"),
                channel: 5,
                nvim_pid: 77,
                in_tmux: true
            }
        );

        // Attached + Closed
        assert_eq!(a.handle(AttachEvent::Closed { gen: 1 }), vec![Effect::CancelDraftEdits]);
        assert_eq!(
            *a.state(),
            LinkState::Detached {
                why: Detach::EditorWentAway
            }
        );

        // Attached + Replaced
        let mut a = attached();
        assert_eq!(
            a.handle(AttachEvent::Replaced { gen: 1 }),
            vec![Effect::Close { gen: 1 }, Effect::CancelDraftEdits]
        );
        assert_eq!(*a.state(), LinkState::Detached { why: Detach::Replaced });

        // Connecting/Attaching + a failure or Closed
        let failures = |gen| {
            [
                (
                    AttachEvent::ConnectFailed {
                        gen,
                        why: "refused".into(),
                    },
                    "refused",
                ),
                (
                    AttachEvent::InstallFailed {
                        gen,
                        why: "no lua".into(),
                    },
                    "no lua",
                ),
                (AttachEvent::Closed { gen }, "the editor went away before attaching"),
            ]
        };
        for (event, why) in failures(1) {
            let (mut connecting, _) = Attacher::new(Some(addr("a")));
            assert_eq!(connecting.handle(event.clone()), vec![Effect::Close { gen: 1 }]);
            assert_eq!(
                *connecting.state(),
                LinkState::Failed {
                    addr: addr("a"),
                    why: why.into()
                }
            );
            let mut attaching = attaching();
            assert_eq!(attaching.handle(event), vec![Effect::Close { gen: 1 }]);
            assert_eq!(
                *attaching.state(),
                LinkState::Failed {
                    addr: addr("a"),
                    why: why.into()
                }
            );
        }

        // Connecting/Attaching + Replaced: another panel's install is heard before our answer is read
        let (mut connecting, _) = Attacher::new(Some(addr("a")));
        assert_eq!(
            connecting.handle(AttachEvent::Replaced { gen: 1 }),
            vec![Effect::Close { gen: 1 }]
        );
        assert_eq!(*connecting.state(), LinkState::Detached { why: Detach::Replaced });
        let mut racing = attaching();
        assert_eq!(
            racing.handle(AttachEvent::Replaced { gen: 1 }),
            vec![Effect::Close { gen: 1 }]
        );
        assert_eq!(*racing.state(), LinkState::Detached { why: Detach::Replaced });
        // ... and the install answer that follows changes nothing.
        assert_eq!(
            racing.handle(AttachEvent::Installed {
                gen: 1,
                report: report()
            }),
            vec![]
        );
        assert_eq!(*racing.state(), LinkState::Detached { why: Detach::Replaced });
    }

    #[test]
    fn attach_from_every_state_that_is_not_live_only_connects() {
        for mut a in [
            Attacher::new(None).0,
            {
                let mut a = attached();
                a.handle(AttachEvent::Closed { gen: 1 });
                a
            },
            {
                let mut a = attaching();
                a.handle(AttachEvent::ConnectFailed {
                    gen: 1,
                    why: "x".into(),
                });
                a
            },
        ] {
            let before = a.gen();
            assert_eq!(
                a.handle(AttachEvent::Attach(addr("b"))),
                vec![Effect::Connect {
                    gen: before + 1,
                    addr: addr("b")
                }]
            );
            assert_eq!(*a.state(), LinkState::Connecting { addr: addr("b") });
        }
    }

    #[test]
    fn a_late_event_from_an_old_generation_changes_nothing() {
        let (mut a, _) = Attacher::new(Some(addr("a")));
        assert_eq!(
            a.handle(AttachEvent::Attach(addr("b"))),
            vec![
                Effect::Close { gen: 1 },
                Effect::Connect {
                    gen: 2,
                    addr: addr("b")
                }
            ]
        );
        let late = [
            AttachEvent::Installed {
                gen: 1,
                report: report(),
            },
            AttachEvent::Connected { gen: 1, channel: 9 },
            AttachEvent::Blocking { gen: 1, blocking: true },
            AttachEvent::InstallFailed {
                gen: 1,
                why: "x".into(),
            },
            AttachEvent::ConnectFailed {
                gen: 1,
                why: "x".into(),
            },
            AttachEvent::Closed { gen: 1 },
            AttachEvent::Replaced { gen: 1 },
        ];
        for event in late {
            assert_eq!(a.handle(event), vec![]);
            assert_eq!(*a.state(), LinkState::Connecting { addr: addr("b") });
        }
    }

    #[test]
    fn asking_for_the_editor_already_followed_changes_nothing() {
        // Attached: no teardown, no close, the drafts kept, the same generation.
        let mut a = attached();
        let before = a.state().clone();
        assert_eq!(a.handle(AttachEvent::Requested(addr("a"))), vec![]);
        assert_eq!(*a.state(), before);
        assert_eq!(a.gen(), 1);
        // Still connecting or installing: the attach under way goes on.
        let (mut a, _) = Attacher::new(Some(addr("a")));
        assert_eq!(a.handle(AttachEvent::Requested(addr("a"))), vec![]);
        assert_eq!(*a.state(), LinkState::Connecting { addr: addr("a") });
        let mut a = attaching();
        assert_eq!(a.handle(AttachEvent::Requested(addr("a"))), vec![]);
        assert_eq!(a.gen(), 1);
    }

    #[test]
    fn asking_for_another_editor_or_after_the_link_ended_attaches() {
        let mut a = attached();
        assert_eq!(
            a.handle(AttachEvent::Requested(addr("b"))),
            vec![
                Effect::ExplicitTeardown { gen: 1 },
                Effect::Close { gen: 1 },
                Effect::CancelDraftEdits,
                Effect::Connect {
                    gen: 2,
                    addr: addr("b")
                },
            ]
        );
        // The same address once the editor went away, or the attach failed, is a fresh connect.
        let mut a = attached();
        a.handle(AttachEvent::Closed { gen: 1 });
        assert_eq!(
            a.handle(AttachEvent::Requested(addr("a"))),
            vec![Effect::Connect {
                gen: 2,
                addr: addr("a")
            }]
        );
        let mut a = attaching();
        a.handle(AttachEvent::ConnectFailed {
            gen: 1,
            why: "x".into(),
        });
        assert_eq!(
            a.handle(AttachEvent::Requested(addr("a"))),
            vec![Effect::Connect {
                gen: 2,
                addr: addr("a")
            }]
        );
        // A forced attach to the same address still starts again (the driver's own re-attach).
        let mut a = attached();
        assert_eq!(
            a.handle(AttachEvent::Attach(addr("a"))),
            vec![
                Effect::ExplicitTeardown { gen: 1 },
                Effect::Close { gen: 1 },
                Effect::CancelDraftEdits,
                Effect::Connect {
                    gen: 2,
                    addr: addr("a")
                },
            ]
        );
    }

    #[test]
    fn retarget_from_attached_tears_down_closes_and_cancels_drafts_in_that_order() {
        let mut a = attached();
        assert_eq!(
            a.handle(AttachEvent::Attach(addr("b"))),
            vec![
                Effect::ExplicitTeardown { gen: 1 },
                Effect::Close { gen: 1 },
                Effect::CancelDraftEdits,
                Effect::Connect {
                    gen: 2,
                    addr: addr("b")
                },
            ]
        );
        // From Attaching: only the close, then the connect.
        let mut a = attaching();
        assert_eq!(
            a.handle(AttachEvent::Attach(addr("b"))),
            vec![
                Effect::Close { gen: 1 },
                Effect::Connect {
                    gen: 2,
                    addr: addr("b")
                }
            ]
        );
        // The same address again re-attaches: nvim may have quit and a new one taken its place.
        let mut a = attached();
        let effects = a.handle(AttachEvent::Attach(addr("a")));
        assert_eq!(
            effects.last(),
            Some(&Effect::Connect {
                gen: 2,
                addr: addr("a")
            })
        );
        assert_eq!(effects.len(), 4);
    }

    #[test]
    fn an_event_that_does_not_apply_to_the_state_changes_nothing() {
        let blocking = AttachEvent::Blocking { gen: 1, blocking: true };
        let installed = AttachEvent::Installed {
            gen: 1,
            report: report(),
        };
        let failed = AttachEvent::ConnectFailed {
            gen: 1,
            why: "x".into(),
        };
        let connected = AttachEvent::Connected { gen: 1, channel: 5 };

        let (mut connecting, _) = Attacher::new(Some(addr("a")));
        for event in [blocking.clone(), installed.clone()] {
            assert_eq!(connecting.handle(event), vec![]);
            assert_eq!(*connecting.state(), LinkState::Connecting { addr: addr("a") });
        }

        let mut live = attached();
        for event in [blocking.clone(), installed.clone(), failed.clone(), connected.clone()] {
            assert_eq!(live.handle(event), vec![]);
            assert_eq!(
                *live.state(),
                LinkState::Attached {
                    addr: addr("a"),
                    channel: 5,
                    nvim_pid: 77,
                    in_tmux: true
                }
            );
        }

        let mut attaching = attaching();
        assert_eq!(attaching.handle(connected.clone()), vec![]);
        assert!(is_attaching(&attaching, false));

        let mut detached = attached();
        detached.handle(AttachEvent::Closed { gen: 1 });
        let (mut none, _) = Attacher::new(None);
        let mut failed_state = attaching_failed();
        for machine in [&mut detached, &mut none, &mut failed_state] {
            let before = machine.state().clone();
            for event in [blocking.clone(), installed.clone(), failed.clone(), connected.clone()] {
                assert_eq!(machine.handle(event), vec![]);
            }
            assert_eq!(*machine.state(), before);
        }
        // Even a current-generation Closed and Replaced do nothing once detached.
        for event in [AttachEvent::Closed { gen: 1 }, AttachEvent::Replaced { gen: 1 }] {
            assert_eq!(detached.handle(event), vec![]);
        }
    }

    fn attaching_failed() -> Attacher {
        let mut a = attaching();
        a.handle(AttachEvent::InstallFailed {
            gen: 1,
            why: "x".into(),
        });
        a
    }

    #[test]
    fn band_texts() {
        let band = |a: &Attacher| {
            let b = a.band();
            (b.state, b.text)
        };
        let (none, _) = Attacher::new(None);
        assert_eq!(
            band(&none),
            ("none", "no editor attached: run :EitriPanel in nvim".to_owned())
        );
        let (connecting, _) = Attacher::new(Some(addr("a")));
        assert_eq!(band(&connecting), ("attaching", "attaching\u{2026}".to_owned()));
        let mut a = attaching();
        assert_eq!(band(&a), ("attaching", "attaching\u{2026}".to_owned()));
        a.handle(AttachEvent::Blocking { gen: 1, blocking: true });
        assert_eq!(
            band(&a),
            ("attaching", "attaching\u{2026} (nvim is waiting for a key)".to_owned())
        );
        assert_eq!(band(&attached()), ("attached", String::new()));
        let mut gone = attached();
        gone.handle(AttachEvent::Closed { gen: 1 });
        assert_eq!(
            band(&gone),
            (
                "detached",
                "editor detached: run :EitriPanel to attach again".to_owned()
            )
        );
        let mut replaced = attached();
        replaced.handle(AttachEvent::Replaced { gen: 1 });
        assert_eq!(
            band(&replaced),
            (
                "detached",
                "editor detached: another Eitri panel attached to it".to_owned()
            )
        );
        let mut failed = attaching();
        failed.handle(AttachEvent::ConnectFailed {
            gen: 1,
            why: "refused".into(),
        });
        assert_eq!(band(&failed), ("failed", "could not attach: refused".to_owned()));
        let mut went = attaching();
        went.handle(AttachEvent::Closed { gen: 1 });
        assert_eq!(
            band(&went),
            (
                "failed",
                "could not attach: the editor went away before attaching".to_owned()
            )
        );
    }

    #[test]
    fn the_band_serializes_for_the_panel() {
        let json = serde_json::to_string(&attached().band()).unwrap();
        assert_eq!(json, r#"{"state":"attached","text":""}"#);
    }
}
