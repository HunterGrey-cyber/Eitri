//! Private, bounded image preparation for composer attachments.
//!
//! File operations and normalization belong on a worker, never the window or provider runtime thread. Decoding
//! happens only in a child process (`isolate`), so a decoder's runaway allocation cannot end the window.

mod desk;
mod gif;
mod isolate;
mod normalize;
mod staging;
mod webp;
mod worker;

pub use desk::start_desk;
#[cfg(test)]
pub(crate) use isolate::tests::test_decoder;
pub use isolate::{
    decoder_child_main, decoder_setup_failure, normalize_in_child, DecoderCommand, DECODER_ADDRESS_HEADROOM,
    DECODER_CPU_SECONDS, DECODER_WALL_SECONDS, SUBCOMMAND,
};
#[cfg(test)]
pub(crate) use normalize::tests::png as test_png;
pub use staging::{sweep_stale_staging, AttachmentId, RawReady, Staging, StagingRefusal};
pub use worker::{NormalizeDone, NormalizeJob, Worker, WorkerBusy};

pub const RAW_MAX_BYTES: u64 = 33_554_432;
pub const MAX_PIXELS: u64 = 40_000_000;
pub const DECODE_MAX_ALLOC: u64 = 268_435_456;
pub const OUT_MAX_SIDE: u32 = 1568;
pub const OUT_MAX_AREA: u64 = 1_150_000;
pub const JPEG_QUALITY: u8 = 85;
pub const THUMB_MAX_SIDE: u32 = 256;
pub const THUMB_MAX_BYTES: usize = 262_144;
pub const WINDOW_QUOTA_BYTES: u64 = 67_108_864;
pub const CHUNK_MAX_BYTES: usize = 262_144;
pub const SESSION_WARN_BYTES: u64 = 16_777_216;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageType {
    Png,
    Jpeg,
    Gif,
    Webp,
}

impl ImageType {
    pub fn media_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
        }
    }

    pub fn sniff(bytes: &[u8]) -> Option<Self> {
        sniff(bytes)
    }
}

pub fn sniff(bytes: &[u8]) -> Option<ImageType> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(ImageType::Png)
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some(ImageType::Jpeg)
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some(ImageType::Gif)
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some(ImageType::Webp)
    } else {
        None
    }
}

// No Debug or serialization: full image bytes must stay out of logs and page/projection state.
pub struct Normalized {
    pub image_type: ImageType,
    pub width: u32,
    pub height: u32,
    pub bytes: Vec<u8>,
    pub sha256: [u8; 32],
    pub thumbnail_png: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImageRefusal {
    NotAnImage,
    UnsupportedType,
    Undecodable,
    DecoderUnavailable(String),
    FrameOutsideScreen,
    CanvasBuffersTooLarge,
    TooManyPixels { width: u32, height: u32 },
    TooLargeRaw { bytes: u64 },
    TooLargeAfterResize { bytes: u64, limit: u64 },
    TooLargeToDecode,
    Empty,
}

impl ImageRefusal {
    pub fn sentence(&self) -> String {
        match self {
            Self::NotAnImage => "this file is not an image".into(),
            Self::UnsupportedType => "unsupported image type: use PNG, JPEG, GIF or WebP".into(),
            Self::Undecodable => "this image could not be decoded".into(),
            Self::DecoderUnavailable(why) => {
                format!("images cannot be prepared here: the image decoder could not run ({why})")
            }
            Self::FrameOutsideScreen => "this GIF's frame extends outside its logical screen".into(),
            Self::CanvasBuffersTooLarge => format!(
                "this WebP's canvas buffers exceed the decode memory limit ({} MB)",
                DECODE_MAX_ALLOC / 1_048_576
            ),
            Self::TooManyPixels { width, height } => {
                format!("image too large: {width} × {height} pixels (limits: 20,000 pixels per side and 40 megapixels)")
            }
            Self::TooLargeRaw { bytes } => {
                format!("image too large: {} MB (the limit is 32 MB)", bytes.div_ceil(1_048_576))
            }
            Self::TooLargeAfterResize { bytes, limit } => {
                format!("image still too large after resizing: {bytes} bytes (the limit is {limit} bytes)")
            }
            Self::TooLargeToDecode => "image too large to decode".into(),
            Self::Empty => "this image file is empty".into(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct StagedImage {
    pub id: AttachmentId,
    pub image_type: ImageType,
    pub width: u32,
    pub height: u32,
    pub bytes: u64,
    pub sha256: [u8; 32],
    pub thumbnail_png: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_decode_failure_names_no_universal_memory_figure() {
        assert_eq!(ImageRefusal::TooLargeToDecode.sentence(), "image too large to decode");
    }

    #[test]
    fn every_refusal_has_a_sentence() {
        for refusal in [
            ImageRefusal::NotAnImage,
            ImageRefusal::UnsupportedType,
            ImageRefusal::Undecodable,
            ImageRefusal::TooManyPixels {
                width: 10000,
                height: 5000,
            },
            ImageRefusal::TooLargeRaw { bytes: 54_525_952 },
            ImageRefusal::TooLargeAfterResize {
                bytes: 5_000_000,
                limit: 4_000_000,
            },
            ImageRefusal::TooLargeToDecode,
            ImageRefusal::Empty,
        ] {
            assert!(!refusal.sentence().is_empty());
        }
        for kind in [ImageType::Png, ImageType::Jpeg, ImageType::Gif, ImageType::Webp] {
            assert!(kind.media_type().starts_with("image/"));
        }
    }
}
