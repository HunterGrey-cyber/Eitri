//! Images on a Claude turn (protocol 3.26, `turn_content_v1`): the `SendTurn` content blocks, and the
//! checks made before a turn with images leaves, so that one the sidecar would refuse is refused here
//! with nothing sent.
//!
//! The sidecar accepts content only when its handshake lists `turn_content_v1`; an older one ignores the
//! field and would run the text without the images. So images go only to a sidecar that listed it, and
//! only within the `turn_content_limits` it stated: count, media type, magic bytes, size per image and
//! in total, in the sidecar's own order, refused with its own reason words. The whole request must also
//! fit the handshake's message limit, with room for what gRPC adds around it.
//!
//! A turn without images is built and sent exactly as before: no content field on the wire.

use crate::provider::{ProviderCapabilities, ProviderError, ProviderErrorCode, TurnContentLimits, TurnImage};
use claude_runtime_protocol::v1::{turn_content_block, ErrorCode as ProtoErrorCode, ImageContent, TurnContentBlock};

/// An image names a media type the sidecar does not take, or a block has no kind it knows.
pub const MEDIA_TYPE: &str = "turn_content_media_type";
/// An image's bytes do not start with the magic of the type it names.
pub const MAGIC_MISMATCH: &str = "turn_content_magic_mismatch";
/// An image has no bytes.
pub const EMPTY_IMAGE: &str = "turn_content_empty_image";
/// More images than one turn may carry.
pub const TOO_MANY: &str = "turn_content_too_many";
/// One image is larger than one image may be.
pub const IMAGE_TOO_LARGE: &str = "turn_content_image_too_large";
/// The images together are larger than one turn may carry, or the request with them would not fit the
/// message limit.
pub const TOO_LARGE: &str = "turn_content_too_large";

/// Composer budgets of the bundled sidecar; sends also obey the limits of the connected sidecar.
pub const BUNDLED_MAX_IMAGES_PER_TURN: usize = 8;
pub const BUNDLED_MAX_IMAGE_BYTES: u64 = 3_670_016;
pub const BUNDLED_MAX_CONTENT_BYTES: u64 = 3_670_016;
pub const BUNDLED_MAX_MESSAGE_BYTES: u64 = 4 * 1024 * 1024;

/// What a request leaves for gRPC's own framing under the handshake's `max_message_bytes`.
pub const MESSAGE_MARGIN_BYTES: u64 = 4096;

/// The media type `bytes` start like, by the same magic the sidecar reads: PNG's signature, JPEG's
/// `0xFF 0xD8 0xFF`, `GIF87a`/`GIF89a`, and `RIFF` with `WEBP` at offset 8. It says what the bytes claim to
/// be; it decodes nothing.
pub fn sniff(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP".as_slice()) {
        Some("image/webp")
    } else {
        None
    }
}

/// The first of `limits` that `images` break, as the sidecar's reason word, in the order the sidecar
/// checks: the count, then each image in turn (its media type, that it has bytes, its size, its magic)
/// and the running total.
pub fn check(images: &[TurnImage], limits: &TurnContentLimits) -> Result<(), &'static str> {
    if images.len() as u64 > u64::from(limits.max_images_per_turn) {
        return Err(TOO_MANY);
    }
    let mut total: u64 = 0;
    for image in images {
        let media_type = image.meta.media_type.as_str();
        if !limits.image_media_types.iter().any(|known| known == media_type) {
            return Err(MEDIA_TYPE);
        }
        let len = image.data.len() as u64;
        if len == 0 {
            return Err(EMPTY_IMAGE);
        }
        if len > limits.max_image_bytes {
            return Err(IMAGE_TOO_LARGE);
        }
        if sniff(&image.data) != Some(media_type) {
            return Err(MAGIC_MISMATCH);
        }
        total = total.saturating_add(len);
        if total > limits.max_content_bytes {
            return Err(TOO_LARGE);
        }
    }
    Ok(())
}

/// The content blocks of a turn carrying `images`, in their order, once the sidecar is known to take
/// them: empty for a turn without images, whatever the capabilities. Refused with nothing built when
/// the sidecar did not list `turn_content_v1`, stated no limits, or `images` break them.
pub(super) fn blocks(
    images: &[TurnImage],
    capabilities: &ProviderCapabilities,
) -> Result<Vec<TurnContentBlock>, ProviderError> {
    if images.is_empty() {
        return Ok(Vec::new());
    }
    if !capabilities.turn_content_v1 {
        return Err(ProviderError::UnsupportedCapability("turn_content_v1"));
    }
    // Content is sent only within limits the sidecar stated; one that listed the capability and stated
    // none gives nothing to stay within.
    let Some(limits) = capabilities.turn_content_limits.as_ref() else {
        return Err(ProviderError::UnsupportedCapability("turn_content_limits"));
    };
    check(images, limits).map_err(|reason| refused(reason, Some(limits)))?;
    Ok(images
        .iter()
        .map(|image| TurnContentBlock {
            block: Some(turn_content_block::Block::Image(ImageContent {
                media_type: image.meta.media_type.clone(),
                data: image.data.to_vec(),
            })),
        })
        .collect())
}

