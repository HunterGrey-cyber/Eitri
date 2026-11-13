use std::ffi::{OsStr, OsString};
use std::fmt;
use std::os::unix::ffi::{OsStrExt, OsStringExt};

pub const MAX_ARGS_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArgsError {
    NotHex,
    Truncated,
    TooLong,
}

impl fmt::Display for ArgsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotHex => "arguments are not hexadecimal bytes",
            Self::Truncated => "argument length extends past the input",
            Self::TooLong => "arguments exceed the 64 KiB limit",
        })
    }
}

impl std::error::Error for ArgsError {}

/// Each argument's byte length as a big-endian u32, then its bytes, all in lowercase hex.
pub fn encode_args(args: &[&OsStr]) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    for arg in args {
        let bytes = arg.as_bytes();
        let len = u32::try_from(bytes.len()).expect("one argument must fit a u32 length");
        for byte in len.to_be_bytes().iter().chain(bytes) {
            write!(out, "{byte:02x}").expect("writing into a String");
        }
    }
    out
}

pub fn decode_args(hex: &str) -> Result<Vec<OsString>, ArgsError> {
    if hex.len() % 2 != 0 {
        return Err(ArgsError::NotHex);
    }
    if hex.len() / 2 > MAX_ARGS_BYTES {
        return Err(ArgsError::TooLong);
    }
    let nibble = |b: u8| match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        _ => Err(ArgsError::NotHex),
    };
    let bytes = hex
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| Ok((nibble(pair[0])? << 4) | nibble(pair[1])?))
        .collect::<Result<Vec<_>, ArgsError>>()?;
    let mut rest = bytes.as_slice();
    let mut args = Vec::new();
    while !rest.is_empty() {
        let prefix = rest.get(..4).ok_or(ArgsError::Truncated)?;
        let len = u32::from_be_bytes(prefix.try_into().expect("four bytes")) as usize;
        rest = &rest[4..];
        let arg = rest.get(..len).ok_or(ArgsError::Truncated)?;
        args.push(OsString::from_vec(arg.to_vec()));
        rest = &rest[len..];
    }
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt;

    #[test]
    fn args_hex_round_trips_any_bytes() {
        let mut args: Vec<OsString> = ["a b", "$(rm -rf ~)", "'\"", "*", "\n", "-o", ""]
            .into_iter()
            .map(OsString::from)
            .collect();
        args.push(OsString::from_vec((0..4096).map(|i| (i % 256) as u8).collect()));
        let refs: Vec<_> = args.iter().map(|arg| arg.as_os_str()).collect();
        let encoded = encode_args(&refs);
        assert!(encoded
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
        assert_eq!(decode_args(&encoded).unwrap(), args);
        assert_eq!(encode_args(&[]), "");
        assert!(decode_args("").unwrap().is_empty());
        assert_eq!(encode_args(&[std::ffi::OsStr::new("a")]), "0000000161");
    }

    #[test]
    fn args_hex_refuses_truncated_odd_and_oversized_input() {
        for input in ["0000000261", "00", "000000", "ffffffff"] {
            assert_eq!(decode_args(input), Err(ArgsError::Truncated), "{input}");
        }
        for input in ["0g", "0A", "00000001AF", "0", "000000000"] {
            assert_eq!(decode_args(input), Err(ArgsError::NotHex), "{input}");
        }
        assert_eq!(decode_args(&"00".repeat(MAX_ARGS_BYTES + 1)), Err(ArgsError::TooLong));
        let at_limit = OsString::from_vec(vec![0xff; MAX_ARGS_BYTES - 4]);
        assert_eq!(decode_args(&encode_args(&[&at_limit])).unwrap(), vec![at_limit]);
    }
}
