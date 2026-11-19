use std::cell::RefCell;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use super::*;
use crate::editor_record::{Record, RecordFile};
use crate::editor_reopen_start::tests::{one_file, root, Place};
use crate::editor_reopen_start::{prepare_start, Prepared};
use crate::layout::{Layout, ModuleDecl, ModuleId, ModuleKeys, Placement, Workspaces};
use crate::pane_switch::AsideWord;
use crate::wm::fake::{FakeHost, FakeScheduler, Logged};
use crate::wm::start::StartStep;
use crate::wm::toasts::ToastKind;
use crate::wm::*;

const WIN: WinId = WinId::MAIN;

/// A window walked through its start with `prepared` as its reopen step, over the fake host and clock.
struct Rig {
    driver: Rc<Driver>,
    host: Rc<FakeHost>,
    scheduler: Rc<FakeScheduler>,
    writer: Option<RecordWriter>,
    token: String,
    channel: Option<PathBuf>,
    /// While `Some`, the handoff reads asked are kept here instead of being handed to the writer.
    held_reads: Rc<RefCell<Option<Vec<(String, u64)>>>>,
    /// nvim's input as it stood on disk each time a restart was carried out.
    input_at_restart: Rc<RefCell<Vec<Option<serde_json::Value>>>>,
}

fn spawn() -> EditorSpawn {
    EditorSpawn {
        env: vec![],
        args: vec![],
        cwd: None,
        clean: true,
        background: None,
        open: vec![],
    }
}

impl Rig {
    /// The start walked up to the window's showing.
    fn until_shown(prepared: Prepared) -> Rig {
        let host = FakeHost::new();
        let scheduler = FakeScheduler::new();
        let driver = Driver::new(Controller::new(scheduler.clone()), host.clone(), scheduler.clone());
        let sink: Rc<dyn HostSink> = driver.clone();
        host.attach(Rc::downgrade(&sink));
        scheduler.attach(Rc::downgrade(&sink));
        let writer = prepared.writer.clone();
        let token = prepared.token.clone().unwrap_or_default();
        let channel = prepared.channel_dir.clone();
        let held_reads: Rc<RefCell<Option<Vec<(String, u64)>>>> = Rc::default();
        let input_at_restart: Rc<RefCell<Vec<Option<serde_json::Value>>>> = Rc::default();
        // The record's ops carried out as the shell's services carry them out.
        {
            let (writer, channel) = (writer.clone(), channel.clone());
            let (held_reads, input_at_restart) = (held_reads.clone(), input_at_restart.clone());
            host.on_apply(move |_, _, op| match op {
                HostOp::Editor(EditorCall::ReadHandoff { token, seq, .. }) => {
                    if let Some(held) = held_reads.borrow_mut().as_mut() {
                        held.push((token.clone(), *seq));
                        return;
                    }
                    if let (Some(writer), Some(channel)) = (&writer, &channel) {
                        crate::editor_reopen_start::read_handoff(writer, channel, token, *seq);
                    }
                }
                HostOp::Cleanup(CleanupOp::EditorRecord { token, seq }) => {
                    if let (Some(writer), Some(channel)) = (&writer, &channel) {
                        crate::editor_reopen_start::close_record(writer, channel, token.as_deref(), *seq);
                    }
                }
                HostOp::Editor(EditorCall::DropInput { .. }) => {
                    if let Some(channel) = &channel {
                        crate::editor_reopen_start::drop_input(channel);
                    }
                }
                HostOp::Editor(EditorCall::Restart { .. }) => {
                    let input = channel
                        .as_ref()
                        .and_then(|channel| std::fs::read_to_string(channel.join(crate::editor_reopen::REOPEN_IN)).ok())
                        .map(|text| serde_json::from_str(&text).unwrap());
                    input_at_restart.borrow_mut().push(input);
                }
                _ => {}
            });
        }
        let keymap = Rc::new(crate::keymap::Keymap::defaults());
        let module_keys = Rc::new(ModuleKeys::build(&[], &keymap).unwrap());
        let layout = Layout::initial(&[ModuleDecl {
            id: ModuleId::terminal(),
            placement: Placement::BelowRoot,
        }])
        .unwrap();
        let decls = layout.modules().iter().map(crate::wm::verbs::rig::decl).collect();
        for step in [
            StartStep::Feeds { spawn: spawn() },
            StartStep::Window {
                spec: WindowSpec {
                    role: WindowRole::Main,
                    presence: EditorPresence::Owned,
                    title: "project".into(),
                },
            },
            StartStep::Editor,
            StartStep::Kernel,
            StartStep::Config { keymap, module_keys },
            StartStep::Layout {
                parts: WindowParts::new(layout, Workspaces::single(), None),
                own_window: None,
                tiling: Tiling::Eitri,
            },
            StartStep::Reopen { prepared },
            StartStep::Surfaces { decls },
            StartStep::Chrome,
        ] {
            driver.start(WIN, step);
        }
        Rig {
            driver,
            host,
            scheduler,
            writer,
            token,
            channel,
            held_reads,
            input_at_restart,
        }
    }

