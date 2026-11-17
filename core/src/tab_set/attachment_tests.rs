// Five tests below carry `#[cfg(target_os = "linux")]`: they run the real attachment desk, `crate::attachments`, whose
// image decoder exists only on Linux (the macOS host has none yet).
//! Composer images in the tab set: imported through the window's desk, kept per tab and per draft, sent as a turn
//! of their own and never put into a running turn, given back with their text, and never shown to the page as
//! bytes.

use super::*;
use crate::test_providers::RecordingProvider;
use agent::AgentConversation;
use base64::Engine as _;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn workspace(label: &str) -> PathBuf {
    agent::state_dirs::redirect_state_to_a_test_root();
    agent::state_dirs::test_workspace_dir(label)
}

fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// A desk this test answers by hand.
pub(super) struct Scripted {
    end: DeskEnd,
}

impl Scripted {
    pub(super) fn install(set: &mut TabSet) -> Scripted {
        let (desk, end) = AttachmentDesk::channels();
        set.install_attachments(desk);
        Scripted { end }
    }

    fn next(&self) -> DeskRequest {
        self.end
            .requests
            .recv_timeout(Duration::from_secs(10))
            .expect("a request reached the desk")
    }

    fn requests(&self) -> Vec<DeskRequest> {
        self.end.requests.try_iter().collect()
    }

    fn tell(&self, event: DeskEvent) {
        self.end.events.send(event).unwrap();
    }

    /// Answers the next request, which must be a begin, with `token`.
    fn begun(&self, token: &str) {
        let DeskRequest::Begin { reply, .. } = self.next() else {
            panic!("a begin")
        };
        self.tell(DeskEvent::Begun {
            reply,
            result: Ok(token.into()),
        });
    }

    /// Takes the next chunk or end and accepts it.
    fn accept(&self) {
        let reply = match self.next() {
            DeskRequest::Chunk { reply, .. } | DeskRequest::End { reply, .. } => reply,
            other => panic!("a chunk or an end, not {other:?}"),
        };
        self.tell(DeskEvent::Answered { reply, result: Ok(()) });
    }

    fn prepared(&self, token: &str, data: &[u8]) {
        self.tell(DeskEvent::Prepared {
            token: token.into(),
            result: Ok(PreparedImage {
                meta: meta_of(data),
                data: Arc::from(data),
                thumbnail_png: b"thumb".to_vec(),
            }),
        });
    }

    /// The tokens of every `Remove` among the requests waiting.
    pub(super) fn removed(&self) -> Vec<String> {
        self.requests()
            .into_iter()
            .flat_map(|request| match request {
                DeskRequest::Remove { tokens } => tokens,
                _ => Vec::new(),
            })
            .collect()
    }
}

fn meta_of(data: &[u8]) -> agent::AttachmentMeta {
    agent::AttachmentMeta {
        media_type: "image/png".into(),
        bytes: data.len() as u64,
        width: 2,
        height: 1,
        sha256: hex(data),
    }
}

/// A PNG-looking image whose bytes are its own.
pub(super) fn png_bytes(tag: u8) -> Vec<u8> {
    let mut data = b"\x89PNG\r\n\x1a\n".to_vec();
    data.extend([tag; 24]);
    data
}

/// Imports `data` into `tab`'s draft through the scripted desk, all the way to ready: its token.
pub(super) fn import(set: &mut TabSet, desk: &Scripted, tab: TabId, token: &str, data: &[u8]) -> String {
    let generation = set.get(tab).unwrap().attachments.generation();
    set.attach_begin(tab, generation, data.len() as u64, "shot.png", "o-begin")
        .unwrap();
    desk.begun(token);
    assert_eq!(
        set.poll_attachments(),
        [AttachAnswer {
            owed: "o-begin".into(),
            result: Ok(Some(token.into()))
        }]
    );
    set.attach_chunk(tab, token, 0, &b64(data), "o-chunk").unwrap();
    desk.accept();
    set.poll_attachments();
    set.attach_end(tab, token, &hex(data), "o-end").unwrap();
    desk.accept();
    desk.prepared(token, data);
    set.poll_attachments();
    token.to_string()
}

/// A Claude tab whose provider takes images (and steers), idle, with a scripted desk.
fn claude_tab(label: &str, provider: RecordingProvider) -> (PathBuf, TabSet, TabId, Arc<RecordingProvider>, Scripted) {
    let dir = workspace(label);
    let provider = Arc::new(provider);
    let conversation =
        AgentConversation::create(provider.clone(), &dir, agent::setting_sources::ProjectTrust::Untrusted).unwrap();
    let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
    let tab = set.active();
    set.get_mut(tab).unwrap().backend = TabBackend::Live(AgentBackend::Conversation {
        agent: AgentId::Claude,
        conversation: Box::new(conversation),
    });
    let desk = Scripted::install(&mut set);
    (dir, set, tab, provider, desk)
}

pub(super) fn turn_with(text: &str, attachments: Vec<ReadyImage>) -> FirstTurn {
    FirstTurn {
        wire: text.into(),
        typed: text.into(),
        typed_by_person: true,
        shell_line: false,
        attachments,
    }
}

fn shut_down(set: &mut TabSet) {
    for tab in set.take_all() {
        if let Some(retired) = tab.retired {
            retired.into_backend().shutdown();
        }
    }
}

fn last_state(set: &mut TabSet) -> serde_json::Value {
    let payloads = set.take_attachment_payloads();
    let (_, last) = payloads
        .iter()
        .rev()
        .find(|(_, p)| p.contains("attachment_state"))
        .expect("an attachment_state");
    serde_json::from_str(last).unwrap()
}

/// The whole path through the window's real desk: staged in a private directory, decoded in a child process,
/// read back; the page is told of it with a thumbnail and never its bytes.
#[cfg(target_os = "linux")]
#[test]
fn begin_chunk_end_makes_a_ready_attachment_with_a_thumbnail() {
    let base = crate::test_scratch_dir::ScratchDir::new("attach", "desk");
    let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
    set.install_attachments(crate::attachments::start_desk(
        Some(base.to_path_buf()),
        crate::attachments::test_decoder(),
        None,
    ));
    let tab = set.active();
    // Larger than a thumbnail, so the thumbnail is not the image itself.
    let raw = crate::attachments::test_png(600, 400, 255);

    set.attach_begin(tab, 0, raw.len() as u64, "a\u{7}shot.png", "b")
        .unwrap();
    let mut answers = Vec::new();
    until("the begin's answer", || {
        answers.extend(set.poll_attachments());
        !answers.is_empty()
    });
    let Ok(Some(token)) = answers.remove(0).result else {
        panic!("begun")
    };
    assert_eq!(token.len(), 32, "an opaque token");
    for (index, chunk) in raw.chunks(1000).enumerate() {
        set.attach_chunk(tab, &token, (index * 1000) as u64, &b64(chunk), "c")
            .unwrap();
        until("the chunk's answer", || {
            answers.extend(set.poll_attachments());
            !answers.is_empty()
        });
        assert_eq!(answers.remove(0).result, Ok(None));
    }
    set.attach_end(tab, &token, &hex(&raw), "e").unwrap();
    until("the image is ready", || {
        set.poll_attachments();
        set.draft_images(tab, std::slice::from_ref(&token)).is_ok()
    });

    let ready = set.draft_images(tab, std::slice::from_ref(&token)).unwrap().remove(0);
    assert_eq!(ready.image.meta.media_type, "image/png");
    assert_eq!((ready.image.meta.width, ready.image.meta.height), (600, 400));
    assert_eq!(&ready.image.data[..], &raw[..], "a small PNG is kept byte for byte");
    assert_eq!(ready.name, "ashot.png", "control characters are dropped from the name");
    let draft: serde_json::Value = serde_json::from_str(&set.draft_attachments_payload(tab).unwrap()).unwrap();
    let chip = &draft["attachments"][0];
    assert_eq!(chip["state"], "ready");
    assert!(chip["thumbnail"]
        .as_str()
        .unwrap()
        .starts_with("data:image/png;base64,"));
    assert!(!draft.to_string().contains(&b64(&raw)), "never the bytes");
    shut_down(&mut set);
}

