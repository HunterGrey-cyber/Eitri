//! A turn's images through [`AgentBackend::send_turn`] and [`AgentBackend::send_turn_in`]: they reach only a
//! provider that takes them, the prompt's row records what each image was and never its bytes, and a turn
//! whose images are refused leaves no row and keeps the session.

use super::*;
use crate::test_providers::{empty_trust_plan, ExternalRecordingProvider, RecordingProvider};
use std::sync::Arc;

fn workspace(label: &str) -> PathBuf {
    agent::state_dirs::redirect_state_to_a_test_root();
    agent::state_dirs::test_workspace_dir(label)
}

fn claude_on(provider: RecordingProvider, dir: &Path) -> (Arc<RecordingProvider>, AgentBackend) {
    let provider = Arc::new(provider);
    let conversation = AgentConversation::create(provider.clone(), dir, ProjectTrust::Untrusted).unwrap();
    (
        provider,
        AgentBackend::Conversation {
            agent: AgentId::Claude,
            conversation: Box::new(conversation),
        },
    )
}

/// A small PNG-shaped image whose bytes and digest are its own.
fn png(tag: u8) -> agent::TurnImage {
    let mut data = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    data.extend([tag; 24]);
    agent::TurnImage {
        meta: agent::AttachmentMeta {
            media_type: "image/png".into(),
            bytes: data.len() as u64,
            width: 4,
            height: 3,
            sha256: format!("{tag:02x}").repeat(32),
        },
        data: Arc::from(data),
    }
}

#[test]
fn an_image_turn_sends_its_images_and_its_row_keeps_what_they_were() {
    let dir = workspace("images-sent");
    let (provider, mut backend) = claude_on(RecordingProvider::image_taking(), &dir);
    let images = vec![png(1), png(2)];
    let metas: Vec<agent::AttachmentMeta> = images.iter().map(|image| image.meta.clone()).collect();

    backend
        .send_turn("wire", "look at these", agent::PromptOrigin::Typed, images)
        .expect("a provider that takes images takes the turn");

    assert_eq!(provider.turn_images(), vec![metas.clone()], "both images, in order");
    let prompts = &backend.projection().user_prompts;
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0].text, "look at these");
    assert_eq!(prompts[0].attachments, metas);
    let row = serde_json::to_value(&prompts[0]).unwrap();
    assert_eq!(row["attachments"][1]["sha256"], metas[1].sha256);
    // The row names each image by these fields alone: a field added for the bytes would show here.
    for attachment in row["attachments"].as_array().unwrap() {
        let mut keys: Vec<&str> = attachment.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["bytes", "height", "media_type", "sha256", "width"], "{row}");
    }
}

/// The agent had a turn open that this side had not folded (work Claude Code started, a start event not yet
/// read): the projection shows no turn, the provider refuses the send as busy. A turn with images is refused in
/// words the tab tells apart from a text turn's busy refusal, which it would put into the running turn as text.
#[test]
fn an_image_turn_refused_as_busy_by_an_unseen_turn_is_never_a_steer() {
    let dir = workspace("images-busy");
    let (provider, mut backend) = claude_on(RecordingProvider::image_taking(), &dir);
    provider.refuse_sends(true);
    assert!(backend.projection().active_turn_id.is_none(), "this side sees no turn");

    let error = backend
        .send_turn("wire", "look", agent::PromptOrigin::Typed, vec![png(1)])
        .expect_err("the agent is busy");

    assert!(error.benign, "the session stays");
    assert!(!error.is_turn_busy(), "never held to be put into the running turn");
    assert!(error.is_turn_busy_with_images());
    assert!(backend.projection().user_prompts.is_empty(), "its row is withdrawn");

    let text = backend
        .send_turn("wire", "words", agent::PromptOrigin::Typed, Vec::new())
        .expect_err("still busy");
    assert!(
        text.is_turn_busy() && !text.is_turn_busy_with_images(),
        "a text turn is held as before"
    );
}

#[test]
fn a_sidecar_listing_images_without_limits_is_refused_before_anything_is_sent() {
    let dir = workspace("images-no-limits");
    let (provider, mut backend) = claude_on(RecordingProvider::image_taking_without_limits(), &dir);
    assert!(backend.why_no_images().is_some(), "the page is not offered image input");

    let error = backend
        .send_turn("wire", "look", agent::PromptOrigin::Typed, vec![png(1)])
        .expect_err("no limits stated");

    assert!(error.benign, "the session stays");
    assert!(provider.turns().is_empty(), "nothing reached the provider");
    assert!(backend.projection().user_prompts.is_empty());
    backend
        .send_turn("wire", "words", agent::PromptOrigin::Typed, Vec::new())
        .expect("text still goes");
}