/// Refuses a request carrying content whose encoding would not fit `max_message_bytes` less
/// [`MESSAGE_MARGIN_BYTES`] (the text and the images together). 0 says the peer stated no limit, and
/// then only the content limits apply. A request without content is not measured here: it is sent as
/// it always was.
pub(super) fn fits(
    request: &claude_runtime_protocol::v1::SendTurnRequest,
    max_message_bytes: u64,
) -> Result<(), ProviderError> {
    if request.content.is_empty() || max_message_bytes == 0 {
        return Ok(());
    }
    let len = prost::Message::encoded_len(request) as u64;
    let room = max_message_bytes.saturating_sub(MESSAGE_MARGIN_BYTES);
    if len > room {
        return Err(ProviderError::Provider {
            code: ProviderErrorCode::TurnContentRefused,
            message: message_too_large(room),
        });
    }
    Ok(())
}

/// What the composer says when the text and the images together do not fit one message. The images alone fit
/// (their limits are checked first), so it names both: shortening the text is usually the way out.
pub fn message_too_large(room: u64) -> String {
    format!(
        "this message is too large to send: its text and its images together must stay under {}; shorten the text \
         or send fewer images",
        megabytes(room)
    )
}

/// The refusal of a turn's images for `reason`, worded for the composer.
pub(super) fn refused(reason: &str, limits: Option<&TurnContentLimits>) -> ProviderError {
    ProviderError::Provider {
        code: ProviderErrorCode::TurnContentRefused,
        message: words(reason, limits),
    }
}

/// The sidecar's own refusal of a turn's content, read from its status: `INVALID_CONFIGURATION` with a
/// `turn_content_*` reason, which it gives before the kernel is called or anything is recorded. Any other
/// status is not this, and is classified as every other failed call is.
pub(super) fn sidecar_refusal(
    detail: &claude_runtime_protocol::v1::ErrorDetail,
    limits: Option<&TurnContentLimits>,
) -> Option<ProviderError> {
    (detail.code() == ProtoErrorCode::InvalidConfiguration && detail.reason.starts_with("turn_content_"))
        .then(|| refused(&detail.reason, limits))
}

/// What the composer says for a refusal with `reason`: in Eitri's words, with the limit when the sidecar
/// stated one. A reason this build does not know is named after a fixed sentence, with only the
/// characters a reason word has.
pub fn words(reason: &str, limits: Option<&TurnContentLimits>) -> String {
    match reason {
        TOO_MANY => match limits {
            Some(limits) => format!(
                "too many images for one message (at most {})",
                limits.max_images_per_turn
            ),
            None => "too many images for one message".to_string(),
        },
        MEDIA_TYPE => "an image is of a kind that cannot be sent (PNG, JPEG, GIF and WebP can)".to_string(),
        MAGIC_MISMATCH => "an image's bytes are not the kind of image it says it is".to_string(),
        EMPTY_IMAGE => "an image is empty".to_string(),
        IMAGE_TOO_LARGE => match limits {
            Some(limits) => format!(
                "an image is too large to send (at most {} each)",
                megabytes(limits.max_image_bytes)
            ),
            None => "an image is too large to send".to_string(),
        },
        TOO_LARGE => match limits {
            Some(limits) => format!(
                "the images are too large to send in one message (at most {} together)",
                megabytes(limits.max_content_bytes)
            ),
            None => "the images are too large to send in one message".to_string(),
        },
        other => {
            let word: String = other
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
                .take(64)
                .collect();
            format!("the images were refused ({word})")
        }
    }
}