#[test]
fn a_chunk_out_of_order_or_over_the_cap_is_refused_and_the_import_cancelled() {
    let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
    let desk = Scripted::install(&mut set);
    let tab = set.active();
    let total = (CHUNK_MAX_BYTES * 2) as u64;
    for (token, offset, data) in [
        ("a".repeat(32), 5, vec![1u8; 10]),
        ("b".repeat(32), 0, vec![1u8; CHUNK_MAX_BYTES + 1]),
    ] {
        set.attach_begin(tab, 0, total, "x", "b").unwrap();
        desk.begun(&token);
        set.poll_attachments();
        set.take_attachment_payloads();
        let refused = set.attach_chunk(tab, &token, offset, &b64(&data), "c").unwrap_err();
        assert!(
            refused.contains("out of order") || refused.contains("256 KB"),
            "{refused}"
        );
        assert_eq!(desk.removed(), [token.clone()], "its staged bytes go");
        let state = last_state(&mut set);
        assert_eq!(state["state"], "failed");
        assert_eq!(state["reason"], refused.as_str());
        assert!(set.draft_images(tab, std::slice::from_ref(&token)).is_err());
        assert!(
            set.attach_chunk(tab, &token, 0, &b64(&[1]), "c").is_err(),
            "the import is over"
        );
    }
}

#[test]
fn a_wrong_digest_fails_the_import() {
    let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
    let desk = Scripted::install(&mut set);
    let tab = set.active();
    let token = "c".repeat(32);
    set.attach_begin(tab, 0, 3, "x", "b").unwrap();
    desk.begun(&token);
    set.poll_attachments();
    set.attach_chunk(tab, &token, 0, &b64(b"abc"), "c").unwrap();
    desk.accept();
    set.poll_attachments();
    set.attach_end(tab, &token, &"0".repeat(64), "e").unwrap();
    let DeskRequest::End { reply, .. } = desk.next() else {
        panic!("the end")
    };
    desk.tell(DeskEvent::Answered {
        reply,
        result: Err("image upload failed its integrity check".into()),
    });
    let answers = set.poll_attachments();
    assert_eq!(answers[0].result, Err("image upload failed its integrity check".into()));
    assert_eq!(last_state(&mut set)["state"], "failed");
    assert_eq!(desk.removed(), [token]);
}

#[test]
fn an_id_from_another_tab_or_an_old_generation_is_refused() {
    let (_dir, mut set, tab, _provider, desk) = claude_tab("attach-owner", RecordingProvider::image_taking());
    let other = set.open(AgentId::Claude);
    let token = import(&mut set, &desk, tab, &"d".repeat(32), &png_bytes(1));
    assert!(
        set.draft_images(other, std::slice::from_ref(&token)).is_err(),
        "another tab's"
    );
    assert!(set.attach_remove(other, &token).is_err());
    assert!(set.attach_chunk(other, &token, 0, &b64(b"x"), "c").is_err());

    // A send takes the draft's images and moves it to its next generation.
    let images = set.draft_images(tab, std::slice::from_ref(&token)).unwrap();
    assert!(matches!(
        set.send_prompt(tab, "p1", ViewId::BASE, turn_with("look", images)),
        Some(Sent::Prompt { outcome: Ok(()), .. })
    ));
    assert_eq!(set.get(tab).unwrap().attachments.generation(), 1);
    assert!(
        set.draft_images(tab, std::slice::from_ref(&token)).is_err(),
        "it left the draft"
    );
    let refused = set.attach_begin(tab, 0, 10, "x", "b").unwrap_err();
    assert!(refused.contains("draft has changed"), "{refused}");
    set.attach_begin(tab, 1, 10, "x", "b").expect("the current generation");
    shut_down(&mut set);
}

