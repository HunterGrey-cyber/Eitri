//! The Lua quoting the generated file is built from: the one function that writes a string literal.

use std::fmt::Write;

use super::display::is_hidden;

/// `s` as a Lua string literal.
pub fn quote(s: &str) -> String {
    quote_bytes(s.as_bytes())
}

/// `bytes` as a Lua string literal, in the manner of Lua's `%q`: a backslash and a double quote are escaped, a
/// newline is `\n`, and anything else that would not read as itself in the file (a control character, a character
/// that changes how the line looks without showing, a byte that is not UTF-8) is written byte by byte as a decimal
/// escape. The escape always has three digits, so a digit after it cannot join it.
pub fn quote_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() + 2);
    out.push('"');
    for chunk in bytes.utf8_chunks() {
        for c in chunk.valid().chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                c if c.is_control() || is_hidden(c) => {
                    c.encode_utf8(&mut [0; 4])
                        .bytes()
                        .for_each(|byte| escape(&mut out, byte));
                }
                c => out.push(c),
            }
        }
        chunk.invalid().iter().for_each(|byte| escape(&mut out, *byte));
    }
    out.push('"');
    out
}

fn escape(out: &mut String, byte: u8) {
    write!(out, "\\{byte:03}").expect("writing to a String does not fail");
}

#[cfg(test)]
mod tests {
    use super::*;
    use mlua::Lua;

    fn read_back(lua: &Lua, literal: &str) -> Vec<u8> {
        let value = lua.load(format!("return {literal}")).eval::<mlua::Value>();
        match value.unwrap_or_else(|e| panic!("{literal}: {e}")) {
            mlua::Value::String(text) => text.as_bytes().to_vec(),
            other => panic!("{literal}: {other:?}"),
        }
    }

    #[test]
    fn quotes_and_backslashes_are_escaped() {
        assert_eq!(quote("a\"b\\c"), r#""a\"b\\c""#);
    }

    #[test]
    fn a_newline_is_written_as_its_letter() {
        assert_eq!(quote("a\nb"), r#""a\nb""#);
    }

    #[test]
    fn a_control_byte_is_three_digits_so_no_digit_can_join_it() {
        assert_eq!(quote("\x07"), r#""\007""#);
        assert_eq!(quote("\x071"), r#""\0071""#);
        assert_eq!(quote("\r"), r#""\013""#);
        assert_eq!(quote("\t"), r#""\009""#);
        assert_eq!(quote("\0"), r#""\000""#);
        assert_eq!(quote("\x7f"), r#""\127""#);
    }

    #[test]
    fn a_byte_that_is_not_utf8_is_a_decimal_escape() {
        assert_eq!(quote_bytes(&[0xff]), r#""\255""#);
        assert_eq!(quote_bytes(&[b'a', 0xc3, b'b']), r#""a\195b""#);
    }

    #[test]
    fn valid_utf8_stays_as_written_but_for_its_control_characters() {
        assert_eq!(quote("Ctrl+é 中"), "\"Ctrl+é 中\"");
        assert_eq!(quote("\u{85}"), r#""\194\133""#);
    }

    #[test]
    fn a_character_that_would_not_show_is_a_decimal_escape() {
        assert_eq!(quote("\u{202e}"), r#""\226\128\174""#);
    }

    #[test]
    fn the_quoted_text_reads_back_in_lua() {
        let lua = Lua::new();
        for byte in 0..=255u8 {
            assert_eq!(read_back(&lua, &quote_bytes(&[byte])), [byte], "byte {byte}");
            // A digit right after: a shorter escape would swallow it.
            assert_eq!(
                read_back(&lua, &quote_bytes(&[byte, b'1'])),
                [byte, b'1'],
                "byte {byte}"
            );
        }
        let every_byte: Vec<u8> = (0..=255u8).collect();
        assert_eq!(read_back(&lua, &quote_bytes(&every_byte)), every_byte);
        let text = "M-\"\\ é 中 \u{202e}\n";
        assert_eq!(read_back(&lua, &quote(text)), text.as_bytes());
    }
}