    /// The window is shown and its start ends.
    fn present(&self) {
        let editor = ModuleId::editor();
        self.host
            .with_surface(WIN, editor.clone(), crate::wm::verbs::rig::decl(&editor).kind, true);
        self.driver.start(
            WIN,
            StartStep::Present {
                activation: None,
                first: editor,
                arrive: None,
            },
        );
        self.driver.start(WIN, StartStep::Requests);
    }

    fn start(prepared: Prepared) -> Rig {
        let rig = Rig::until_shown(prepared);
        rig.present();
        rig
    }

    /// A window whose start found `record` on disk, or nothing, with the marker set when `marker`.
    fn with(place: &Place, record: Option<&Record>, marker: bool) -> Rig {
        if let Some(record) = record {
            place.file().write(record).unwrap();
        }
        if marker {
            place.file().set_marker("fedcba9876543210").unwrap();
        }
        Rig::start(prepare_start(&place.start()))
    }

    fn writer(&self) -> &RecordWriter {
        self.writer.as_ref().expect("a writer")
    }

    fn flush(&self) {
        assert!(self.writer().flush(Duration::from_secs(10)));
    }

    fn line(&self, line: ReopenLine) {
        self.host
            .post(HostEvent::Editor(EditorEvent::Reopen { win: WIN, line }));
    }

    fn began(&self) {
        self.line(ReopenLine::Began {
            token: self.token.clone(),
        });
    }

    fn ended(&self, gone: u32, failed: u32, squeezed: u32) {
        self.line(ReopenLine::Ended {
            token: self.token.clone(),
            opened: 3,
            gone,
            failed,
            squeezed,
            micros: 1234,
        });
    }

    fn name(&self, gone: bool, name: &str) {
        self.line(ReopenLine::Name {
            token: self.token.clone(),
            gone,
            name: name.into(),
        });
    }

    fn answer(&self, reply: Result<Option<(String, bool)>, String>) {
        self.host.post(HostEvent::Editor(EditorEvent::Captured {
            win: WIN,
            token: self.token.clone(),
            reply,
        }));
    }

    fn captures(&self) -> Vec<(String, rmpv::Value)> {
        self.host
            .log()
            .into_iter()
            .filter_map(|logged| match logged {
                Logged::Op(HostOp::Editor(EditorCall::Capture { token, args, .. }), _) => Some((token, args)),
                _ => None,
            })
            .collect()
    }

    fn state(&self) -> Option<TokenState> {
        self.driver
            .query(|c| c.window(WIN).and_then(|w| w.reopen.as_ref().map(|r| r.state)))
    }

    fn toast(&self) -> Option<(String, ToastKind)> {
        self.driver
            .query(|c| c.chrome(WIN).toast.map(|toast| (toast.text, toast.kind)))
    }

    fn notes(&self) -> Vec<String> {
        self.driver.query(|c| c.notes(WIN))
    }

    fn marker(&self, place: &Place) -> Option<String> {
        self.flush();
        place.file().marker()
    }

    /// nvim says it is quitting on purpose (its watch's `VimLeavePre`, which a deadly signal or a hang never runs).
    fn leaving(&self) {
        self.host.post(HostEvent::Editor(EditorEvent::Leaving {
            win: WIN,
            filetype: String::new(),
        }));
    }

    /// nvim ends with nobody asking: a crash when `crashed`, else a `:qa`.
    fn exits(&self, crashed: bool) {
        if crashed {
            self.host.post(HostEvent::Editor(EditorEvent::Watched { win: WIN }));
        }
        self.host.set_editor_facts(EditorFacts {
            exited: true,
            ..Default::default()
        });
        self.host.post(HostEvent::Editor(EditorEvent::Exited {
            win: WIN,
            settled: None,
        }));
    }

    /// The ended editor's pane is let go and the editor's key shows it, which starts nvim again.
    fn restart(&self) {
        self.scheduler.advance(Duration::ZERO);
        self.host.post(HostEvent::Editor(EditorEvent::Released { win: WIN }));
        self.driver.run(
            WIN,
            crate::keymap::Action::Module(ModuleId::editor()),
            Origin::Key { keycode: 1 },
        );
    }

    /// What the writer was handed is done and its answers are heard.
    fn settle(&self) {
        if self.writer.is_some() {
            self.flush();
        }
        self.scheduler.advance(Duration::ZERO);
    }

    /// From now on, the handoff reads asked wait for [`Rig::carry_reads`].
    fn hold_reads(&self) {
        *self.held_reads.borrow_mut() = Some(Vec::new());
    }

    /// The handoff reads held so far are handed to the writer, and later ones go at once.
    fn carry_reads(&self) {
        let held = self.held_reads.borrow_mut().take().unwrap_or_default();
        for (token, seq) in held {
            crate::editor_reopen_start::read_handoff(self.writer(), self.channel.as_ref().unwrap(), &token, seq);
        }
    }

    /// The handoff reads asked, as (token, sequence number).
    fn reads(&self) -> Vec<(String, u64)> {
        self.host
            .log()
            .into_iter()
            .filter_map(|logged| match logged {
                Logged::Op(HostOp::Editor(EditorCall::ReadHandoff { token, seq, .. }), _) => Some((token, seq)),
                _ => None,
            })
            .collect()
    }

