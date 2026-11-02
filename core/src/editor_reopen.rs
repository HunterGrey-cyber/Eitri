//! Shared snippets and channel file names for reopening the editor.

/// The Lua entry point loaded by the reopen `--cmd`: restores what the last session left open.
pub static EDITOR_REOPEN_LUA: &str = include_str!("editor_reopen.lua");
/// The Lua capture module: describes what is open in the editor as a record.
pub static EDITOR_REOPEN_CAPTURE_LUA: &str = include_str!("editor_reopen_capture.lua");

/// The file in the channel directory the host writes the record to reopen into.
pub const REOPEN_IN: &str = "reopen-in.json";
/// The channel directory's file name for [`EDITOR_REOPEN_LUA`].
pub const REOPEN_LUA_NAME: &str = "editor_reopen.lua";
/// The channel directory's file name for [`EDITOR_REOPEN_CAPTURE_LUA`].
pub const REOPEN_CAPTURE_NAME: &str = "editor_reopen_capture.lua";

/// The file name the editor writes its captured record to for one handoff, keyed by its token.
pub fn handoff_name(token: &str) -> String {
    format!("reopen-out-{token}.json")
}

/// A fresh handoff token: 16 lowercase hex characters, the form the reopen lines carry.
pub fn new_token() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..16].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_is_sixteen_lowercase_hex_characters_and_differs_each_time() {
        let token = new_token();
        assert_eq!(token.len(), 16);
        assert!(token.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
        assert_ne!(token, new_token());
    }

    #[test]
    fn the_handoff_file_is_named_after_its_token() {
        assert_eq!(handoff_name("0123456789abcdef"), "reopen-out-0123456789abcdef.json");
    }
}
