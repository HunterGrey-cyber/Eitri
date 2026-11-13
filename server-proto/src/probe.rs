use std::collections::BTreeMap;
use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeReport {
    pub uname: String,
    pub home: PathBuf,
    pub data_dir: PathBuf,
    pub state_dir: PathBuf,
    pub login_shell: String,
    pub login_path: String,
    pub claude: Option<PathBuf>,
    pub git: Option<PathBuf>,
    pub glibc: Option<(u32, u32)>,
    pub linger: Option<bool>,
    pub kill_user_processes: Option<bool>,
    pub free_bytes: Option<u64>,
    pub gzip: bool,
    pub root: RootState,
    pub sync_marker: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootState {
    Missing,
    NotADirectory,
    NotOwned,
    Ok,
}

/// `run/<version>/info`, published by the daemon for `list-daemons` to read.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonInfo {
    pub version: String,
    pub rev: String,
    pub pid: u32,
    pub protocol_major: u32,
    pub protocol_minor: u32,
    pub started_unix_ms: u64,
}

impl ProbeReport {
    /// Required fields always have a line. An optional field is absent for None;
    /// an empty value is still present, so even an empty path remains distinguishable.
    pub fn parse(text: &str) -> Result<ProbeReport, String> {
        let mut values = BTreeMap::new();
        for line in text.split_terminator('\n') {
            let (key, value) = line.split_once('=').ok_or("probe line has no '='")?;
            if !matches!(
                key,
                "uname"
                    | "home"
                    | "data_dir"
                    | "state_dir"
                    | "login_shell"
                    | "login_path"
                    | "claude"
                    | "git"
                    | "glibc"
                    | "linger"
                    | "kill_user_processes"
                    | "free_bytes"
                    | "gzip"
                    | "root"
                    | "sync_marker"
            ) {
                return Err(format!("unknown probe key {key:?}"));
            }
            if values
                .insert(key, unescape(value).map_err(|e| format!("{key}: {e}"))?)
                .is_some()
            {
                return Err(format!("duplicate probe key {key:?}"));
            }
        }
        let required = |key| {
            values
                .get(key)
                .map(Vec::as_slice)
                .ok_or_else(|| format!("missing probe key {key:?}"))
        };
        let path = |key| required(key).map(|bytes| PathBuf::from(OsString::from_vec(bytes.to_vec())));
        let string = |key| required(key).and_then(|bytes| as_text(key, bytes).map(str::to_owned));
        let optional_path = |key| {
            values
                .get(key)
                .map(|bytes| PathBuf::from(OsString::from_vec(bytes.clone())))
        };
        let optional_bool = |key| values.get(key).map(|bytes| boolean(key, bytes)).transpose();
        let glibc = values
            .get("glibc")
            .map(|bytes| {
                let value = as_text("glibc", bytes)?;
                let (major, minor) = value.split_once('.').ok_or("glibc must be major.minor")?;
                Ok::<_, String>((number("glibc major", major)?, number("glibc minor", minor)?))
            })
            .transpose()?;
        let root = match required("root")? {
            b"missing" => RootState::Missing,
            b"not-a-directory" => RootState::NotADirectory,
            b"not-owned" => RootState::NotOwned,
            b"ok" => RootState::Ok,
            _ => return Err("invalid probe root state".into()),
        };
        Ok(Self {
            uname: string("uname")?,
            home: path("home")?,
            data_dir: path("data_dir")?,
            state_dir: path("state_dir")?,
            login_shell: string("login_shell")?,
            login_path: string("login_path")?,
            claude: optional_path("claude"),
            git: optional_path("git"),
            glibc,
            linger: optional_bool("linger")?,
            kill_user_processes: optional_bool("kill_user_processes")?,
            free_bytes: values
                .get("free_bytes")
                .map(|bytes| number("free_bytes", as_text("free_bytes", bytes)?))
                .transpose()?,
            gzip: boolean("gzip", required("gzip")?)?,
            root,
            sync_marker: optional_path("sync_marker"),
        })
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        let mut line = |key: &str, bytes: &[u8]| {
            out.push_str(key);
            out.push('=');
            escape(bytes, &mut out);
            out.push('\n');
        };
        line("uname", self.uname.as_bytes());
        line("home", self.home.as_os_str().as_bytes());
        line("data_dir", self.data_dir.as_os_str().as_bytes());
        line("state_dir", self.state_dir.as_os_str().as_bytes());
        line("login_shell", self.login_shell.as_bytes());
        line("login_path", self.login_path.as_bytes());
        for (key, path) in [
            ("claude", &self.claude),
            ("git", &self.git),
            ("sync_marker", &self.sync_marker),
        ] {
            if let Some(path) = path {
                line(key, path.as_os_str().as_bytes());
            }
        }
        if let Some((major, minor)) = self.glibc {
            line("glibc", format!("{major}.{minor}").as_bytes());
        }
        for (key, value) in [
            ("linger", self.linger),
            ("kill_user_processes", self.kill_user_processes),
            ("gzip", Some(self.gzip)),
        ] {
            if let Some(value) = value {
                line(key, if value { b"true" } else { b"false" });
            }
        }
        if let Some(bytes) = self.free_bytes {
            line("free_bytes", bytes.to_string().as_bytes());
        }
        line(
            "root",
            match self.root {
                RootState::Missing => b"missing",
                RootState::NotADirectory => b"not-a-directory",
                RootState::NotOwned => b"not-owned",
                RootState::Ok => b"ok",
            },
        );
        out
    }
}

