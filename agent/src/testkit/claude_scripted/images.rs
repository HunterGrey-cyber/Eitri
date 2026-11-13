//! The images a scenario's driver part sends: files under `tests/fixtures/claude_scripted_images/`, named by a
//! part's `images` list. Each is read whole and described the way the host describes an image it sends: its media
//! type from its first bytes, its size from its header, the length and the SHA-256 of its bytes.

use crate::provider::{AttachmentMeta, TurnImage};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::Arc;

/// The directory the fixture images live in.
pub fn directory() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude_scripted_images")
}

/// The path of the fixture `name`: a plain file name of that directory, never a path of the caller's making.
pub fn path_of(name: &str) -> Result<PathBuf, String> {
    let plain = !name.is_empty()
        && name != "generate.py"
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-');
    if !plain || name.starts_with('.') {
        return Err(format!("`{name}` is not the name of an image fixture"));
    }
    let path = directory().join(name);
    if path.is_file() {
        Ok(path)
    } else {
        Err(format!("no image fixture `{name}` under {}", directory().display()))
    }
}

/// The media type a file's first bytes name, in the words of the Messages API.
pub fn media_type_of(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// The description of an image's bytes, as a turn carries it.
pub fn describe(bytes: &[u8]) -> Result<AttachmentMeta, String> {
    let media_type = media_type_of(bytes).ok_or("the bytes are none of PNG, JPEG, GIF or WebP")?;
    let (width, height) = crate::history::transcript_jsonl::image_size(bytes).ok_or("the header holds no size")?;
    Ok(AttachmentMeta {
        media_type: media_type.to_owned(),
        bytes: bytes.len() as u64,
        width,
        height,
        sha256: Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect(),
    })
}

/// The fixture `name` as an image to send.
pub fn load(name: &str) -> Result<TurnImage, String> {
    let path = path_of(name)?;
    let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let meta = describe(&bytes).map_err(|e| format!("{name}: {e}"))?;
    Ok(TurnImage {
        meta,
        data: Arc::from(bytes),
    })
}

/// A driver part's `images` list: the names it holds, in order.
pub fn names_in(arg: &serde_json::Value) -> Result<Vec<String>, String> {
    match arg.get("images") {
        None | Some(serde_json::Value::Null) => Ok(Vec::new()),
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| "an image name is a string".to_owned())
            })
            .collect(),
        Some(_) => Err("`images` is a list of fixture names".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_fixture_is_described_by_its_own_header() {
        for (name, kind, size) in [
            ("png_48x32.png", "image/png", (48, 32)),
            ("png_alpha_48x32.png", "image/png", (48, 32)),
            ("jpeg_48x32.jpg", "image/jpeg", (48, 32)),
            ("gif_40x40_two_frames.gif", "image/gif", (40, 40)),
            ("webp_48x32.webp", "image/webp", (48, 32)),
        ] {
            let image = load(name).unwrap_or_else(|e| panic!("{e}"));
            assert_eq!(image.meta.media_type, kind, "{name}");
            assert_eq!((image.meta.width, image.meta.height), size, "{name}");
            assert_eq!(image.meta.bytes, image.data.len() as u64);
            assert_eq!(image.meta.sha256.len(), 64);
        }
    }

    #[test]
    fn a_name_is_a_file_of_the_directory_and_nothing_else() {
        for bad in [
            "",
            "../png_48x32.png",
            "/etc/passwd",
            "a/b.png",
            ".hidden",
            "generate.py",
            "missing.png",
        ] {
            assert!(path_of(bad).is_err(), "{bad}");
        }
        assert!(path_of("png_48x32.png").is_ok());
    }

    #[test]
    fn a_part_names_its_images_in_a_list() {
        use serde_json::json;
        assert_eq!(names_in(&json!("text")).unwrap(), Vec::<String>::new());
        assert_eq!(
            names_in(&json!({"text": "x", "images": ["a.png", "b.png"]})).unwrap(),
            ["a.png", "b.png"]
        );
        assert!(names_in(&json!({"images": "a.png"})).is_err());
        assert!(names_in(&json!({"images": [1]})).is_err());
    }
}