/// `bytes` in MiB, as "3.5 MB", one decimal and none when it is whole.
fn megabytes(bytes: u64) -> String {
    let tenths = (bytes.saturating_mul(10) + 524_288) / 1_048_576;
    if tenths % 10 == 0 {
        format!("{} MB", tenths / 10)
    } else {
        format!("{}.{} MB", tenths / 10, tenths % 10)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::AttachmentMeta;
    use std::sync::Arc;

    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13];
    const JPEG: &[u8] = &[0xff, 0xd8, 0xff, 0xe0, 0, 16];

    fn limits() -> TurnContentLimits {
        TurnContentLimits {
            image_media_types: ["image/png", "image/jpeg", "image/gif", "image/webp"]
                .map(String::from)
                .to_vec(),
            max_images_per_turn: BUNDLED_MAX_IMAGES_PER_TURN as u32,
            max_image_bytes: BUNDLED_MAX_IMAGE_BYTES,
            max_content_bytes: BUNDLED_MAX_CONTENT_BYTES,
        }
    }

    fn image(media_type: &str, data: Vec<u8>) -> TurnImage {
        TurnImage {
            meta: AttachmentMeta {
                media_type: media_type.to_string(),
                bytes: data.len() as u64,
                width: 1,
                height: 1,
                sha256: String::new(),
            },
            data: Arc::from(data),
        }
    }

    /// `len` bytes that start as a PNG.
    fn png_of(len: usize) -> Vec<u8> {
        let mut data = PNG.to_vec();
        data.resize(len, 0);
        data
    }

    #[test]
    fn the_content_budget_refusal_names_only_the_images() {
        assert_eq!(
            words(TOO_LARGE, Some(&limits())),
            "the images are too large to send in one message (at most 3.5 MB together)"
        );
    }

    #[test]
    fn sniffing_tells_the_four_formats_apart_and_refuses_others() {
        assert_eq!(sniff(PNG), Some("image/png"));
        assert_eq!(sniff(JPEG), Some("image/jpeg"));
        assert_eq!(sniff(b"GIF87a...."), Some("image/gif"));
        assert_eq!(sniff(b"GIF89a...."), Some("image/gif"));
        assert_eq!(sniff(b"RIFF\x10\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(sniff(b"RIFF\x10\0\0\0WAVEfmt "), None, "a RIFF that is not WebP");
        assert_eq!(sniff(b"RIFF\x10\0\0\0WEB"), None, "cut short before the WebP mark");
        assert_eq!(sniff(b"GIF88a"), None);
        assert_eq!(sniff(&PNG[..7]), None, "half a PNG signature");
        assert_eq!(sniff(b"<svg"), None);
        assert_eq!(sniff(b""), None);
    }

    #[test]
    fn each_limit_is_refused_exactly_one_over_and_accepted_at_the_limit() {
        let limits = limits();
        let at = |n: usize| vec![image("image/png", png_of(16)); n];
        assert_eq!(check(&at(8), &limits), Ok(()));
        assert_eq!(check(&at(9), &limits), Err(TOO_MANY));

        let one = |len: usize| vec![image("image/png", png_of(len))];
        assert_eq!(check(&one(3_670_016), &limits), Ok(()));
        assert_eq!(check(&one(3_670_017), &limits), Err(IMAGE_TOO_LARGE));

        let two = |second: usize| {
            vec![
                image("image/png", png_of(3_000_000)),
                image("image/png", png_of(second)),
            ]
        };
        assert_eq!(check(&two(670_016), &limits), Ok(()));
        assert_eq!(check(&two(670_017), &limits), Err(TOO_LARGE));
    }

    #[test]
    fn each_image_is_checked_for_its_type_bytes_and_magic() {
        let limits = limits();
        assert_eq!(check(&[image("image/svg+xml", png_of(16))], &limits), Err(MEDIA_TYPE));
        assert_eq!(check(&[image("image/png", Vec::new())], &limits), Err(EMPTY_IMAGE));
        assert_eq!(check(&[image("image/jpeg", png_of(16))], &limits), Err(MAGIC_MISMATCH));
        assert_eq!(check(&[image("image/jpeg", JPEG.to_vec())], &limits), Ok(()));
        let narrow = TurnContentLimits {
            image_media_types: vec!["image/png".to_string()],
            ..limits.clone()
        };
        assert_eq!(
            check(&[image("image/jpeg", JPEG.to_vec())], &narrow),
            Err(MEDIA_TYPE),
            "only the types the sidecar listed"
        );
        // The sidecar's order: an image's type before its bytes, its size before its magic.
        assert_eq!(check(&[image("image/bmp", Vec::new())], &limits), Err(MEDIA_TYPE));
        let mut big_lie = vec![0u8; 3_670_017];
        big_lie[..3].copy_from_slice(&[0xff, 0xd8, 0xff]);
        assert_eq!(check(&[image("image/png", big_lie)], &limits), Err(IMAGE_TOO_LARGE));
    }

    #[test]
    fn every_reason_has_words_and_an_unknown_one_is_named_plainly() {
        let limits = limits();
        for reason in [
            MEDIA_TYPE,
            MAGIC_MISMATCH,
            EMPTY_IMAGE,
            TOO_MANY,
            IMAGE_TOO_LARGE,
            TOO_LARGE,
        ] {
            let said = words(reason, Some(&limits));
            assert!(!said.contains("turn_content"), "{reason}: {said}");
            assert!(!words(reason, None).is_empty());
        }
        assert_eq!(
            words(TOO_MANY, Some(&limits)),
            "too many images for one message (at most 8)"
        );
        assert_eq!(
            words(IMAGE_TOO_LARGE, Some(&limits)),
            "an image is too large to send (at most 3.5 MB each)"
        );
        assert_eq!(
            words("turn_content_new\u{202e}<b>", None),
            "the images were refused (turn_content_newb)"
        );
    }

    #[test]
    fn blocks_need_the_capability_and_stated_limits_and_a_text_turn_needs_neither() {
        let images = vec![image("image/png", png_of(16)), image("image/jpeg", JPEG.to_vec())];
        let none = ProviderCapabilities::default();
        assert!(blocks(&[], &none).unwrap().is_empty(), "a text turn builds no content");
        assert!(matches!(
            blocks(&images, &none),
            Err(ProviderError::UnsupportedCapability("turn_content_v1"))
        ));
        let unlimited = ProviderCapabilities {
            turn_content_v1: true,
            ..ProviderCapabilities::default()
        };
        assert!(matches!(
            blocks(&images, &unlimited),
            Err(ProviderError::UnsupportedCapability("turn_content_limits"))
        ));
        let offered = ProviderCapabilities {
            turn_content_limits: Some(limits()),
            ..unlimited
        };
        let built = blocks(&images, &offered).unwrap();
        let sent: Vec<(String, Vec<u8>)> = built
            .into_iter()
            .map(|block| match block.block {
                Some(turn_content_block::Block::Image(image)) => (image.media_type, image.data),
                None => panic!("a block without a kind"),
            })
            .collect();
        assert_eq!(
            sent,
            vec![
                ("image/png".to_string(), png_of(16)),
                ("image/jpeg".to_string(), JPEG.to_vec())
            ]
        );
    }

    #[test]
    fn only_a_request_with_content_is_measured_against_the_message_limit() {
        let text_alone = claude_runtime_protocol::v1::SendTurnRequest {
            text: "x".repeat(10_000),
            ..Default::default()
        };
        assert!(fits(&text_alone, 5_000).is_ok(), "a text turn is sent as it always was");
        let with_content = claude_runtime_protocol::v1::SendTurnRequest {
            content: vec![TurnContentBlock {
                block: Some(turn_content_block::Block::Image(ImageContent {
                    media_type: "image/png".into(),
                    data: png_of(1_000),
                })),
            }],
            ..Default::default()
        };
        let len = prost::Message::encoded_len(&with_content) as u64;
        assert!(fits(&with_content, len + MESSAGE_MARGIN_BYTES).is_ok(), "at the limit");
        let over = fits(&with_content, len + MESSAGE_MARGIN_BYTES - 1).unwrap_err();
        assert!(matches!(
            over,
            ProviderError::Provider {
                code: ProviderErrorCode::TurnContentRefused,
                ref message,
            } if message.contains("text and its images together") && !message.starts_with("the images are too large")
        ));
        assert!(
            fits(&with_content, 0).is_ok(),
            "no stated limit, only the content limits"
        );
    }

    #[test]
    fn a_sidecar_refusal_is_the_images_only_with_its_code_and_a_content_reason() {
        use claude_runtime_protocol::v1::ErrorDetail;
        let detail = |code: ProtoErrorCode, reason: &str| ErrorDetail {
            code: code as i32,
            message: "refused".into(),
            reason: reason.into(),
        };
        let refused = sidecar_refusal(&detail(ProtoErrorCode::InvalidConfiguration, TOO_MANY), Some(&limits()));
        assert!(matches!(
            refused,
            Some(ProviderError::Provider { code: ProviderErrorCode::TurnContentRefused, ref message })
                if message == "too many images for one message (at most 8)"
        ));
        assert!(sidecar_refusal(
            &detail(ProtoErrorCode::InvalidConfiguration, "tool_policy_empty_name"),
            None
        )
        .is_none());
        assert!(sidecar_refusal(&detail(ProtoErrorCode::SessionNotFound, TOO_MANY), None).is_none());
    }

    #[test]
    fn megabytes_are_rounded_to_a_tenth() {
        assert_eq!(megabytes(3_670_016), "3.5 MB");
        assert_eq!(megabytes(4 * 1_048_576), "4 MB");
        assert_eq!(megabytes(0), "0 MB");
    }
}