    fn records(&self) -> Vec<CleanupOp> {
        self.host
            .log()
            .into_iter()
            .filter_map(|logged| match logged {
                Logged::Op(HostOp::Cleanup(op @ CleanupOp::EditorRecord { .. }), _) => Some(op),
                _ => None,
            })
            .collect()
    }

    fn restarts(&self) -> usize {
        self.input_at_restart.borrow().len()
    }

    /// The last record the nvim of `token` leaves at its exit, as the snippet leaves it.
    fn hand_off(&self, token: &str, record: &Record, same: bool) {
        let path = self
            .channel
            .as_ref()
            .unwrap()
            .join(crate::editor_reopen::handoff_name(token));
        let json = format!("{{\"same\":{same},\"record\":{}}}", record.to_json());
        agent::private_fs::write_private(&path, json.as_bytes()).unwrap();
    }

    fn reopen<T: 'static>(&self, f: impl FnOnce(&Reopen) -> T) -> T {
        self.driver
            .query(|c| c.window(WIN).map(|w| f(w.reopen.as_ref().expect("the window reopens"))))
            .unwrap()
    }
}

fn record_json(record: &Record) -> serde_json::Value {
    serde_json::from_str(&record.to_json()).unwrap()
}

fn json(record: &Record) -> Option<(String, bool)> {
    Some((record.to_json(), false))
}

#[test]
fn a_set_aside_record_toasts_at_present() {
    let place = Place::new("aside-toast");
    let file: RecordFile = place.file();
    crate::state_paths::create_state_dir(file.path().parent().unwrap()).unwrap();
    agent::private_fs::write_private(file.path(), b"not json").unwrap();
    let prepared = prepare_start(&place.start());
    let text = crate::wm::feedback::reopen_set_aside_text();
    assert_eq!(prepared.toast.as_deref(), Some(text.as_str()));
    let rig = Rig::until_shown(prepared);
    assert_eq!(rig.toast(), None, "held until the window is shown");
    rig.present();
    assert_eq!(rig.toast(), Some((text, ToastKind::Other)));

    // A marker's line is a warning.
    let place = Place::new("marker-toast");
    let rig = Rig::with(&place, Some(&one_file("/home/someone/project/a.rs")), true);
    assert_eq!(
        rig.toast(),
        Some((crate::wm::feedback::reopen_unfinished_text(), ToastKind::Warning))
    );
}

#[test]
fn lines_move_the_token_state_only_forward() {
    let place = Place::new("forward");
    let rig = Rig::with(&place, None, false);
    assert_eq!(rig.state(), Some(TokenState::Started));

    // An `O` before any `B` is dropped.
    rig.ended(0, 0, 0);
    assert_eq!(rig.state(), Some(TokenState::Started));
    // A stale token's line is dropped.
    rig.line(ReopenLine::Began {
        token: "0000000000000000".into(),
    });
    assert_eq!(rig.state(), Some(TokenState::Started));
    assert!(rig
        .notes()
        .contains(&"[editor] reopen: dropped B for an earlier nvim".to_string()));

    rig.began();
    assert_eq!(rig.state(), Some(TokenState::Began));
    // An `A` after `B` is dropped.
    rig.line(ReopenLine::Aside {
        token: rig.token.clone(),
        word: AsideWord::Args,
    });
    assert_eq!(rig.state(), Some(TokenState::Began));
    rig.ended(0, 0, 0);
    assert_eq!(rig.state(), Some(TokenState::Ended));
    // A `B` after `O`, and a second `O`, are dropped.
    let notes = rig.notes().len();
    rig.began();
    rig.ended(1, 0, 0);
    assert_eq!(rig.state(), Some(TokenState::Ended));
    let dropped: Vec<String> = rig.notes()[notes..].to_vec();
    assert_eq!(
        dropped,
        vec![
            "[editor] reopen: dropped B while the restore is Ended".to_string(),
            "[editor] reopen: dropped O while the restore is Ended".to_string(),
        ]
    );
    assert_eq!(rig.captures().len(), 1, "the second O asked for no capture");
    // The `B` after `O` set no marker.
    assert_eq!(
        rig.marker(&place),
        Some(rig.token.clone()),
        "the one marker is the first B's"
    );
}