/// The window's 64 MB is reserved when an import begins, before any byte is kept: the third begin past it is
/// refused and leaves no file.
#[cfg(target_os = "linux")]
#[test]
fn the_window_quota_refuses_before_any_byte_is_kept() {
    let base = crate::test_scratch_dir::ScratchDir::new("attach", "quota");
    let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
    set.install_attachments(crate::attachments::start_desk(
        Some(base.to_path_buf()),
        crate::attachments::test_decoder(),
        None,
    ));
    let tab = set.active();
    let half = crate::attachments::WINDOW_QUOTA_BYTES / 2;
    let mut results = Vec::new();
    for (owed, len) in [("a", half), ("b", half), ("c", 1)] {
        set.attach_begin(tab, 0, len, "x", owed).unwrap();
        until("its answer", || {
            results.extend(set.poll_attachments());
            results.iter().any(|a| a.owed == owed)
        });
    }
    assert!(results[0].result.is_ok() && results[1].result.is_ok());
    assert_eq!(
        results[2].result,
        Err("this window has reached its 64 MB image limit".into())
    );
    let dir = std::fs::read_dir(base.join("eitri/attachments"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert_eq!(
        std::fs::read_dir(dir).unwrap().count(),
        2,
        "no file for the refused one"
    );
    assert_eq!(set.get(tab).unwrap().attachments.len(), 2);
    shut_down(&mut set);
}

#[test]
fn send_reads_the_staged_bytes_in_order_and_removes_them_after_the_reply() {
    let (_dir, mut set, tab, provider, desk) = claude_tab("attach-send", RecordingProvider::image_taking());
    let (one, two) = (png_bytes(1), png_bytes(2));
    let first = import(&mut set, &desk, tab, &"1".repeat(32), &one);
    let second = import(&mut set, &desk, tab, &"2".repeat(32), &two);
    desk.requests();
    let images = set.draft_images(tab, &[second.clone(), first.clone()]).unwrap();
    assert!(matches!(
        set.send_prompt(tab, "p1", ViewId::BASE, turn_with("both", images)),
        Some(Sent::Prompt { outcome: Ok(()), .. })
    ));
    assert_eq!(
        provider.turn_images(),
        vec![vec![meta_of(&two), meta_of(&one)]],
        "in the order the message named them"
    );
    assert_eq!(
        desk.removed(),
        [second, first],
        "their staged files go once the send was answered"
    );
    assert!(set.get(tab).unwrap().attachments.is_empty());
    shut_down(&mut set);
}

#[test]
fn a_refused_send_keeps_text_and_attachments() {
    let (_dir, mut set, tab, provider, desk) = claude_tab("attach-refused", RecordingProvider::image_taking());
    let token = import(&mut set, &desk, tab, &"e".repeat(32), &png_bytes(3));
    desk.requests();
    provider.refuse_content(Some("an image is too large to send"));
    let images = set.draft_images(tab, std::slice::from_ref(&token)).unwrap();
    let Some(Sent::Prompt { outcome, typed, .. }) = set.send_prompt(tab, "p1", ViewId::BASE, turn_with("look", images))
    else {
        panic!("answered at once")
    };
    assert_eq!(outcome.unwrap_err().message, "an image is too large to send");
    assert_eq!(typed, "look", "the page puts the text back");
    assert!(
        set.draft_images(tab, std::slice::from_ref(&token)).is_ok(),
        "the image stays in the draft"
    );
    assert_eq!(set.get(tab).unwrap().attachments.generation(), 0);
    assert!(desk.removed().is_empty(), "nothing was removed");
    assert!(set.get(tab).unwrap().session.queue.is_empty());
    shut_down(&mut set);
}

/// Enter while a turn runs: the message with images is held, never put into the turn, and holds back what
/// follows it; at the turn's end it goes as its own turn with its images.
#[test]
fn a_running_turn_queues_attachments_whole_and_never_steers_them() {
    let (dir, mut set, tab, provider, desk) = claude_tab("attach-running", RecordingProvider::image_taking_steering());
    let data = png_bytes(4);
    let token = import(&mut set, &desk, tab, &"f".repeat(32), &data);
    provider.queue(AgentDomainEvent::TurnStarted {
        turn_id: "turn-1".into(),
    });
    until("the turn runs", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().turn_running()
    });
    let images = set.draft_images(tab, std::slice::from_ref(&token)).unwrap();
    set.queue_message_with_images(
        tab,
        "see this",
        "see this".into(),
        1,
        false,
        PromptOrigin::Typed,
        images,
    )
    .unwrap();
    set.queue_message(tab, "and this", "and this".into(), 2, true, PromptOrigin::Typed)
        .unwrap();
    set.steer_held(tab, 3, Some("turn-1"));
    assert!(provider.steers().is_empty(), "nothing went into the running turn");
    assert!(set.get(tab).unwrap().queue_line().unwrap().contains("images"));
    assert!(
        set.get(tab).unwrap().attachments.is_empty(),
        "the draft's images went with the message"
    );

    provider.queue(AgentDomainEvent::TurnCompleted {
        turn_id: "turn-1".into(),
        outcome: agent::TurnOutcome::Completed,
        result_text: String::new(),
        stop_reason: None,
        usage: None,
        detail: Default::default(),
    });
    until("the turn ends", || {
        set.pump(&dir, true);
        !set.get(tab).unwrap().turn_running()
    });
    let flush = set.flush_queue(tab).expect("the queue goes at the turn's end");
    assert!(flush.outcome.is_ok());
    assert_eq!(flush.typed, "see this", "a message with images is a turn of its own");
    assert_eq!(provider.turn_images().last().unwrap(), &vec![meta_of(&data)]);
    assert_eq!(
        set.get(tab).unwrap().session.queue.len(),
        1,
        "the next one waits for its own turn"
    );
    assert_eq!(desk.removed(), [token]);
    shut_down(&mut set);
}

/// The agent had a turn open that this side had not folded: the projection shows none, the provider refuses
/// the image turn as busy. It is queued whole, never steered, and its images go with it later.
#[test]
fn an_image_turn_refused_as_busy_by_an_unseen_turn_is_queued_whole() {
    let (_dir, mut set, tab, provider, desk) =
        claude_tab("attach-unseen-busy", RecordingProvider::image_taking_steering());
    let data = png_bytes(5);
    let token = import(&mut set, &desk, tab, &"9".repeat(32), &data);
    provider.refuse_sends(true);
    assert!(!set.get(tab).unwrap().turn_running(), "this side sees no turn");
    let images = set.draft_images(tab, std::slice::from_ref(&token)).unwrap();
    assert!(matches!(
        set.send_prompt(tab, "p1", ViewId::BASE, turn_with("look", images)),
        Some(Sent::Prompt { outcome: Ok(()), .. })
    ));
    {
        let queue = &set.get(tab).unwrap().session.queue;
        assert_eq!(queue.len(), 1);
        assert!(!queue[0].steer_when_running, "never marked to go into the turn");
        assert_eq!(queue[0].attachments.len(), 1, "its images are with it");
    }
    assert!(provider.steers().is_empty());
    provider.refuse_sends(false);
    let flush = set.flush_queue(tab).expect("it goes once the agent takes a turn");
    assert!(flush.outcome.is_ok());
    assert_eq!(provider.turn_images().last().unwrap(), &vec![meta_of(&data)]);
    shut_down(&mut set);
}

#[test]
fn take_back_returns_text_and_attachments_together() {
    let (dir, mut set, tab, provider, desk) = claude_tab("attach-take-back", RecordingProvider::image_taking());
    let token = import(&mut set, &desk, tab, &"7".repeat(32), &png_bytes(6));
    provider.queue(AgentDomainEvent::TurnStarted {
        turn_id: "turn-1".into(),
    });
    until("the turn runs", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().turn_running()
    });
    set.queue_message(tab, "first", "first".into(), 1, true, PromptOrigin::Typed)
        .unwrap();
    let images = set.draft_images(tab, std::slice::from_ref(&token)).unwrap();
    set.queue_message_with_images(tab, "second", "second".into(), 2, false, PromptOrigin::Typed, images)
        .unwrap();
    set.take_attachment_payloads();
    assert_eq!(
        set.take_back_queue(tab),
        vec![
            QueuedDraft {
                text: "first".into(),
                attachments: Vec::new()
            },
            QueuedDraft {
                text: "second".into(),
                attachments: vec![token.clone()]
            },
        ]
    );
    assert!(
        set.draft_images(tab, std::slice::from_ref(&token)).is_ok(),
        "ready in the draft again"
    );
    let payloads = set.take_attachment_payloads();
    let draft: serde_json::Value = serde_json::from_str(&payloads.last().unwrap().1).unwrap();
    assert_eq!(draft["kind"], "draft_attachments");
    assert_eq!(draft["attachments"][0]["attachmentId"], token.as_str());
    let taken: serde_json::Value = serde_json::from_str(&crate::agent_bridge::serialize_queue_taken_drafts_for_js(
        tab,
        &[QueuedDraft {
            text: "second".into(),
            attachments: vec![token.clone()],
        }],
    ))
    .unwrap();
    assert_eq!(taken["texts"][0], "second");
    assert_eq!(taken["attachments"][0][0], token.as_str());
    assert!(desk.removed().is_empty(), "the staged files stay for the draft");
    shut_down(&mut set);
}

#[test]
fn a_slash_action_with_attachments_is_refused() {
    let (_dir, mut set, tab, _provider, desk) = claude_tab("attach-slash", RecordingProvider::image_taking());
    let token = import(&mut set, &desk, tab, &"8".repeat(32), &png_bytes(7));
    let images = set.draft_images(tab, std::slice::from_ref(&token)).unwrap();
    let refused = set
        .queue_message_with_images(
            tab,
            " /compact",
            "/compact".into(),
            1,
            false,
            PromptOrigin::Typed,
            images,
        )
        .unwrap_err();
    assert_eq!(refused, crate::agent_backend::NO_IMAGES_WITH_A_COMMAND);
    assert!(set.get(tab).unwrap().session.queue.is_empty());
    assert!(
        set.draft_images(tab, std::slice::from_ref(&token)).is_ok(),
        "still in the draft"
    );
    shut_down(&mut set);
}

#[test]
fn the_cumulative_warning_comes_once_past_16_mib() {
    let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
    let tab = set.active();
    let heavy = |token: &str| ReadyImage {
        token: token.into(),
        name: "x".into(),
        image: agent::TurnImage {
            meta: agent::AttachmentMeta {
                bytes: 6 * 1_048_576,
                ..meta_of(b"x")
            },
            data: Arc::from(&b"x"[..]),
        },
        thumbnail: String::new(),
    };
    set.images_sent(tab, &[heavy("a"), heavy("b")]);
    assert!(set.take_image_warnings(|_| true).is_empty(), "12 MB: no warning yet");
    set.images_sent(tab, &[heavy("c")]);
    let warnings = set.take_image_warnings(|_| true);
    assert_eq!(
        warnings,
        [(
            tab,
            "this conversation now carries 18 MB of images; /compact or a new tab if Claude starts failing".to_string()
        )]
    );
    set.images_sent(tab, &[heavy("d")]);
    assert!(set.take_image_warnings(|_| true).is_empty(), "said once");
}

