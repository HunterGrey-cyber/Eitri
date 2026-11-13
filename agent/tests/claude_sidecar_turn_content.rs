//! Images on a Claude turn (protocol 3.26, `turn_content_v1`) through the sidecar provider, against a
//! stand-in sidecar (`tests/support/sidecar_standin.rs --turn-content`). No real `claude` runs here.
//!
//! What is checked: what reaches the wire (each image as a content block, in order, read from the
//! stand-in's record of the decoded request, which names an image by type, length and digest and never
//! keeps its bytes); that a turn of text alone is the request it always was; that images go only to a
//! sidecar that listed the capability and only within the limits it stated, refused here with nothing
//! sent otherwise; that the sidecar's own refusal of content leaves the session working; and that a
//! prompt's record keeps what its images were and never their bytes.
#![cfg(target_os = "linux")]

use agent::setting_sources::ProjectTrust;
use agent::{
    turn_content, AgentConversation, AgentDomainEvent, AgentProvider, AgentSessionProjection, AttachmentMeta,
    ClaudeSidecarProvider, CloseSessionRequest, ConversationError, CreateSessionRequest, PromptOrigin, ProviderError,
    ProviderErrorCode, SendTurnRequest, StreamingPreference, TurnContentLimits, TurnImage,
};
use claude_runtime_protocol::v1::SendTurnRequest as ProtoSendTurnRequest;
use prost::Message as _;
use sha2::Digest as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long any one wait here may take before the test is called hung. Not a speed bound: every wait
/// ends as soon as what it waits for has happened.
const HANG: Duration = Duration::from_secs(30);