#[test]
fn ended_captures_at_once_and_its_answer_clears_the_marker() {
    let place = Place::new("ended");
    let rig = Rig::with(&place, None, false);
    assert_eq!(rig.marker(&place), None);
    rig.began();
    assert_eq!(
        rig.marker(&place),
        Some(rig.token.clone()),
        "B writes the marker with this nvim's token"
    );
    rig.ended(0, 0, 0);
    let captures = rig.captures();
    assert_eq!(captures.len(), 1, "one capture at once");
    let (token, args) = &captures[0];
    assert_eq!(*token, rig.token);
    let rmpv::Value::Map(args) = args else {
        panic!("the capture's arguments are one map: {args:?}")
    };
    let get = |key: &str| {
        args.iter()
            .find(|(k, _)| k.as_str() == Some(key))
            .map(|(_, v)| v.clone())
            .unwrap()
    };
    assert_eq!(get("token").as_str(), Some(rig.token.as_str()));
    assert_eq!(
        get("root").as_str(),
        Some(crate::editor_record::encode_name(root().as_os_str().as_bytes()).as_str())
    );
    assert_eq!(get("prefixes").as_array().unwrap().len(), 2);
    assert_eq!(
        rig.marker(&place),
        Some(rig.token.clone()),
        "the O alone does not clear it"
    );

    // The restore's own record answers: nothing is written, and the marker goes.
    let restored = one_file("/home/someone/project/a.rs");
    rig.answer(Ok(Some((restored.to_json(), true))));
    assert_eq!(rig.marker(&place), None);
    assert!(!place.file().path().exists(), "the restore's own record is not written");
}

#[test]
fn a_skip_session_never_clears_the_marker_on_b_or_o() {
    let place = Place::new("skip");
    let rig = Rig::with(&place, Some(&one_file("/home/someone/project/a.rs")), true);
    rig.began();
    rig.ended(0, 0, 0);
    assert_eq!(rig.captures().len(), 1);
    rig.answer(Ok(Some((one_file("/home/someone/project/a.rs").to_json(), true))));
    assert_eq!(
        rig.marker(&place).as_deref(),
        Some("fedcba9876543210"),
        "neither the lines nor an answer that writes nothing touch the old marker"
    );
    // A record that is written lets it go.
    rig.scheduler.advance(CAPTURE_EVERY);
    assert_eq!(rig.captures().len(), 2);
    rig.answer(Ok(json(&one_file("/home/someone/project/b.rs"))));
    assert_eq!(rig.marker(&place), None);
}

#[test]
fn no_capture_before_ended_and_none_after_aside() {
    let place = Place::new("aside");
    let rig = Rig::with(&place, None, false);
    rig.host.post(HostEvent::Active {
        win: WIN,
        active: false,
    });
    rig.scheduler.advance(CAPTURE_EVERY * 3);
    rig.began();
    rig.host.post(HostEvent::Active { win: WIN, active: true });
    rig.host.post(HostEvent::Active {
        win: WIN,
        active: false,
    });
    rig.scheduler.advance(CAPTURE_EVERY * 3);
    assert!(rig.captures().is_empty(), "nothing is captured before the restore ends");

    let place = Place::new("aside2");
    let rig = Rig::with(&place, None, false);
    rig.line(ReopenLine::Aside {
        token: rig.token.clone(),
        word: AsideWord::Persistence,
    });
    assert_eq!(rig.state(), Some(TokenState::Aside));
    assert!(rig
        .notes()
        .contains(&"[editor] reopen: standing aside for persistence".to_string()));
    rig.ended(0, 0, 0);
    rig.host.post(HostEvent::Active {
        win: WIN,
        active: false,
    });
    rig.scheduler.advance(CAPTURE_EVERY * 3);
    assert!(
        rig.captures().is_empty(),
        "nothing is captured for an nvim that stood aside"
    );
    assert_eq!(rig.toast(), None, "standing aside says nothing");
}

#[test]
fn one_capture_out_at_a_time_and_every_thirty_seconds() {
    let place = Place::new("timed");
    let rig = Rig::with(&place, None, false);
    rig.began();
    rig.ended(0, 0, 0);
    assert_eq!(rig.captures().len(), 1);
    // Not answered: nothing more, however long, and focus asks for nothing either.
    rig.scheduler.advance(CAPTURE_EVERY * 4);
    rig.host.post(HostEvent::Active {
        win: WIN,
        active: false,
    });
    assert_eq!(rig.captures().len(), 1, "one capture out at a time");

    // Its answer, even nothing, arms the next one 30 s from then.
    rig.answer(Ok(None));
    rig.scheduler.advance(CAPTURE_EVERY - Duration::from_secs(1));
    assert_eq!(rig.captures().len(), 1);
    rig.scheduler.advance(Duration::from_secs(1));
    assert_eq!(rig.captures().len(), 2);
    // A failed call arms it too.
    rig.answer(Err("nvim is at a prompt".into()));
    rig.scheduler.advance(CAPTURE_EVERY);
    assert_eq!(rig.captures().len(), 3);
    // Each capture asked takes the next sequence number.
    assert_eq!(
        rig.driver
            .query(|c| c.window(WIN).map(|w| w.reopen.as_ref().unwrap().seq)),
        Some(3)
    );
}