#[test]
fn the_snapshot_carries_metadata_and_thumbnails_never_bytes() {
    let (_dir, mut set, tab, _provider, desk) = claude_tab("attach-snapshot", RecordingProvider::image_taking());
    let data = png_bytes(8);
    let token = import(&mut set, &desk, tab, &"6".repeat(32), &data);
    let payloads = set.state_payloads(tab);
    let draft = payloads
        .iter()
        .find(|p| p.contains("\"draft_attachments\""))
        .expect("the draft's images go with its text");
    let draft: serde_json::Value = serde_json::from_str(draft).unwrap();
    assert_eq!(draft["generation"], 0);
    assert_eq!(draft["imageInput"], true);
    let chip = &draft["attachments"][0];
    assert_eq!(chip["attachmentId"], token.as_str());
    assert_eq!(chip["mediaType"], "image/png");
    assert_eq!(chip["bytes"], data.len());
    assert_eq!(chip["thumbnail"], format!("data:image/png;base64,{}", b64(b"thumb")));
    let mut keys: Vec<&str> = chip.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "attachmentId",
            "bytes",
            "height",
            "mediaType",
            "name",
            "state",
            "tab",
            "thumbnail",
            "width"
        ],
        "metadata and a thumbnail, nothing else"
    );
    assert!(payloads.iter().all(|p| !p.contains(&b64(&data))), "never the bytes");
    shut_down(&mut set);
}

#[test]
fn image_input_is_offered_only_with_turn_content_v1() {
    let (_dir, mut set, tab, _provider, _desk) = claude_tab("attach-offered", RecordingProvider::image_taking());
    assert_eq!(set.image_input(tab), Ok(()));
    let snapshot = |set: &mut TabSet, tab| -> serde_json::Value {
        let payload = set.state_payloads(tab).remove(0);
        serde_json::from_str::<serde_json::Value>(&payload).unwrap()["state"]["capabilities"].clone()
    };
    let offered = snapshot(&mut set, tab);
    assert_eq!(offered["imageInput"], true);
    assert_eq!(offered["imageLimits"]["maxImagesPerTurn"], 8);
    assert_eq!(offered["imageLimits"]["maxImageBytes"], IMAGE_MAX_BYTES);
    shut_down(&mut set);

    for provider in [
        RecordingProvider::default(),
        RecordingProvider::image_taking_without_limits(),
    ] {
        let (_dir, mut set, tab, _provider, _desk) = claude_tab("attach-unoffered", provider);
        let why = set.image_input(tab).unwrap_err();
        assert!(why.starts_with("this tab cannot take images"), "{why}");
        let capabilities = snapshot(&mut set, tab);
        assert!(capabilities.get("imageInput").is_none() && capabilities.get("imageLimits").is_none());
        assert!(set.attach_begin(tab, 0, 10, "x", "b").is_err(), "nothing is imported");
        shut_down(&mut set);
    }

    let mut bare = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
    let unstarted = bare.active();
    assert!(
        bare.image_input(unstarted).is_err(),
        "a window without a desk takes no images"
    );
    let codex = bare.open(AgentId::Codex);
    Scripted::install(&mut bare);
    assert_eq!(
        bare.image_input(unstarted),
        Ok(()),
        "a Claude tab may import for its first message"
    );
    assert!(bare.image_input(codex).is_err());
}

/// A tab moving to the other window leaves its draft's images here (their files go when it leaves), and the
/// window it lands in says so; one whose queue holds images does not move.
#[test]
fn a_moved_tab_drops_draft_attachments_and_says_so() {
    let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
    let desk = Scripted::install(&mut set);
    let tab = set.active();
    let token = import(&mut set, &desk, tab, &"5".repeat(32), &png_bytes(9));
    set.open(AgentId::Claude);
    desk.requests();

    let frozen = set.freeze_for_move(tab).expect("an unstarted tab moves");
    let mut other = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
    Scripted::install(&mut other);
    let landed = other.adopt_moved_unstarted(frozen.tab, ViewId::BASE).unwrap();
    assert!(other.get(landed).unwrap().attachments.is_empty());
    let draft: serde_json::Value = serde_json::from_str(&other.draft_attachments_payload(landed).unwrap()).unwrap();
    assert_eq!(
        draft["note"],
        "1 image in the draft stayed in the other window: paste it again"
    );
    let again: serde_json::Value = serde_json::from_str(&other.draft_attachments_payload(landed).unwrap()).unwrap();
    assert!(again.get("note").is_none(), "said once");
    set.hand_over(tab).unwrap();
    assert_eq!(desk.removed(), [token]);

    let mut queued = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
    let held = queued.active();
    let mut item = Queued::held("look", "look".into(), 1);
    item.attachments = vec![ReadyImage {
        token: "4".repeat(32),
        name: "x".into(),
        image: agent::TurnImage {
            meta: meta_of(b"x"),
            data: Arc::from(&b"x"[..]),
        },
        thumbnail: String::new(),
    }];
    queued.get_mut(held).unwrap().session.queue.push(item);
    assert_eq!(queued.move_blocker(held), Some(MoveRefusedWhy::QueuedImages));
}

#[test]
fn close_removes_the_tab_s_staged_files() {
    let (_dir, mut set, tab, _provider, desk) = claude_tab("attach-close", RecordingProvider::image_taking());
    set.open(AgentId::Claude);
    let draft = import(&mut set, &desk, tab, &"3".repeat(32), &png_bytes(10));
    let queued = import(&mut set, &desk, tab, &"2".repeat(32), &png_bytes(11));
    let images = set.draft_images(tab, std::slice::from_ref(&queued)).unwrap();
    set.queue_message_with_images(tab, "later", "later".into(), 1, true, PromptOrigin::Typed, images)
        .unwrap();
    desk.requests();
    let removed = set.remove(tab).expect("closed");
    if let Some(retired) = removed.retired {
        retired.into_backend().shutdown();
    }
    let mut gone = desk.removed();
    gone.sort();
    assert_eq!(gone, [queued, draft]);
    shut_down(&mut set);
}

/// A first message with an image waits for the trust question: while it waits, its image can neither be taken out
/// of the draft (the message would still carry it) nor go with another message; put off, the image is the draft's
/// again, and nothing had to give it back.
#[test]
fn a_waiting_first_message_keeps_its_images_from_being_removed_or_sent_twice() {
    let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
    let desk = Scripted::install(&mut set);
    let tab = set.active();
    let token = import(&mut set, &desk, tab, &"7".repeat(32), &png_bytes(20));
    let images = set.draft_images(tab, std::slice::from_ref(&token)).unwrap();
    set.defer_start(
        tab,
        DeferredStart::FirstSend {
            request_id: "r1".into(),
            first_turn: turn_with("look", images),
        },
    )
    .unwrap();
    desk.requests();
    assert_eq!(set.attach_remove(tab, &token), Err(PARKED.to_string()));
    assert_eq!(
        set.draft_images(tab, std::slice::from_ref(&token)).unwrap_err(),
        PARKED,
        "the queue cannot take it a second time"
    );
    assert!(desk.removed().is_empty(), "its staged file stays");
    assert_eq!(set.move_blocker(tab), Some(MoveRefusedWhy::QueuedImages));

    let nonce = set.ask_trust(tab).unwrap();
    let put_off = set.cancel_trust(tab, nonce, ViewId::BASE).unwrap();
    drop(put_off);
    assert!(
        set.draft_images(tab, std::slice::from_ref(&token)).is_ok(),
        "put off, the image is the draft's again"
    );
    assert_eq!(set.attach_remove(tab, &token), Ok(()));
}