/// A private directory under the test target, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("turn-content-{}", uuid::Uuid::new_v4()));
        agent::private_fs::create_private_dir_all(&dir, &dir).unwrap();
        Self(dir)
    }

    fn sends(&self) -> PathBuf {
        self.0.join("sends.txt")
    }

    fn turns(&self) -> PathBuf {
        self.0.join("turns.bin")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Writes an executable script through a child process, so no write descriptor of this process is open
/// for a forking test on another thread to inherit (a script still open for writing fails with `ETXTBSY`).
fn write_script(path: &Path, body: &str) {
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(r#"umask 077 && printf '%s' "$1" > "$2" && chmod 700 "$2""#)
        .arg("sh")
        .arg(body)
        .arg(path)
        .status()
        .expect("sh runs");
    assert!(status.success(), "writing the script failed: {status}");
}

/// A provider on the stand-in, recording into `scratch`, with `flags` on the stand-in's command line.
fn stand_in(scratch: &Scratch, flags: &str) -> ClaudeSidecarProvider {
    let quote = |path: &Path| format!("'{}'", path.display().to_string().replace('\'', "'\\''"));
    let launcher = scratch.0.join("sidecar");
    let script = format!(
        "#!/bin/sh\nexec {} --record-sends {} --record-turns {} {flags}\n",
        quote(Path::new(env!("CARGO_BIN_EXE_eitri-sidecar-standin"))),
        quote(&scratch.sends()),
        quote(&scratch.turns()),
    );
    write_script(&launcher, &script);
    ClaudeSidecarProvider::connect_to_program(&uuid::Uuid::new_v4().to_string(), &launcher)
        .expect("the stand-in sidecar starts and answers the handshake")
}

fn create(provider: &ClaudeSidecarProvider) -> String {
    provider
        .create_session(CreateSessionRequest {
            cwd: "/".into(),
            streaming: StreamingPreference::Partial,
            project: ProjectTrust::Untrusted,
            client_tools: None,
        })
        .expect("the stand-in opens a session")
}

fn close(provider: &ClaudeSidecarProvider, session_id: String) {
    let _ = provider.close_session(CloseSessionRequest { session_id });
}

/// The `SendTurn` lines the stand-in recorded so far. A `SendTurn` is recorded before it is answered, so
/// after an answered send its line is there; after a refusal here there is none to wait for.
fn sends(scratch: &Scratch) -> Vec<String> {
    std::fs::read_to_string(scratch.sends())
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

/// The `SendTurn` requests the stand-in recorded, as it received them (images emptied), with their bytes.
fn turns(scratch: &Scratch) -> Vec<(Vec<u8>, ProtoSendTurnRequest)> {
    let bytes = std::fs::read(scratch.turns()).unwrap_or_default();
    let mut rest = bytes.as_slice();
    let mut requests = Vec::new();
    while !rest.is_empty() {
        let length = u32::from_le_bytes(rest[..4].try_into().unwrap()) as usize;
        rest = &rest[4..];
        requests.push((
            rest[..length].to_vec(),
            ProtoSendTurnRequest::decode(&rest[..length]).unwrap(),
        ));
        rest = &rest[length..];
    }
    requests
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// An image of `media_type` whose bytes start with that type's magic and run to `len`, filled with `fill`.
fn image(media_type: &str, len: usize, fill: u8) -> TurnImage {
    let magic: &[u8] = match media_type {
        "image/png" => &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a],
        "image/jpeg" => &[0xff, 0xd8, 0xff, 0xe0],
        "image/gif" => b"GIF89a",
        "image/webp" => b"RIFF\x10\0\0\0WEBPVP8 ",
        _ => b"",
    };
    let mut data = magic.to_vec();
    data.resize(len.max(magic.len()), fill);
    data.truncate(len);
    with_type(media_type, data)
}

/// An image naming `media_type` over exactly `data`.
fn with_type(media_type: &str, data: Vec<u8>) -> TurnImage {
    TurnImage {
        meta: AttachmentMeta {
            media_type: media_type.to_string(),
            bytes: data.len() as u64,
            width: 2,
            height: 2,
            sha256: hex(&sha2::Sha256::digest(&data)),
        },
        data: Arc::from(data),
    }
}

/// How the stand-in's record names `image`.
fn mark(image: &TurnImage) -> String {
    format!(
        "image:{}:{}:{}",
        image.meta.media_type,
        image.data.len(),
        hex(&sha2::Sha256::digest(&image.data))
    )
}

fn turn(session_id: &str, text: &str, images: Vec<TurnImage>) -> SendTurnRequest {
    SendTurnRequest {
        images,
        ..SendTurnRequest::text(session_id, text)
    }
}

fn refusal_words(error: ProviderError) -> String {
    match error {
        ProviderError::Provider {
            code: ProviderErrorCode::TurnContentRefused,
            message,
        } => message,
        other => panic!("not a refusal of the images: {other}"),
    }
}

/// The limits the real sidecar states, as the stand-in's `--turn-content` states them.
fn sidecar_limits() -> TurnContentLimits {
    TurnContentLimits {
        image_media_types: ["image/png", "image/jpeg", "image/gif", "image/webp"]
            .map(String::from)
            .to_vec(),
        max_images_per_turn: 8,
        max_image_bytes: 3_670_016,
        max_content_bytes: 3_670_016,
    }
}

#[test]
fn images_go_as_content_blocks_in_order() {
    let scratch = Scratch::new();
    let provider = stand_in(&scratch, "--turn-content");
    assert!(provider.capabilities().turn_content_v1);
    assert_eq!(provider.capabilities().turn_content_limits, Some(sidecar_limits()));
    let session = create(&provider);
    let images = vec![
        image("image/png", 40, 1),
        image("image/jpeg", 30, 2),
        image("image/gif", 20, 3),
        image("image/webp", 50, 4),
    ];
    let expected: String = images.iter().map(|image| format!(" {}", mark(image))).collect();

    provider
        .send_turn(turn(&session, "end", images))
        .expect("the stand-in takes the turn");

    assert_eq!(sends(&scratch), vec![format!("send - false{expected}")]);
    let recorded = turns(&scratch);
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].1.text, "end", "the text goes as it is, beside the images");
    let types: Vec<String> = recorded[0]
        .1
        .content
        .iter()
        .map(|block| match &block.block {
            Some(claude_runtime_protocol::v1::turn_content_block::Block::Image(image)) => image.media_type.clone(),
            None => "none".to_string(),
        })
        .collect();
    assert_eq!(types, ["image/png", "image/jpeg", "image/gif", "image/webp"]);
    close(&provider, session);
}

#[test]
fn an_image_only_turn_goes_with_empty_text() {
    let scratch = Scratch::new();
    let provider = stand_in(&scratch, "--turn-content");
    let session = create(&provider);
    let png = image("image/png", 64, 7);
    provider
        .send_turn(turn(&session, "", vec![png.clone()]))
        .expect("an image without text is a turn");
    assert_eq!(sends(&scratch), vec![format!("send - false {}", mark(&png))]);
    assert_eq!(turns(&scratch)[0].1.text, "");
    close(&provider, session);
}

/// A turn of text alone carries the fields a 3.25 build sent and no content. The stand-in records the request
/// as it decoded it, encoded again, so this compares fields, not the bytes on the wire.
#[test]
fn a_text_only_turn_has_the_fields_it_had_before_and_no_content() {
    let scratch = Scratch::new();
    let provider = stand_in(&scratch, "--turn-content");
    let session = create(&provider);
    provider
        .send_turn(SendTurnRequest::text(&session, "end"))
        .expect("the stand-in takes the turn");

    let recorded = turns(&scratch);
    assert_eq!(recorded.len(), 1);
    let (bytes, request) = &recorded[0];
    let before = ProtoSendTurnRequest {
        session_id: session.clone(),
        command_id: request.command_id.clone(),
        text: "end".into(),
        client_message_id: None,
        typed_by_person: false,
        content: Vec::new(),
    };
    assert_eq!(bytes, &before.encode_to_vec());
    assert_eq!(sends(&scratch), vec!["send - false".to_string()]);
    close(&provider, session);
}

#[test]
fn images_without_the_capability_are_refused_with_nothing_sent() {
    let scratch = Scratch::new();
    let provider = stand_in(&scratch, "");
    assert!(!provider.capabilities().turn_content_v1);
    let session = create(&provider);

    let refused = provider
        .send_turn(turn(&session, "end", vec![image("image/png", 40, 1)]))
        .expect_err("a sidecar that did not list turn_content_v1 would drop the images and run the text");

    assert!(
        matches!(refused, ProviderError::UnsupportedCapability("turn_content_v1")),
        "{refused}"
    );
    assert!(sends(&scratch).is_empty(), "nothing reached the sidecar");
    provider
        .send_turn(SendTurnRequest::text(&session, "end"))
        .expect("text turns go on");
    assert_eq!(sends(&scratch).len(), 1);
    close(&provider, session);
}

/// Every limit the handshake stated is held before anything is sent, with the sidecar's own order and in
/// the composer's words; at each limit the turn goes. The stand-in states narrow limits of its own, so
/// the ones held are the ones it stated.
#[test]
fn each_advertised_limit_is_checked_before_sending() {
    let scratch = Scratch::new();
    let provider = stand_in(&scratch, "--content-limits 2/64/100/image/png,image/gif");
    let limits = provider
        .capabilities()
        .turn_content_limits
        .expect("the stand-in states its limits");
    assert_eq!(limits.max_images_per_turn, 2);
    let session = create(&provider);
    let words = |reason: &str| turn_content::words(reason, Some(&limits));
    let refused = |images: Vec<TurnImage>| {
        refusal_words(
            provider
                .send_turn(turn(&session, "end", images))
                .expect_err("over a stated limit"),
        )
    };

    assert_eq!(
        refused(vec![image("image/png", 10, 1); 3]),
        words(turn_content::TOO_MANY)
    );
    assert_eq!(
        refused(vec![image("image/jpeg", 10, 1)]),
        words(turn_content::MEDIA_TYPE),
        "a type the sidecar did not list"
    );
    assert_eq!(
        refused(vec![with_type("image/png", Vec::new())]),
        words(turn_content::EMPTY_IMAGE)
    );
    assert_eq!(
        refused(vec![image("image/png", 65, 1)]),
        words(turn_content::IMAGE_TOO_LARGE)
    );
    assert_eq!(
        refused(vec![with_type("image/gif", image("image/png", 10, 1).data.to_vec())]),
        words(turn_content::MAGIC_MISMATCH),
        "the bytes are not the type named"
    );
    assert_eq!(
        refused(vec![image("image/png", 50, 1), image("image/gif", 51, 2)]),
        words(turn_content::TOO_LARGE)
    );
    assert!(
        sends(&scratch).is_empty(),
        "nothing reached the sidecar: {:?}",
        sends(&scratch)
    );

    let at_the_limits = vec![image("image/png", 64, 1), image("image/gif", 36, 2)];
    let expected: String = at_the_limits.iter().map(|image| format!(" {}", mark(image))).collect();
    provider
        .send_turn(turn(&session, "end", at_the_limits))
        .expect("at every limit the turn goes");
    assert_eq!(sends(&scratch), vec![format!("send - false{expected}")]);
    close(&provider, session);
}

/// The images at the sidecar's content limit fit its 4 MiB message with the text beside them; text long
/// enough to take the request past the message limit is refused here, as the images' refusal.
#[test]
fn a_turn_over_the_message_limit_is_refused_locally() {
    let scratch = Scratch::new();
    let provider = stand_in(&scratch, "--turn-content");
    let session = create(&provider);
    let full = || vec![image("image/png", 3_670_016, 9)];

    let long_text = "x".repeat(600_000);
    let refused = provider
        .send_turn(turn(&session, &long_text, full()))
        .expect_err("text and images together pass the message limit");
    let words = refusal_words(refused);
    assert!(
        words.starts_with("this message is too large to send: its text and its images together"),
        "the text is named, not only the images: {words}"
    );
    assert!(sends(&scratch).is_empty(), "nothing reached the sidecar");

    provider
        .send_turn(turn(&session, "end", full()))
        .expect("the images at the content limit fit with short text");
    assert_eq!(sends(&scratch).len(), 1);
    close(&provider, session);
}

/// The sidecar's own refusal of content (`INVALID_CONFIGURATION`, a `turn_content_*` reason) is the
/// images' refusal, benign: the session goes on.
#[test]
fn the_sidecar_s_own_refusal_of_content_keeps_the_session() {
    let scratch = Scratch::new();
    let provider = stand_in(&scratch, "--turn-content --refuse-content turn_content_magic_mismatch");
    let session = create(&provider);

    let refused = provider
        .send_turn(turn(&session, "end", vec![image("image/png", 40, 1)]))
        .expect_err("the stand-in refuses every turn with content");
    assert!(refused.is_benign(), "{refused}");
    assert_eq!(
        refusal_words(refused),
        turn_content::words(turn_content::MAGIC_MISMATCH, Some(&sidecar_limits()))
    );

    provider
        .send_turn(SendTurnRequest::text(&session, "end"))
        .expect("the session takes the next turn");
    assert_eq!(sends(&scratch), vec!["send - false".to_string()]);
    close(&provider, session);
}

/// `SteerTurnRequest` carries no images, so images reach the agent only as a turn of their own: a send
/// with images while a turn runs is refused as busy, with nothing sent, and never becomes a steer.
#[test]
fn a_steer_with_images_is_refused() {
    let scratch = Scratch::new();
    let provider = Arc::new(stand_in(&scratch, "--turn-content"));
    let cwd = agent::state_dirs::test_workspace_dir("turn-content-steer");
    let mut conversation =
        AgentConversation::create(provider.clone(), &cwd, ProjectTrust::Untrusted).expect("a conversation opens");
    wait(&mut conversation, "the session opening", |p| p.session_id.is_some());
    let gate = scratch.0.join("gate");
    std::fs::create_dir(&gate).unwrap();
    conversation
        .send_turn_as(
            &format!("gate {};end", gate.display()),
            "hold",
            PromptOrigin::Typed,
            "first",
            Vec::new(),
        )
        .expect("the first turn goes");
    wait(&mut conversation, "the turn running", |p| p.active_turn_id.is_some());

    let refused = conversation
        .send_turn_as(
            "end",
            "look",
            PromptOrigin::Typed,
            "second",
            vec![image("image/png", 40, 1)],
        )
        .expect_err("a turn runs");
    assert!(matches!(refused, ConversationError::TurnAlreadyActive), "{refused}");
    assert_eq!(sends(&scratch).len(), 1, "only the first turn reached the sidecar");

    std::fs::write(gate.join("release"), b"").ok();
    conversation.shutdown();
}

/// Waits until the conversation's projection satisfies `done`.
fn wait(conversation: &mut AgentConversation, what: &str, done: impl Fn(&AgentSessionProjection) -> bool) {
    let started = Instant::now();
    loop {
        let _ = conversation.take_ui_delivery();
        if done(&conversation.projection()) {
            return;
        }
        assert!(started.elapsed() < HANG, "{what} never happened");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The prompt's record keeps what its images were (type, size, size in pixels, digest) and never their
/// bytes; a prompt of text alone serializes as it did before images existed.
#[test]
fn the_prompt_record_keeps_attachment_metadata_and_no_bytes() {
    let png = image("image/png", 40, 0x5a);
    let mut projection = AgentSessionProjection::default();
    projection.apply(&AgentDomainEvent::UserPromptSubmitted { text: "before".into() });
    projection.apply(&AgentDomainEvent::UserPromptSubmitted { text: "look".into() });
    projection.apply(&AgentDomainEvent::UserPromptAttachments {
        attachments: vec![png.meta.clone()],
    });

    assert!(
        projection.user_prompts[0].attachments.is_empty(),
        "only the newest prompt's"
    );
    assert_eq!(projection.user_prompts[1].attachments, vec![png.meta.clone()]);
    let text_alone = serde_json::to_value(&projection.user_prompts[0]).unwrap();
    assert!(text_alone.get("attachments").is_none(), "{text_alone}");
    let with_images = serde_json::to_value(&projection.user_prompts[1]).unwrap();
    assert_eq!(
        with_images["attachments"],
        serde_json::json!([{
            "media_type": "image/png", "bytes": 40, "width": 2, "height": 2, "sha256": png.meta.sha256,
        }])
    );
    let read: agent::UserPromptRecord = serde_json::from_value(text_alone).unwrap();
    assert!(read.attachments.is_empty());
}