#[test]
fn focus_out_captures_at_most_once_per_five_seconds() {
    let place = Place::new("focus");
    let rig = Rig::with(&place, None, false);
    rig.began();
    rig.ended(0, 0, 0);
    rig.answer(Ok(None));
    assert_eq!(rig.captures().len(), 1);

    rig.host.post(HostEvent::Active {
        win: WIN,
        active: false,
    });
    assert_eq!(rig.captures().len(), 2, "losing the keyboard captures");
    rig.answer(Ok(None));
    rig.scheduler.advance(Duration::from_secs(2));
    rig.host.post(HostEvent::Active { win: WIN, active: true });
    rig.host.post(HostEvent::Active {
        win: WIN,
        active: false,
    });
    assert_eq!(rig.captures().len(), 2, "not again within five seconds");
    rig.scheduler.advance(Duration::from_secs(3));
    rig.host.post(HostEvent::Active { win: WIN, active: true });
    rig.host.post(HostEvent::Active {
        win: WIN,
        active: false,
    });
    assert_eq!(rig.captures().len(), 3, "again after five");
    // The timed capture counts from the last answer, not from the focus.
    rig.answer(Ok(None));
    rig.scheduler.advance(CAPTURE_EVERY - Duration::from_secs(1));
    assert_eq!(rig.captures().len(), 3);
    rig.scheduler.advance(Duration::from_secs(1));
    assert_eq!(rig.captures().len(), 4);
}

#[test]
fn a_captured_json_that_editor_record_refuses_is_logged_not_offered() {
    let place = Place::new("refused");
    let seed = one_file("/home/someone/project/a.rs");
    let rig = Rig::with(&place, Some(&seed), false);
    rig.began();
    rig.ended(0, 0, 0);
    let mut other_root = one_file("/elsewhere/a.rs");
    other_root.root = b"/elsewhere".to_vec();
    for json in ["{\"v\":1, nope".to_string(), other_root.to_json()] {
        let before = rig.notes().len();
        rig.answer(Ok(Some((json, false))));
        assert!(
            rig.notes()[before..]
                .iter()
                .any(|line| line.starts_with("[editor] reopen: the capture's record was refused: ")),
            "{:?}",
            rig.notes()
        );
        rig.flush();
        assert_eq!(rig.writer().newest(), Some(seed.clone()), "nothing was offered");
        rig.scheduler.advance(CAPTURE_EVERY);
    }
    assert_eq!(std::fs::read_to_string(place.file().path()).unwrap(), seed.to_json());
}

#[test]
fn gone_failed_and_squeezed_toasts_use_the_exact_words() {
    use crate::wm::feedback::{
        reopen_failed_count_text, reopen_failed_text, reopen_gone_count_text, reopen_gone_text, reopen_squeezed_text,
    };
    assert_eq!(reopen_gone_count_text(2), "2 files from last time are gone");
    assert_eq!(reopen_gone_count_text(1), "1 file from last time is gone");
    assert_eq!(reopen_failed_count_text(2), "could not reopen 2 files");
    assert_eq!(reopen_failed_count_text(1), "could not reopen 1 file");
    assert_eq!(
        reopen_gone_text(2, "src/old.rs"),
        "2 files from last time are gone: src/old.rs and 1 more"
    );
    assert_eq!(
        reopen_gone_text(1, "src/old.rs"),
        "1 file from last time is gone: src/old.rs"
    );
    assert_eq!(reopen_failed_text("src/x.rs"), "could not reopen src/x.rs");
    assert_eq!(
        reopen_squeezed_text(3),
        "3 files from last time did not fit on screen; they are in :ls"
    );
    assert_eq!(
        reopen_squeezed_text(1),
        "1 file from last time did not fit on screen; it is in :ls"
    );
    assert_eq!(
        crate::wm::feedback::reopen_set_aside_text(),
        "the editor's last files could not be read; the file was set aside"
    );
    assert_eq!(
        crate::wm::feedback::reopen_unfinished_text(),
        "the editor's last files were not reopened: the last try did not finish; editor.restore = false turns \
         reopening off"
    );

    let place = Place::new("toasts");
    let rig = Rig::with(&place, None, false);
    rig.began();
    rig.ended(2, 1, 3);
    assert_eq!(
        rig.toast(),
        Some((
            "2 files from last time are gone; could not reopen 1 file; 3 files from last time did not fit on screen; \
             they are in :ls"
                .to_string(),
            ToastKind::Other
        )),
        "everything counted is said at once, the names when they come"
    );
    rig.name(true, "src/old.rs");
    rig.name(false, "src/x.rs");
    // A second name of a kind is dropped.
    rig.name(true, "src/other.rs");
    assert_eq!(
        rig.toast(),
        Some((
            "2 files from last time are gone: src/old.rs and 1 more; could not reopen src/x.rs; 3 files from last \
             time did not fit on screen; they are in :ls"
                .to_string(),
            ToastKind::Other
        ))
    );

    // A name for a kind nothing was counted under is dropped.
    let place = Place::new("toasts2");
    let rig = Rig::with(&place, None, false);
    rig.began();
    rig.ended(1, 0, 0);
    let unnamed = Some((reopen_gone_count_text(1), ToastKind::Other));
    assert_eq!(
        rig.toast(),
        unnamed,
        "a name that never comes costs the toast only its name"
    );
    rig.name(false, "src/x.rs");
    assert_eq!(rig.toast(), unnamed);
    rig.name(true, "src/old.rs");
    assert_eq!(
        rig.toast(),
        Some((
            "1 file from last time is gone: src/old.rs".to_string(),
            ToastKind::Other
        ))
    );
}