/// A page loaded again in the middle of an upload cannot finish it: the import fails with a sentence and its room
/// in the window's quota goes. One whose bytes all reached the desk is being prepared and stays.
#[test]
fn a_page_reload_ends_the_imports_it_was_sending() {
    let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
    let desk = Scripted::install(&mut set);
    let tab = set.active();
    let data = png_bytes(21);
    let (half, rest) = data.split_at(10);
    set.attach_begin(tab, 0, data.len() as u64, "half.png", "b1").unwrap();
    desk.begun(&"8".repeat(32));
    set.poll_attachments();
    set.attach_chunk(tab, &"8".repeat(32), 0, &b64(half), "c1").unwrap();
    desk.accept();
    set.poll_attachments();
    set.attach_begin(tab, 0, rest.len() as u64, "whole.png", "b2").unwrap();
    desk.begun(&"9".repeat(32));
    set.poll_attachments();
    set.attach_chunk(tab, &"9".repeat(32), 0, &b64(rest), "c2").unwrap();
    desk.accept();
    set.poll_attachments();
    set.attach_end(tab, &"9".repeat(32), &hex(rest), "e2").unwrap();
    desk.requests();
    set.take_attachment_payloads();

    set.abandon_imports_of(ViewId::BASE);
    let state = last_state(&mut set);
    assert_eq!(
        (
            state["attachmentId"].as_str().unwrap(),
            state["state"].as_str().unwrap()
        ),
        ("8".repeat(32).as_str(), "failed")
    );
    assert_eq!(state["reason"], ABANDONED);
    assert_eq!(
        desk.removed(),
        ["8".repeat(32)],
        "its room goes; the one being prepared stays"
    );
    assert!(set.take_attachment_payloads().is_empty());
}

/// A begin the desk answers after its tab has gone frees the room the desk kept for it.
#[test]
fn a_begin_answered_after_its_tab_left_frees_its_room() {
    let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
    let desk = Scripted::install(&mut set);
    let tab = set.active();
    set.open(AgentId::Claude);
    set.attach_begin(tab, 0, 10, "x.png", "b").unwrap();
    let DeskRequest::Begin { reply, .. } = desk.next() else {
        panic!("a begin")
    };
    set.remove(tab).expect("closed");
    desk.requests();
    desk.tell(DeskEvent::Begun {
        reply,
        result: Ok("a".repeat(32)),
    });
    assert!(set.poll_attachments().is_empty(), "no page waits for it");
    assert_eq!(desk.removed(), ["a".repeat(32)]);
}

/// A message handed back and sent again carries the same images: they count once towards the warning.
#[test]
fn an_image_sent_again_counts_once() {
    let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
    let tab = set.active();
    let heavy = ReadyImage {
        token: "a".into(),
        name: "x".into(),
        image: agent::TurnImage {
            meta: agent::AttachmentMeta {
                bytes: 10 * 1_048_576,
                ..meta_of(b"x")
            },
            data: Arc::from(&b"x"[..]),
        },
        thumbnail: String::new(),
    };
    set.images_sent(tab, std::slice::from_ref(&heavy));
    set.images_sent(tab, std::slice::from_ref(&heavy));
    assert!(set.take_image_warnings(|_| true).is_empty(), "10 MB, not 20");
}

/// The warning about a tab no page shows waits for a page that shows it.
#[test]
fn the_image_warning_waits_until_a_page_shows_its_tab() {
    let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
    let tab = set.active();
    let heavy = ReadyImage {
        token: "a".into(),
        name: "x".into(),
        image: agent::TurnImage {
            meta: agent::AttachmentMeta {
                bytes: 17 * 1_048_576,
                ..meta_of(b"x")
            },
            data: Arc::from(&b"x"[..]),
        },
        thumbnail: String::new(),
    };
    set.images_sent(tab, &[heavy]);
    assert!(set.take_image_warnings(|_| false).is_empty());
    assert_eq!(set.take_image_warnings(|_| true).len(), 1, "kept until shown");
    assert!(set.take_image_warnings(|_| true).is_empty(), "once");
}

/// A sent message the CLI may still hand back to the queue holds its images in this process: the tab does not
/// move while it does.
#[test]
fn a_tab_whose_sent_message_may_come_back_with_images_does_not_move() {
    let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
    let tab = set.active();
    let mut message = Queued::held("look", "look".into(), 1);
    message.attachments = vec![ReadyImage {
        token: "a".into(),
        name: "x".into(),
        image: agent::TurnImage {
            meta: meta_of(b"x"),
            data: Arc::from(&b"x"[..]),
        },
        thumbnail: String::new(),
    }];
    set.get_mut(tab).unwrap().session.opening_prompt = Some(OpeningPrompt {
        submitted: crate::agent_backend::SubmittedPrompt {
            client_message_id: "c1".into(),
            turn_id: None,
            after_revision: 0,
        },
        message,
        typed_by_person: true,
    });
    assert_eq!(set.move_blocker(tab), Some(MoveRefusedWhy::QueuedImages));
}

/// The real desk keeps a prepared image in memory only: its file goes once read, and its room stays counted.
#[cfg(target_os = "linux")]
#[test]
fn a_prepared_image_is_not_kept_twice() {
    let base = crate::test_scratch_dir::ScratchDir::new("attach", "once");
    let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
    set.install_attachments(crate::attachments::start_desk(
        Some(base.to_path_buf()),
        crate::attachments::test_decoder(),
        None,
    ));
    let tab = set.active();
    let raw = crate::attachments::test_png(64, 64, 255);
    set.attach_begin(tab, 0, raw.len() as u64, "a.png", "b").unwrap();
    let mut answers = Vec::new();
    until("the begin's answer", || {
        answers.extend(set.poll_attachments());
        !answers.is_empty()
    });
    let Ok(Some(token)) = answers.remove(0).result else {
        panic!("begun")
    };
    set.attach_chunk(tab, &token, 0, &b64(&raw), "c").unwrap();
    until("the chunk's answer", || {
        answers.extend(set.poll_attachments());
        !answers.is_empty()
    });
    set.attach_end(tab, &token, &hex(&raw), "e").unwrap();
    until("the image is ready", || {
        set.poll_attachments();
        set.draft_images(tab, std::slice::from_ref(&token)).is_ok()
    });
    let files: Vec<_> = std::fs::read_dir(base.join("eitri/attachments"))
        .unwrap()
        .flat_map(|window| std::fs::read_dir(window.unwrap().path()).unwrap())
        .map(|file| file.unwrap().file_name())
        .collect();
    assert!(files.is_empty(), "nothing staged on disk once in memory: {files:?}");
    shut_down(&mut set);
}