#[test]
fn a_message_starting_with_slash_with_images_is_refused_before_anything_is_sent() {
    let dir = workspace("images-command");
    let (provider, mut backend) = claude_on(RecordingProvider::image_taking(), &dir);
    for text in ["  /review", "/tmp/screenshot.png"] {
        let error = backend
            .send_turn(text, text, agent::PromptOrigin::Typed, vec![png(1)])
            .expect_err("a leading slash takes no images");
        assert!(error.benign);
        assert_eq!(
            error.message,
            "a message starting with / takes no images: remove the images first"
        );
        assert!(provider.turns().is_empty(), "nothing reached the provider");
        assert!(backend.projection().user_prompts.is_empty());
    }
}

#[test]
fn a_text_turn_s_row_has_no_attachments_field() {
    let dir = workspace("images-none");
    let (provider, mut backend) = claude_on(RecordingProvider::image_taking(), &dir);
    backend
        .send_turn("wire", "just words", agent::PromptOrigin::Typed, Vec::new())
        .unwrap();
    assert_eq!(provider.turn_images(), vec![Vec::<agent::AttachmentMeta>::new()]);
    let row = serde_json::to_value(&backend.projection().user_prompts[0]).unwrap();
    assert!(row.get("attachments").is_none(), "{row}");
}

#[test]
fn refused_images_take_their_row_back_and_keep_the_session() {
    let dir = workspace("images-refused");
    let (provider, mut backend) = claude_on(RecordingProvider::image_taking(), &dir);
    provider.refuse_content(Some("an image is empty"));

    let error = backend
        .send_turn("wire", "look", agent::PromptOrigin::Typed, vec![png(1)])
        .expect_err("the provider refused the images");

    assert!(error.benign, "the session stays");
    assert!(!error.is_turn_busy(), "never held to be put into a running turn");
    assert_eq!(error.message, "an image is empty", "the provider's words as they stand");
    assert!(
        backend.projection().user_prompts.is_empty(),
        "no row for a prompt that was never sent"
    );
    provider.refuse_content(None);
    backend
        .send_turn("wire", "again", agent::PromptOrigin::Typed, Vec::new())
        .expect("the session takes the next turn");
}

#[test]
fn a_tab_whose_sidecar_takes_no_images_refuses_them_before_anything_is_folded_or_sent() {
    let dir = workspace("images-unoffered");
    let (provider, mut backend) = claude_on(RecordingProvider::default(), &dir);

    let error = backend
        .send_turn("wire", "look", agent::PromptOrigin::Typed, vec![png(1)])
        .expect_err("no turn_content_v1");

    assert!(error.benign);
    assert_eq!(
        error.message,
        "this tab cannot take images: its Claude sidecar does not accept them"
    );
    assert!(provider.turns().is_empty(), "nothing reached the provider");
    assert!(backend.projection().user_prompts.is_empty());
}

#[test]
fn an_external_agent_s_tab_refuses_images_before_anything_is_folded_or_sent() {
    let dir = workspace("images-external");
    let provider = Arc::new(ExternalRecordingProvider::codex());
    let mut backend = AgentBackend::start_external_with(
        AgentId::Codex,
        provider.clone(),
        &dir,
        empty_trust_plan(),
        SessionModeChoice::Auto,
        None,
    )
    .expect("a scripted external start succeeds");

    let error = backend
        .send_turn_in(
            "wire",
            "look",
            SessionModeChoice::Auto,
            Some(&dir),
            agent::PromptOrigin::Typed,
            vec![png(1)],
        )
        .expect_err("Codex turns are text");

    assert!(error.benign);
    assert_eq!(error.message, "this tab cannot take images: Codex takes text only here");
    assert!(provider.policies().is_empty(), "nothing reached the provider");
    assert!(backend.projection().user_prompts.is_empty());
}

#[cfg(feature = "legacy-backend")]
#[test]
fn the_legacy_backend_refuses_images_before_anything_is_folded_or_sent() {
    let mut backend = AgentBackend::Legacy(agent::AgentSession::for_tests());
    let error = backend
        .send_turn("wire", "look", agent::PromptOrigin::Typed, vec![png(1)])
        .expect_err("the legacy CLI wire carries text");
    assert!(error.benign);
    assert_eq!(
        error.message,
        "this tab cannot take images: its backend sends text only"
    );
    assert!(backend.projection().user_prompts.is_empty());
    assert!(backend.projection().active_turn_id.is_none(), "no turn started");
}