#[test]
fn everything_back_says_nothing() {
    let place = Place::new("quiet");
    let rig = Rig::with(&place, Some(&one_file("/home/someone/project/a.rs")), false);
    rig.began();
    rig.ended(0, 0, 0);
    rig.answer(Ok(Some((one_file("/home/someone/project/a.rs").to_json(), true))));
    assert_eq!(rig.toast(), None);
}

#[test]
fn the_writer_wakes_the_controller_through_the_scheduler_waker() {
    let place = Place::new("wake");
    let rig = Rig::with(&place, None, false);
    rig.began();
    rig.ended(0, 0, 0);
    let record = one_file("/home/someone/project/b.rs");
    rig.answer(Ok(json(&record)));
    rig.flush();
    assert_eq!(
        std::fs::read_to_string(place.file().path()).unwrap(),
        record.to_json(),
        "the capture's record was handed to the writer and written"
    );
    let taken = || {
        rig.driver
            .query(|c| c.window(WIN).map(|w| w.reopen.as_ref().unwrap().answered.clone()))
    };
    assert_eq!(taken(), Some(vec![]), "nothing taken before the wake is heard");
    // The fake scheduler posts the waker's wakes on its next turn.
    rig.scheduler.advance(Duration::ZERO);
    assert_eq!(taken(), Some(vec![WriterAnswer::Written { seq: 1 }]));
    assert!(rig.writer().take_answers().is_empty(), "the controller took them");
}

#[test]
fn an_unrequested_exit_asks_the_handoff_once() {
    // A crash: read at the exit, and the restart that follows does not ask again.
    let place = Place::new("exit-once");
    let rig = Rig::with(&place, None, false);
    rig.began();
    rig.ended(0, 0, 0);
    rig.exits(true);
    assert_eq!(
        rig.reads(),
        vec![(rig.token.clone(), 2)],
        "after the one capture's number"
    );
    rig.restart();
    rig.settle();
    rig.settle();
    assert_eq!(rig.reads().len(), 1, "the restart heard it already");
    assert_eq!(rig.restarts(), 1);

    // A `:qa`: read at the exit, and the close that follows only waits for the writer.
    let place = Place::new("exit-once-qa");
    let rig = Rig::with(&place, None, false);
    rig.began();
    rig.ended(0, 0, 0);
    rig.exits(false);
    rig.scheduler.advance(Duration::ZERO);
    assert_eq!(rig.reads(), vec![(rig.token.clone(), 2)]);
    assert_eq!(rig.records(), vec![CleanupOp::EditorRecord { token: None, seq: 2 }]);
}

#[test]
fn a_restart_waits_for_the_handoff_then_the_input_then_restarts() {
    let place = Place::new("restart-waits");
    let start = one_file("/home/someone/project/a.rs");
    let rig = Rig::with(&place, Some(&start), false);
    let first = rig.token.clone();
    rig.began();
    rig.ended(0, 0, 0);
    let timed = one_file("/home/someone/project/b.rs");
    rig.answer(Ok(json(&timed)));
    rig.flush();
    // nvim left a newer record than the timed one, and its read is slow to come.
    let handed = one_file("/home/someone/project/c.rs");
    rig.hand_off(&first, &handed, false);
    rig.hold_reads();
    rig.exits(true);
    rig.restart();
    assert_eq!(rig.restarts(), 0, "held for the ended nvim's last record");
    rig.scheduler.advance(RESTART_HOLD - Duration::from_millis(100));
    assert_eq!(rig.restarts(), 0);

    rig.carry_reads();
    rig.settle();
    let second = rig.reopen(|r| r.token.clone());
    assert_ne!(second, first, "a new nvim, a new token");
    assert_eq!(rig.reopen(|r| r.state), TokenState::Started);
    rig.settle();
    assert_eq!(rig.restarts(), 1, "the restart went once the input was written");
    let input = rig.input_at_restart.borrow()[0]
        .clone()
        .expect("the input was on disk when nvim started");
    assert_eq!(input["token"], second.as_str());
    assert_eq!(input["skip"], false);
    assert_eq!(
        input["record"],
        record_json(&handed),
        "the handoff is newer than the timed record"
    );
    assert_eq!(std::fs::read_to_string(place.file().path()).unwrap(), handed.to_json());
    // The hold's bound passing later restarts nothing more.
    rig.scheduler.advance(RESTART_HOLD * 2);
    assert_eq!(rig.restarts(), 1);
}

#[test]
fn a_restart_that_hears_nothing_goes_after_its_bound() {
    let place = Place::new("restart-bound");
    let start = one_file("/home/someone/project/a.rs");
    let rig = Rig::with(&place, Some(&start), false);
    rig.began();
    rig.ended(0, 0, 0);
    rig.hold_reads();
    rig.exits(true);
    rig.restart();
    rig.scheduler.advance(RESTART_HOLD - Duration::from_millis(1));
    assert_eq!(rig.restarts(), 0);
    rig.scheduler.advance(Duration::from_millis(1));
    rig.settle();
    assert_eq!(
        rig.restarts(),
        1,
        "the writer never answered the read, and the restart went at its bound"
    );
    let input = rig.input_at_restart.borrow()[0].clone().expect("an input");
    assert_eq!(input["token"], rig.reopen(|r| r.token.clone()).as_str());
    assert_eq!(
        input["record"],
        record_json(&start),
        "the newest record the window holds"
    );
}