/// A file dropped on the page: an importing chip appears before the desk reads it (the path never reaches the page),
/// and stays importing until prepared. A file not on this computer (a browser's link) is left out:
/// no chip for it holds Send.
#[test]
fn a_dropped_file_keeps_its_chip_from_read_through_preparation() {
    let (_dir, mut set, tab, _provider, desk) = claude_tab("attach-drop", RecordingProvider::image_taking());
    let dropped = |path: Option<&str>, name: &str| DroppedFile {
        path: path.map(PathBuf::from),
        name: name.into(),
    };
    set.attach_files(
        tab,
        vec![
            dropped(Some("/pictures/one.png"), "one.png"),
            dropped(None, "far.png"),
            dropped(Some("/pictures/two.png"), "two.png"),
        ],
    )
    .unwrap();
    let chips = set.take_attachment_payloads();
    assert_eq!(chips.len(), 2, "each native file is visible while being read");
    assert!(chips.iter().all(|(_, payload)| payload.contains("importing")));
    assert_eq!(
        set.attach_files(tab, vec![dropped(None, "https://example.com/")]),
        Err("only files on this computer can be attached".to_string()),
        "a drop of links alone is refused, nothing read"
    );
    let mut asked = Vec::new();
    for _ in 0..2 {
        let DeskRequest::ImportFile {
            reply, tab: of, path, ..
        } = desk.next()
        else {
            panic!("a file to read")
        };
        assert_eq!(of, tab.0);
        asked.push((reply, path));
    }
    assert_eq!(
        asked.iter().map(|(_, p)| p.to_str().unwrap()).collect::<Vec<_>>(),
        ["/pictures/one.png", "/pictures/two.png"]
    );
    let one = "1".repeat(32);
    desk.tell(DeskEvent::Imported {
        reply: asked[0].0,
        result: Ok((one.clone(), 32)),
    });
    desk.tell(DeskEvent::Imported {
        reply: asked[1].0,
        result: Err("this file could not be read".into()),
    });
    assert!(set.poll_attachments().is_empty(), "no page waits on a drop");
    assert!(set.attachments_pending(), "the tick polls until the image is prepared");
    let payloads: Vec<serde_json::Value> = set
        .take_attachment_payloads()
        .iter()
        .map(|(_, p)| serde_json::from_str(p).unwrap())
        .collect();
    assert_eq!(payloads[0]["attachments"][0]["state"], "importing");
    assert_eq!(payloads[1]["attachments"][1]["reason"], "this file could not be read");
    assert!(
        payloads.iter().all(|p| !p.to_string().contains("/pictures")),
        "never the path"
    );
    desk.prepared(&one, &png_bytes(30));
    set.poll_attachments();
    assert!(set.draft_images(tab, std::slice::from_ref(&one)).is_ok());
    let draft: serde_json::Value = serde_json::from_str(&set.draft_attachments_payload(tab).unwrap()).unwrap();
    let names: Vec<&str> = draft["attachments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["one.png", "two.png"]);
    shut_down(&mut set);
}

/// A drop is refused whole, before any file is opened, by a tab that takes no images, one that is moving, or a draft
/// it would take past its limit.
#[test]
fn a_drop_the_tab_cannot_take_is_refused_before_any_file_is_read() {
    let file = || DroppedFile {
        path: Some(PathBuf::from("/pictures/one.png")),
        name: "one.png".into(),
    };
    let mut bare = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
    let tab = bare.active();
    assert_eq!(
        bare.attach_files(tab, vec![file()]).unwrap_err(),
        "this window cannot take images"
    );
    let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
    let desk = Scripted::install(&mut set);
    let tab = set.active();
    let refused = set.attach_files(tab, vec![file(); DRAFT_MAX_IMAGES + 1]).unwrap_err();
    assert!(refused.contains("8 images per message"), "{refused}");
    assert!(desk.requests().is_empty(), "no file was asked for");
}

/// A PNG dropped from disk goes through the window's real desk: read, staged, decoded in a child, ready.
#[cfg(target_os = "linux")]
#[test]
fn a_dropped_png_is_read_and_prepared_by_the_real_desk() {
    let base = crate::test_scratch_dir::ScratchDir::new("attach", "drop-desk");
    let pictures = crate::test_scratch_dir::ScratchDir::new("attach", "drop-pictures");
    let picture = pictures.join("shot.png");
    let raw = crate::attachments::test_png(80, 40, 255);
    std::fs::write(&picture, &raw).unwrap();
    let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
    set.install_attachments(crate::attachments::start_desk(
        Some(base.to_path_buf()),
        crate::attachments::test_decoder(),
        None,
    ));
    let tab = set.active();
    set.attach_files(
        tab,
        vec![DroppedFile {
            path: Some(picture),
            name: "shot.png".into(),
        }],
    )
    .unwrap();
    until("the dropped image is ready", || {
        set.poll_attachments();
        let draft: serde_json::Value = serde_json::from_str(&set.draft_attachments_payload(tab).unwrap()).unwrap();
        draft["attachments"][0]["state"] == "ready"
    });
    let draft: serde_json::Value = serde_json::from_str(&set.draft_attachments_payload(tab).unwrap()).unwrap();
    let token = draft["attachments"][0]["attachmentId"].as_str().unwrap().to_string();
    let ready = set.draft_images(tab, &[token]).unwrap().remove(0);
    assert_eq!((ready.image.meta.width, ready.image.meta.height), (80, 40));
    assert_eq!(&ready.image.data[..], &raw[..]);
    shut_down(&mut set);
}

#[test]
fn a_native_drop_is_visible_and_blocks_send_before_the_desk_reads_it() {
    let (_dir, mut set, tab, _provider, _desk) = claude_tab("drop-visible", RecordingProvider::image_taking());
    set.attach_files(
        tab,
        vec![DroppedFile {
            path: Some("/pictures/a.png".into()),
            name: "a.png".into(),
        }],
    )
    .unwrap();
    let payload: serde_json::Value = serde_json::from_str(&set.draft_attachments_payload(tab).unwrap()).unwrap();
    let chips = payload["attachments"].as_array().unwrap();
    assert_eq!(chips.len(), 1);
    assert_eq!(chips[0]["state"], "importing");
    let token = chips[0]["attachmentId"].as_str().unwrap().to_string();
    assert_eq!(
        set.draft_images(tab, &[token]).unwrap_err(),
        "an image is still being prepared"
    );
    shut_down(&mut set);
}

#[test]
fn text_only_sends_and_completions_leave_the_next_drafts_imports_alone() {
    for action in ["send", "queue", "first", "not_sent"] {
        let (_dir, mut set, tab, _provider, desk) = claude_tab("text-paste", RecordingProvider::image_taking());
        set.attach_begin(tab, 0, 32, "paste.png", "begin").unwrap();
        desk.begun(&"a".repeat(32));
        set.poll_attachments();
        set.attach_files(
            tab,
            vec![DroppedFile {
                path: Some("/pictures/a.png".into()),
                name: "a.png".into(),
            }],
        )
        .unwrap();
        let DeskRequest::ImportFile { reply, .. } = desk.next() else {
            panic!("import")
        };
        set.note_sent(tab);
        match action {
            "send" => {
                set.send_prompt(tab, "p1", ViewId::BASE, turn_with("words", vec![]));
            }
            "queue" => {
                set.queue_message(
                    tab,
                    "earlier words",
                    "earlier words".into(),
                    0,
                    false,
                    PromptOrigin::Typed,
                )
                .unwrap();
                assert!(set.flush_queue(tab).unwrap().outcome.is_ok());
            }
            "first" => {
                assert!(matches!(
                    set.send_first_turn(tab, "p1", turn_with("first", vec![])),
                    Some(Sent::First { outcome: Ok(()), .. })
                ));
            }
            _ => set.images_left_draft(tab, &[]),
        }
        desk.tell(DeskEvent::Imported {
            reply,
            result: Ok(("b".repeat(32), 32)),
        });
        set.poll_attachments();
        desk.prepared(&"a".repeat(32), &png_bytes(1));
        desk.prepared(&"b".repeat(32), &png_bytes(2));
        set.poll_attachments();
        assert_eq!(set.get(tab).unwrap().attachments.generation(), 0, "{action}");
        assert_eq!(
            set.draft_images(tab, &["a".repeat(32), "b".repeat(32)]).unwrap().len(),
            2,
            "{action}"
        );
        assert!(set.take_image_warnings(|_| true).is_empty());
        shut_down(&mut set);
    }
}

#[test]
fn deleting_draft_words_preserves_ready_pasting_and_reading_images() {
    let (_dir, mut set, tab, _provider, desk) = claude_tab("erase-words", RecordingProvider::image_taking());
    let ready = import(&mut set, &desk, tab, &"a".repeat(32), &png_bytes(1));
    set.set_draft(tab, "look");
    set.attach_begin(tab, 0, 32, "paste.png", "begin").unwrap();
    desk.begun(&"b".repeat(32));
    set.poll_attachments();
    set.attach_files(
        tab,
        vec![DroppedFile {
            path: Some("/pictures/a.png".into()),
            name: "a.png".into(),
        }],
    )
    .unwrap();
    let DeskRequest::ImportFile { reply, .. } = desk.next() else {
        panic!("import")
    };
    set.set_draft(tab, "");
    desk.tell(DeskEvent::Imported {
        reply,
        result: Ok(("c".repeat(32), 32)),
    });
    desk.prepared(&"b".repeat(32), &png_bytes(2));
    desk.prepared(&"c".repeat(32), &png_bytes(3));
    set.poll_attachments();
    assert_eq!(
        set.draft_images(tab, &[ready, "b".repeat(32), "c".repeat(32)])
            .unwrap()
            .len(),
        3
    );
    assert_eq!(set.get(tab).unwrap().attachments.generation(), 0);
    assert!(set.take_image_warnings(|_| true).is_empty());
    shut_down(&mut set);
}

#[test]
fn taking_images_drops_current_imports_but_later_send_ack_keeps_new_generation_imports() {
    let (_dir, mut set, tab, _provider, desk) = claude_tab("image-generation", RecordingProvider::image_taking());
    let ready = import(&mut set, &desk, tab, &"a".repeat(32), &png_bytes(1));
    let images = set.draft_images(tab, &[ready]).unwrap();
    set.attach_files(
        tab,
        vec![DroppedFile {
            path: Some("/pictures/a.png".into()),
            name: "a.png".into(),
        }],
    )
    .unwrap();
    let DeskRequest::ImportFile { reply, .. } = desk.next() else {
        panic!("import")
    };
    set.images_left_draft(tab, &images);
    set.attach_begin(tab, 1, 32, "next.png", "begin").unwrap();
    desk.begun(&"c".repeat(32));
    set.poll_attachments();
    set.images_sent(tab, &images);
    desk.tell(DeskEvent::Imported {
        reply,
        result: Ok(("b".repeat(32), 32)),
    });
    desk.prepared(&"c".repeat(32), &png_bytes(3));
    set.poll_attachments();
    assert_eq!(set.get(tab).unwrap().attachments.generation(), 1);
    assert!(set.draft_images(tab, &["c".repeat(32)]).is_ok());
    assert!(desk.removed().contains(&"b".repeat(32)));
    shut_down(&mut set);
}

#[test]
fn an_aborted_move_leaves_no_importing_file_chip_or_polling_work() {
    for success in [true, false] {
        let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        let desk = Scripted::install(&mut set);
        let tab = set.active();
        set.attach_files(
            tab,
            vec![DroppedFile {
                path: Some("/pictures/a.png".into()),
                name: "a.png".into(),
            }],
        )
        .unwrap();
        let DeskRequest::ImportFile { reply, .. } = desk.next() else {
            panic!("import")
        };
        let _frozen = set.freeze_for_move(tab).unwrap();
        let token = "a".repeat(32);
        desk.tell(DeskEvent::Imported {
            reply,
            result: if success {
                Ok((token.clone(), 32))
            } else {
                Err("could not read image".into())
            },
        });
        set.poll_attachments();
        set.unfreeze(tab);
        if success {
            assert_eq!(desk.removed(), [token.clone()]);
            desk.prepared(&token, &png_bytes(1));
            set.poll_attachments();
        }
        assert!(!set.attachments_pending());
        assert!(set.get(tab).unwrap().attachments.is_empty());
        assert!(set.draft_images(tab, &[]).is_ok());
        let payload: serde_json::Value = serde_json::from_str(&set.draft_attachments_payload(tab).unwrap()).unwrap();
        assert!(payload["attachments"].as_array().unwrap().is_empty());
        assert!(!set.take_image_warnings(|_| true).is_empty());
    }
}

#[test]
fn removing_a_reading_file_chip_is_silent_when_its_read_finishes() {
    let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
    let desk = Scripted::install(&mut set);
    let tab = set.active();
    set.attach_files(
        tab,
        vec![DroppedFile {
            path: Some("/pictures/a.png".into()),
            name: "a.png".into(),
        }],
    )
    .unwrap();
    let DeskRequest::ImportFile { reply, .. } = desk.next() else {
        panic!("import")
    };
    let payload: serde_json::Value = serde_json::from_str(&set.draft_attachments_payload(tab).unwrap()).unwrap();
    set.attach_remove(tab, payload["attachments"][0]["attachmentId"].as_str().unwrap())
        .unwrap();
    set.get_mut(tab).unwrap().backend = TabBackend::Failed {
        reason: "stopped".into(),
    };
    set.reset(tab).unwrap();
    desk.tell(DeskEvent::Imported {
        reply,
        result: Ok(("a".repeat(32), 32)),
    });
    set.poll_attachments();
    assert_eq!(desk.removed().last(), Some(&"a".repeat(32)));
    assert!(set.take_image_warnings(|_| true).is_empty());
    assert!(!set.attachments_pending());
}

#[test]
fn reset_drops_returned_sent_images_whose_reservations_were_released() {
    let (dir, mut set, tab, provider, desk) = claude_tab("reset-sent-images", RecordingProvider::image_taking());
    let token = import(&mut set, &desk, tab, &"a".repeat(32), &png_bytes(1));
    let images = set.draft_images(tab, &[token.clone()]).unwrap();
    assert!(matches!(
        set.send_prompt(tab, "p1", ViewId::BASE, turn_with("look", images)),
        Some(Sent::Prompt { outcome: Ok(()), .. })
    ));
    assert_eq!(desk.removed(), [token]);
    provider.queue(AgentDomainEvent::TurnStarted {
        turn_id: "turn-1".into(),
    });
    until("the turn starts", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().turn_running()
    });
    provider.queue(AgentDomainEvent::CliCycleStarted {
        turn_id: "turn-1".into(),
        cause: None,
    });
    until("the opening prompt returns to the queue", || {
        set.pump(&dir, true);
        !set.get(tab).unwrap().session.queue.is_empty()
    });
    assert!(matches!(
        set.get(tab).unwrap().session.queue[0].state,
        QueuedState::Steered { .. }
    ));
    let old = std::mem::replace(
        &mut set.get_mut(tab).unwrap().backend,
        TabBackend::Failed {
            reason: "stopped".into(),
        },
    );
    if let TabBackend::Live(mut backend) = old {
        backend.shutdown();
    }
    set.reset(tab).unwrap();
    assert_eq!(set.get(tab).unwrap().draft(), "look");
    assert!(set.get(tab).unwrap().attachments.is_empty());
    assert!(set
        .take_image_warnings(|_| true)
        .iter()
        .any(|(_, note)| note.contains("released") && note.contains("attach")));
}

