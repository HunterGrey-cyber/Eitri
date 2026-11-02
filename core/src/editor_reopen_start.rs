//! What a window does about reopening the editor's files before its controller takes over: once `init.lua` has said
//! whether to reopen, and before the embedded nvim reaches VimEnter, the shell reads the record and the restore marker,
//! writes nvim's input into the pane-switch channel directory, and starts the record's writer seeded with what it read.
//! The window controller does no I/O, so this runs in the shell, on the start's own path, as the remembered theme's
//! start read does; everything after it goes through the writer's thread.
//!
//! A window that does not reopen -- turned off, `--clean`, no pane-switch channel, window 2 of a pair, or one started
//! with files to open -- reads, writes and starts nothing.
//!
//! The shell also carries out the controller's two ops about an ended nvim's last record here
//! ([`read_handoff`], [`close_record`]): telling the writer is all the first does, and the second, at the window's
//! close, waits for the writer, bounded, before the channel directory goes.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::editor_record::{self, RecordFile, RecordWriter, StartRead};
use crate::window_link::pair::Role;
use crate::wm::feedback;

/// How long the writer looks for the record an ended nvim leaves at its exit, from when it is told to.
pub const HANDOFF_WAIT: Duration = Duration::from_secs(1);
/// How long a window's close waits for the record writer to finish what it was handed, that read included.
pub const RECORD_FLUSH: Duration = Duration::from_secs(2);

/// What the start knows when it decides about reopening.
#[derive(Debug, Clone, PartialEq)]
pub struct ReopenStart {
    /// `editor.restore` is on and the window was not started with `--clean`.
    pub enabled: bool,
    pub state_home: Option<OsString>,
    pub home: Option<OsString>,
    pub role: Role,
    /// The pane-switch channel's private directory, where nvim's input goes; `None` without the channel.
    pub channel_dir: Option<PathBuf>,
    pub root: PathBuf,
    /// The window's private directories nvim can see: no file under one is kept in the record.
    pub prefixes: Vec<PathBuf>,
    /// The command line named files for the editor to open.
    pub started_with_files: bool,
}

/// What the start found and started, handed to the window controller with its reopen step.
#[derive(Debug, Clone, PartialEq)]
pub struct Prepared {
    /// The token of the nvim about to start, whose input is on disk; `None` when this window does not reopen.
    pub token: Option<String>,
    /// The last restore never said it ended: this nvim restores nothing.
    pub skip: bool,
    /// What the start has to say once the window can show it.
    pub toast: Option<String>,
    pub writer: Option<RecordWriter>,
    pub channel_dir: Option<PathBuf>,
    pub root: PathBuf,
    pub prefixes: Vec<PathBuf>,
    pub started_with_files: bool,
}

impl Prepared {
    /// A start that reopens nothing.
    pub fn nothing() -> Prepared {
        Prepared {
            token: None,
            skip: false,
            toast: None,
            writer: None,
            channel_dir: None,
            root: PathBuf::new(),
            prefixes: Vec::new(),
            started_with_files: false,
        }
    }
}

/// Reads the record and the marker, writes nvim's input with a fresh token, and starts the writer seeded with the
/// record read. A record that could not be read is set aside and said once; a marker left by a restore that never
/// finished makes this nvim skip its restore, said once too.
pub fn prepare_start(start: &ReopenStart) -> Prepared {
    let off = Prepared {
        channel_dir: start.channel_dir.clone(),
        root: start.root.clone(),
        prefixes: start.prefixes.clone(),
        started_with_files: start.started_with_files,
        ..Prepared::nothing()
    };
    if !start.enabled || start.started_with_files || start.role == Role::Second {
        return off;
    }
    let Some(channel_dir) = &start.channel_dir else {
        return off;
    };
    let Some(file) = RecordFile::for_window(
        start.state_home.as_deref(),
        start.home.as_deref(),
        &start.root,
        start.role,
    ) else {
        eprintln!("[editor] reopen: no state directory (neither XDG_STATE_HOME nor HOME is absolute); nothing is kept");
        return off;
    };
    let (record, mut toast) = match file.read_start(&start.root) {
        StartRead::Record(record) => (Some(record), None),
        StartRead::Nothing => (None, None),
        StartRead::SetAside(why) => {
            eprintln!("[editor] reopen: {why}");
            (None, Some(feedback::reopen_set_aside_text()))
        }
    };
    let skip = file.marker().is_some();
    if skip {
        eprintln!(
            "[editor] reopen: the last restore did not finish ({} is there); this start restores nothing",
            file.marker_path().display()
        );
        // A record set aside is said too: the person reads both, on one toast.
        let unfinished = feedback::reopen_unfinished_text();
        toast = Some(toast.map_or(unfinished.clone(), |aside| format!("{aside}; {unfinished}")));
    }
    let token = crate::editor_reopen::new_token();
    let input = if skip { None } else { record.as_ref() };
    if let Err(e) = editor_record::write_input(channel_dir, &token, &start.root, &start.prefixes, input, skip) {
        eprintln!("[editor] reopen: could not write the editor's input: {e}; nothing is reopened or kept this time");
        return off;
    }
    let writer = RecordWriter::start(file, start.root.clone(), record);
    writer.begin_session(token.clone(), skip);
    Prepared {
        token: Some(token),
        skip,
        toast,
        writer: Some(writer),
        ..off
    }
}

/// Tells `writer` to read the record the nvim of `token` leaves in `channel_dir` at its exit, looking for it for at
/// most [`HANDOFF_WAIT`] from now, and to hand it over with `seq`.
pub fn read_handoff(writer: &RecordWriter, channel_dir: &Path, token: &str, seq: u64) {
    // Only a token this window made names a file; anything else would put a stranger's name in the path.
    if token.len() != 16 || !token.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        eprintln!("[editor] reopen: no handoff is read for a name that is not a token");
        return;
    }
    let path = channel_dir.join(crate::editor_reopen::handoff_name(token));
    writer.read_handoff(seq, token.to_string(), path, HANDOFF_WAIT);
}

/// Removes nvim's input from `channel_dir`; one already gone is no error.
pub fn drop_input(channel_dir: &Path) {
    match std::fs::remove_file(channel_dir.join(editor_record::INPUT_FILE_NAME)) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => eprintln!("[editor] reopen: could not remove the editor's input: {e}"),
    }
}

/// The window's close: the ended nvim's last record is read when `token` is given, then the writer is waited for, at
/// most [`RECORD_FLUSH`], so what it holds is on disk before the channel directory is removed. Whether it finished.
pub fn close_record(writer: &RecordWriter, channel_dir: &Path, token: Option<&str>, seq: u64) -> bool {
    if let Some(token) = token {
        read_handoff(writer, channel_dir, token, seq);
    }
    writer.flush(RECORD_FLUSH)
}

#[cfg(test)]
pub(crate) mod tests;