#[test]
fn a_restart_before_any_capture_restores_the_start_record() {
    let place = Place::new("restart-start-record");
    let start = one_file("/home/someone/project/a.rs");
    let rig = Rig::with(&place, Some(&start), false);
    rig.began();
    rig.ended(0, 0, 0);
    // The capture asked at the restore's end never answers, and nvim leaves nothing.
    rig.exits(true);
    rig.restart();
    rig.settle();
    rig.settle();
    assert_eq!(rig.restarts(), 1);
    let input = rig.input_at_restart.borrow()[0].clone().expect("an input");
    assert_ne!(input["token"], rig.token.as_str(), "the restarted nvim's own input");
    assert_eq!(input["skip"], false);
    assert_eq!(input["record"], record_json(&start));
    assert_eq!(std::fs::read_to_string(place.file().path()).unwrap(), start.to_json());
}

#[test]
fn a_restart_after_a_restore_that_never_ended_skips() {
    let place = Place::new("restart-unfinished");
    let start = one_file("/home/someone/project/a.rs");
    let rig = Rig::with(&place, Some(&start), false);
    rig.began();
    // nvim dies inside its restore: no `O`.
    rig.exits(true);
    rig.restart();
    rig.settle();
    rig.settle();
    assert_eq!(rig.restarts(), 1);
    let input = rig.input_at_restart.borrow()[0].clone().expect("an input");
    assert_eq!(input["skip"], true);
    assert!(rig.reopen(|r| r.skip));
    assert_eq!(
        rig.marker(&place),
        Some(rig.token.clone()),
        "the marker stays until a record is written"
    );
    let (text, kind) = rig.toast().expect("a toast");
    assert_eq!(kind, ToastKind::Warning);
    assert!(
        text.ends_with(&crate::wm::feedback::reopen_unfinished_text()),
        "said after the crash's own line: {text}"
    );

    // A start that found the marker, and wrote nothing before nvim ended: the restart skips too.
    let place = Place::new("restart-unfinished-start");
    let rig = Rig::with(&place, Some(&start), true);
    rig.began();
    rig.ended(0, 0, 0);
    rig.exits(true);
    rig.restart();
    rig.settle();
    rig.settle();
    let input = rig.input_at_restart.borrow()[0].clone().expect("an input");
    assert_eq!(input["skip"], true);
    assert_eq!(rig.marker(&place).as_deref(), Some("fedcba9876543210"));

    // Once a record was written, the next restart restores again.
    let place = Place::new("restart-unfinished-written");
    let rig = Rig::with(&place, Some(&start), true);
    rig.began();
    rig.ended(0, 0, 0);
    let written = one_file("/home/someone/project/b.rs");
    rig.answer(Ok(json(&written)));
    rig.settle();
    rig.exits(true);
    rig.restart();
    rig.settle();
    rig.settle();
    let input = rig.input_at_restart.borrow()[0].clone().expect("an input");
    assert_eq!(input["skip"], false);
    assert_eq!(input["record"], record_json(&written));
}

#[test]
fn a_window_started_with_files_never_restores_or_writes_even_after_a_restart() {
    let place = Place::new("with-files");
    let start = one_file("/home/someone/project/a.rs");
    place.file().write(&start).unwrap();
    let before = std::fs::read(place.file().path()).unwrap();
    let prepared = prepare_start(&crate::editor_reopen_start::ReopenStart {
        started_with_files: true,
        ..place.start()
    });
    assert!(prepared.writer.is_none());
    let rig = Rig::start(prepared);
    assert_eq!(
        rig.driver.query(|c| c.window(WIN).map(|w| w.reopen.is_none())),
        Some(true)
    );
    rig.exits(true);
    rig.restart();
    rig.scheduler.advance(Duration::ZERO);
    assert_eq!(rig.restarts(), 1, "today's restart, at once");
    assert!(rig.reads().is_empty());
    assert_eq!(rig.input_at_restart.borrow()[0], None, "no input was written");
    rig.driver.post(HostEvent::EndSignal { name: "SIGTERM" });
    assert!(rig.records().is_empty());
    assert_eq!(std::fs::read(place.file().path()).unwrap(), before);
    assert!(place.input().is_none());
}

#[test]
fn quit_between_phases_writes_nothing() {
    let place = Place::new("qa-between-phases");
    let start = one_file("/home/someone/project/a.rs");
    let rig = Rig::with(&place, Some(&start), false);
    let before = std::fs::read(place.file().path()).unwrap();
    rig.began();
    assert_eq!(rig.marker(&place), Some(rig.token.clone()));
    // `:qa` before the restore's end: nvim armed no handoff and left nothing, so none is read.
    rig.leaving();
    assert_eq!(
        rig.state(),
        Some(TokenState::Left),
        "a restart after this restores, with no warning"
    );
    rig.exits(false);
    rig.scheduler.advance(Duration::ZERO);
    assert!(rig.reads().is_empty());
    assert_eq!(rig.records(), vec![CleanupOp::EditorRecord { token: None, seq: 0 }]);
    rig.flush();
    assert_eq!(
        std::fs::read(place.file().path()).unwrap(),
        before,
        "the record on disk is unchanged"
    );
    assert!(rig.captures().is_empty());
    assert_eq!(
        place.file().marker(),
        None,
        "an nvim that ran its quit was not stuck in its restore, so the next start restores"
    );
    let next = prepare_start(&place.start());
    assert!(!next.skip);
    assert_eq!(next.toast, None);
}