#[test]
fn taking_back_a_returned_sent_image_keeps_only_attachments_still_reserved() {
    let (_dir, mut set, tab, _provider, desk) = claude_tab("take-back-sent-images", RecordingProvider::image_taking());
    let token = import(&mut set, &desk, tab, &"a".repeat(32), &png_bytes(1));
    let images = set.draft_images(tab, &[token.clone()]).unwrap();
    set.images_sent(tab, &images);
    assert_eq!(desk.removed(), [token]);
    let mut queued = Queued::held("look", "look".into(), 0);
    queued.attachments = images;
    set.get_mut(tab).unwrap().session.queue.push(queued);
    let returned = set.take_back_queue(tab);
    assert_eq!(
        returned,
        [QueuedDraft {
            text: "look".into(),
            attachments: vec![]
        }]
    );
    assert!(set.get(tab).unwrap().attachments.is_empty());
    assert!(set
        .take_image_warnings(|_| true)
        .iter()
        .any(|(_, note)| note.contains("released")));
    shut_down(&mut set);
}

#[cfg(target_os = "linux")]
#[test]
fn resetting_a_failed_tab_restores_queued_images_and_closing_releases_all_quota() {
    let scratch = crate::test_scratch_dir::ScratchDir::new("attach", "reset-quota");
    let pictures = crate::test_scratch_dir::ScratchDir::new("attach", "reset-pictures");
    let (_dir, mut set, tab, _provider, _desk) = claude_tab("reset-images", RecordingProvider::image_taking());
    set.install_attachments(crate::attachments::start_desk(
        Some(scratch.to_path_buf()),
        crate::attachments::test_decoder(),
        None,
    ));
    for tag in [1, 2] {
        let path = pictures.join(format!("{tag}.png"));
        std::fs::write(&path, crate::attachments::test_png(2, 1, 255)).unwrap();
        set.attach_files(
            tab,
            vec![DroppedFile {
                path: Some(path),
                name: format!("{tag}.png"),
            }],
        )
        .unwrap();
    }
    until("both images ready", || {
        set.poll_attachments();
        let payload: serde_json::Value = serde_json::from_str(&set.draft_attachments_payload(tab).unwrap()).unwrap();
        payload["attachments"]
            .as_array()
            .unwrap()
            .iter()
            .all(|image| image["state"] == "ready")
    });
    let payload: serde_json::Value = serde_json::from_str(&set.draft_attachments_payload(tab).unwrap()).unwrap();
    let tokens: Vec<String> = payload["attachments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|image| image["attachmentId"].as_str().unwrap().into())
        .collect();
    let reserved = set.reserved_image_bytes();
    assert!(reserved > 0);
    let images = set.draft_images(tab, &tokens).unwrap();
    set.queue_message_with_images(tab, "look", "look".into(), 0, false, PromptOrigin::Typed, images)
        .unwrap();
    let old = std::mem::replace(
        &mut set.get_mut(tab).unwrap().backend,
        TabBackend::Failed {
            reason: "stopped".into(),
        },
    );
    if let TabBackend::Live(mut backend) = old {
        backend.shutdown();
    }
    set.reset(tab).unwrap();
    assert_eq!(set.get(tab).unwrap().draft(), "look");
    let restored = set.draft_images(tab, &tokens).unwrap();
    assert_eq!(
        restored.iter().map(|image| &image.token).collect::<Vec<_>>(),
        tokens.iter().collect::<Vec<_>>()
    );
    assert_eq!(set.reserved_image_bytes(), reserved);
    set.remove(tab).expect("close the reset tab");
    assert_eq!(set.reserved_image_bytes(), 0);
}

#[test]
fn the_ninth_image_is_refused_before_reserving_any_room() {
    let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
    let desk = Scripted::install(&mut set);
    let tab = set.active();
    for i in 0..8 {
        set.attach_begin(tab, 0, 10, "x", &format!("b{i}")).unwrap();
        desk.begun(&format!("{i:032x}"));
        set.poll_attachments();
    }
    let why = set.attach_begin(tab, 0, 10, "ninth", "b9").unwrap_err();
    assert!(why.contains("8") && why.contains("message"), "{why}");
    assert!(desk.requests().is_empty());
}

#[test]
fn reset_returns_queued_images_in_order_and_releases_the_overflow() {
    let (_dir, mut set, tab, _provider, desk) = claude_tab("reset-overflow", RecordingProvider::image_taking());
    let queued: Vec<_> = (0..2)
        .map(|i| import(&mut set, &desk, tab, &format!("{i:032x}"), &png_bytes(i)))
        .collect();
    let images = set.draft_images(tab, &queued).unwrap();
    set.queue_message_with_images(tab, "old", "old".into(), 0, false, PromptOrigin::Typed, images)
        .unwrap();
    for i in 2..9 {
        import(&mut set, &desk, tab, &format!("{i:032x}"), &png_bytes(i));
    }
    let old = std::mem::replace(
        &mut set.get_mut(tab).unwrap().backend,
        TabBackend::Failed {
            reason: "stopped".into(),
        },
    );
    if let TabBackend::Live(mut backend) = old {
        backend.shutdown();
    }
    set.reset(tab).unwrap();
    assert_eq!(set.get(tab).unwrap().attachments.len(), 8);
    assert!(set.draft_images(tab, &queued[..1]).is_ok());
    assert!(set.draft_images(tab, &queued[1..]).is_err());
    assert_eq!(desk.removed(), queued[1..]);
    assert!(set
        .take_image_warnings(|_| true)
        .iter()
        .any(|(_, note)| note.contains("8 images per message") && note.contains("released")));
    set.remove(tab).expect("close the reset tab");
    assert_eq!(desk.removed().len(), 8, "every kept reservation goes at close");
}

#[test]
fn returning_sent_images_without_reservations_pluralises_the_notice() {
    for (count, noun, pronoun) in [(1, "image", "it"), (2, "images", "them")] {
        let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        let desk = Scripted::install(&mut set);
        let tab = set.active();
        let tokens: Vec<_> = (0..count)
            .map(|i| import(&mut set, &desk, tab, &format!("{i:032x}"), &png_bytes(i)))
            .collect();
        let images = set.draft_images(tab, &tokens).unwrap();
        set.images_sent(tab, &images);
        set.images_back_to_draft(tab, images);
        assert!(set.get(tab).unwrap().attachments.is_empty());
        assert_eq!(set.take_image_warnings(|_| true), [(tab, format!(
            "{count} sent {noun} could not return to the draft because their reserved room was released; attach {pronoun} again"
        ))]);
    }
}

#[test]
fn a_native_drop_preparing_when_the_draft_is_reset_is_discarded() {
    let (_dir, mut set, tab, _provider, desk) = claude_tab("drop-decoding-reset", RecordingProvider::image_taking());
    set.attach_files(
        tab,
        vec![DroppedFile {
            path: Some("/pictures/a.png".into()),
            name: "a.png".into(),
        }],
    )
    .unwrap();
    let DeskRequest::ImportFile { reply, .. } = desk.next() else {
        panic!("import")
    };
    let token = "a".repeat(32);
    desk.tell(DeskEvent::Imported {
        reply,
        result: Ok((token.clone(), 32)),
    });
    set.poll_attachments();
    let old = std::mem::replace(
        &mut set.get_mut(tab).unwrap().backend,
        TabBackend::Failed {
            reason: "stopped".into(),
        },
    );
    if let TabBackend::Live(mut backend) = old {
        backend.shutdown();
    }
    set.reset(tab).unwrap();
    assert!(desk.removed().contains(&token));
    desk.prepared(&token, &png_bytes(1));
    set.poll_attachments();
    assert!(set.get(tab).unwrap().attachments.is_empty());
    shut_down(&mut set);
}