fn as_text<'a>(key: &str, bytes: &'a [u8]) -> Result<&'a str, String> {
    std::str::from_utf8(bytes).map_err(|_| format!("{key} must be UTF-8"))
}

fn boolean(key: &str, bytes: &[u8]) -> Result<bool, String> {
    match bytes {
        b"true" => Ok(true),
        b"false" => Ok(false),
        _ => Err(format!("{key} must be true or false")),
    }
}

fn number<T: std::str::FromStr>(key: &str, text: &str) -> Result<T, String> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("{key} must be unsigned decimal"));
    }
    text.parse().map_err(|_| format!("{key} is out of range"))
}

fn escape(bytes: &[u8], out: &mut String) {
    use std::fmt::Write;
    for &byte in bytes {
        if (0x21..=0x7e).contains(&byte) && byte != b'%' {
            out.push(byte as char);
        } else {
            write!(out, "%{byte:02X}").expect("writing into a String");
        }
    }
}

fn unescape(value: &str) -> Result<Vec<u8>, String> {
    let nibble = |b: u8| match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        _ => Err("invalid percent escape".to_owned()),
    };
    let mut bytes = value.as_bytes().iter().copied();
    let mut out = Vec::new();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let hi = nibble(bytes.next().ok_or("truncated percent escape")?)?;
            let lo = nibble(bytes.next().ok_or("truncated percent escape")?)?;
            out.push(hi << 4 | lo);
        } else if (0x21..=0x7e).contains(&byte) {
            out.push(byte);
        } else {
            return Err("non-printable byte must be percent-encoded".into());
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt;

    fn report() -> ProbeReport {
        ProbeReport {
            uname: "Linux x86_64".into(),
            home: PathBuf::from(std::ffi::OsString::from_vec(b"/home/example/\xff\n%=\0".to_vec())),
            data_dir: "/home/example/.local/share".into(),
            state_dir: "/home/example/.local/state".into(),
            login_shell: "/bin/sh".into(),
            login_path: "/usr/bin:/path with spaces\n%".into(),
            claude: Some("/opt/bin/claude".into()),
            git: None,
            glibc: Some((2, 28)),
            linger: Some(false),
            kill_user_processes: Some(true),
            free_bytes: Some(u64::MAX),
            gzip: true,
            root: RootState::Ok,
            sync_marker: Some("/srv/project/.stfolder".into()),
        }
    }

    #[test]
    fn probe_report_round_trips_and_refuses_unknown_or_duplicate_keys() {
        let mut report = report();
        let rendered = report.render();
        assert_eq!(ProbeReport::parse(&rendered).unwrap(), report);
        assert!(rendered.contains("%FF%0A%25=%00"));
        assert!(ProbeReport::parse(&(rendered.clone() + "unknown=value\n")).is_err());
        assert!(ProbeReport::parse(&(rendered.clone() + "uname=Linux\n")).is_err());
        assert!(ProbeReport::parse(&rendered.replace("gzip=true\n", "")).is_err());
        for (key, bad) in [
            ("gzip", "yes"),
            ("root", "maybe"),
            ("glibc", "2.x"),
            ("free_bytes", "-1"),
        ] {
            let changed = rendered
                .lines()
                .map(|line| {
                    if line.starts_with(&format!("{key}=")) {
                        format!("{key}={bad}")
                    } else {
                        line.into()
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert!(ProbeReport::parse(&changed).is_err(), "{key}");
        }
        for bad in ["%", "%0", "%GG", "raw space", "\u{7f}"] {
            assert!(ProbeReport::parse(&rendered.replace("uname=Linux%20x86_64", &format!("uname={bad}"))).is_err());
        }
        report.claude = None;
        report.git = Some(PathBuf::new());
        report.glibc = None;
        report.linger = None;
        report.kill_user_processes = None;
        report.free_bytes = None;
        report.gzip = false;
        report.sync_marker = None;
        for root in [
            RootState::Missing,
            RootState::NotADirectory,
            RootState::NotOwned,
            RootState::Ok,
        ] {
            report.root = root;
            assert_eq!(ProbeReport::parse(&report.render()).unwrap(), report);
        }
    }
}