/// Only an nvim that said it is leaving on purpose proves it was not stuck in its restore: a crash, or an exit that
/// said nothing, keeps the marker.
#[test]
fn a_crash_or_a_silent_exit_between_phases_keeps_the_marker() {
    for crashed in [true, false] {
        let place = Place::new(if crashed { "crash-between" } else { "silent-between" });
        let rig = Rig::with(&place, Some(&one_file("/home/someone/project/a.rs")), false);
        rig.began();
        rig.exits(crashed);
        rig.scheduler.advance(Duration::ZERO);
        assert_eq!(rig.marker(&place), Some(rig.token.clone()), "crashed: {crashed}");
    }
}

#[test]
fn a_restart_asks_for_the_read_no_exit_asked_for() {
    let place = Place::new("restart-asks");
    let rig = Rig::with(&place, None, false);
    rig.began();
    rig.ended(0, 0, 0);
    rig.exits(true);
    rig.restart();
    rig.settle();
    rig.settle();
    assert_eq!(rig.restarts(), 1);
    let second = rig.reopen(|r| r.token.clone());
    rig.line(ReopenLine::Began { token: second.clone() });
    rig.line(ReopenLine::Ended {
        token: second.clone(),
        opened: 1,
        gone: 0,
        failed: 0,
        squeezed: 0,
        micros: 1,
    });
    // The restarted nvim ends before it is ready: that exit reads nothing, so the next restart asks.
    rig.exits(false);
    assert_eq!(rig.reads().len(), 1);
    rig.restart();
    assert_eq!(rig.reads().last().map(|(token, _)| token.clone()), Some(second));
    assert_eq!(rig.restarts(), 1, "held for that read");
    rig.settle();
    rig.settle();
    assert_eq!(rig.restarts(), 2);
}

/// The snippet leaves its last record only once its restore has ended, so an nvim that never said so (no line at all,
/// as under a config that clears every autocommand, or a restore cut short) is not waited for: not at its exit, not
/// at the window's close, not by a restart.
#[test]
fn no_handoff_is_asked_for_an_nvim_whose_restore_never_ended() {
    let place = Place::new("no-handoff-started");
    let rig = Rig::with(&place, None, false);
    rig.driver.post(HostEvent::EndSignal { name: "SIGTERM" });
    assert_eq!(
        rig.records(),
        vec![CleanupOp::EditorRecord { token: None, seq: 0 }],
        "the close waits only for the writer"
    );

    let place = Place::new("no-handoff-began");
    let rig = Rig::with(&place, None, false);
    rig.began();
    rig.exits(true);
    assert!(rig.reads().is_empty(), "a restore that never ended armed nothing");
    rig.restart();
    rig.settle();
    rig.settle();
    assert_eq!(rig.restarts(), 1, "the restart did not wait out its bound");
    assert!(rig.reads().is_empty());
}

/// The writer still busy past the restart's bound: the restarted nvim finds no input rather than the ended one's,
/// whose token the window no longer hears and whose restore may be the one that never finished.
#[test]
fn a_restart_whose_input_is_late_starts_with_no_input() {
    let place = Place::new("restart-input-late");
    let start = one_file("/home/someone/project/a.rs");
    let rig = Rig::with(&place, Some(&start), false);
    let first = rig.token.clone();
    assert_eq!(place.input().expect("the start's input")["token"], first.as_str());
    rig.began();
    rig.ended(0, 0, 0);
    // The writer is held up looking for a handoff that is slow to come.
    let stall = "aaaaaaaaaaaaaaaa";
    let stall_path = place.channel().join(crate::editor_reopen::handoff_name(stall));
    rig.writer()
        .read_handoff(99, stall.into(), stall_path.clone(), Duration::from_secs(30));
    rig.exits(true);
    rig.restart();
    rig.scheduler.advance(RESTART_HOLD);
    assert_eq!(rig.restarts(), 0, "the input is still owed");
    rig.scheduler.advance(RESTART_HOLD);
    assert_eq!(rig.restarts(), 1, "the restart went at its bound");
    assert_eq!(
        rig.input_at_restart.borrow()[0],
        None,
        "no input of the ended nvim is left for the restarted one"
    );
    // The writer catches up: the input it then writes is the restarted nvim's own.
    agent::private_fs::write_private(&stall_path, b"not a record").unwrap();
    rig.settle();
    let second = rig.reopen(|r| r.token.clone());
    assert_ne!(second, first);
    assert_eq!(place.input().expect("the late input")["token"], second.as_str());
}
