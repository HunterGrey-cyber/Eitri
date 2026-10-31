//! The compositor group's open windows and bottom terminal. Only its editor window writes this file.

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use super::names::WindowName;
use crate::layout::{Layout, ModuleId, Node};
use serde::{Deserialize, Serialize};

const DEBOUNCE: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CmFile {
    pub version: u32,
    pub terminal: CmTerminal,
    pub windows: Vec<CmWindow>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CmTerminal {
    pub shown: bool,
    pub px: Option<u32>,
}

impl CmTerminal {
    pub fn of(layout: &Layout) -> Self {
        let terminal = ModuleId::terminal();
        let px = layout.root().children.iter().find_map(|child| {
            matches!(&child.node, Node::Leaf(id) if *id == terminal)
                .then_some(child.pin?.px?)
                .and_then(|px| u32::try_from(px).ok())
        });
        Self {
            shown: layout.is_shown(&terminal),
            px,
        }
    }

    /// The shape stays the editor above the terminal; only the terminal's height and visibility are remembered.
    pub fn layout(&self) -> Layout {
        let mut root = crate::layout::hub_tree().layout.root().clone();
        if let Some(px) = self.px.and_then(|px| i32::try_from(px).ok()).filter(|px| *px > 0) {
            if let Some(child) = root
                .children
                .iter_mut()
                .find(|child| matches!(&child.node, Node::Leaf(id) if *id == ModuleId::terminal()))
            {
                child.pin = Some(crate::layout::Pin { px: Some(px) });
            }
        }
        let mut layout =
            Layout::new(Node::Con(root), ModuleId::editor()).expect("the hub has its editor above its terminal");
        if !self.shown {
            crate::layout::hide(
                &mut layout,
                &ModuleId::terminal(),
                &crate::layout::Frame::new(crate::wm::persist::FIRST_LAUNCH_FRAME, 1),
            )
            .expect("the editor keeps the keys while the terminal hides");
        }
        layout
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CmWindow {
    pub name: String,
    pub output: Option<String>,
    pub order: u32,
}

impl CmFile {
    pub fn new(layout: &Layout, names: &[WindowName]) -> Self {
        Self {
            version: 1,
            terminal: CmTerminal::of(layout),
            windows: names
                .iter()
                .enumerate()
                .map(|(order, name)| CmWindow {
                    name: name.text(),
                    output: None,
                    order: order as u32,
                })
                .collect(),
        }
    }

    pub fn ordered_windows(&self) -> Vec<WindowName> {
        let mut windows: Vec<_> = self.windows.iter().collect();
        windows.sort_by_key(|w| w.order);
        windows
            .into_iter()
            .filter_map(|w| WindowName::parse(&w.name).ok())
            .filter(|w| *w != WindowName::Second)
            .collect()
    }

    pub fn output(&self, name: &WindowName) -> Option<&str> {
        self.windows.iter().find(|w| w.name == name.text())?.output.as_deref()
    }
}

pub fn cm_file_name(root: &Path) -> String {
    format!("{}.cm.json", crate::state_paths::project_key(root))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadCm {
    Missing,
    Read(CmFile, Vec<String>),
    Unusable(String),
}

pub fn read_cm(dir: &Path, root: &Path) -> ReadCm {
    let path = dir.join(cm_file_name(root));
    let text = match agent::private_fs::read_private_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return ReadCm::Missing,
        Err(err) => return ReadCm::Unusable(format!("{}: {err}", path.display())),
    };
    match decode(&text) {
        Ok((file, notes)) => ReadCm::Read(file, notes),
        Err(why) => {
            let aside = path.with_extension("json.unusable");
            let result = std::fs::rename(&path, &aside);
            let note = match result {
                Ok(()) => format!("saved as {}", aside.display()),
                Err(err) => format!("could not set it aside: {err}"),
            };
            ReadCm::Unusable(format!("{}: {why}; {note}", path.display()))
        }
    }
}

fn decode(text: &str) -> Result<(CmFile, Vec<String>), String> {
    let value: serde_json::Value = serde_json::from_str(text).map_err(|err| err.to_string())?;
    if value.get("version").and_then(|v| v.as_u64()) != Some(1) {
        return Err("expected compositor layout version 1".into());
    }
    let mut terminal: CmTerminal = serde_json::from_value(value.get("terminal").cloned().ok_or("missing terminal")?)
        .map_err(|err| err.to_string())?;
    let list = value
        .get("windows")
        .and_then(|v| v.as_array())
        .ok_or("expected windows array")?;
    let mut notes = Vec::new();
    if terminal.px.is_some_and(|px| px == 0 || px > 16384) {
        notes.push("terminal height is out of range; using its default".into());
        terminal.px = None;
    }
    let mut names = BTreeSet::new();
    let mut windows = Vec::new();
    for entry in list {
        let mut window: CmWindow = match serde_json::from_value(entry.clone()) {
            Ok(window) => window,
            Err(err) => {
                notes.push(format!("left out a window: {err}"));
                continue;
            }
        };
        if !matches!(WindowName::parse(&window.name), Ok(name) if name != WindowName::Second)
            || !names.insert(window.name.clone())
        {
            notes.push(format!("left out an invalid or repeated window {:?}", window.name));
            continue;
        }
        if window
            .output
            .as_deref()
            .is_some_and(|output| !crate::compositor::valid_output_name(output))
        {
            notes.push(format!("left out the invalid output of {}", window.name));
            window.output = None;
        }
        windows.push(window);
    }
    Ok((
        CmFile {
            version: 1,
            terminal,
            windows,
        },
        notes,
    ))
}

/// A private temporary sibling is renamed whole into place; a malformed older file is preserved first.
pub fn write_cm(dir: &Path, root: &Path, file: &CmFile) -> io::Result<()> {
    crate::state_paths::create_state_dir(dir)?;
    if let ReadCm::Unusable(why) = read_cm(dir, root) {
        if dir.join(cm_file_name(root)).symlink_metadata().is_ok() {
            return Err(io::Error::other(why));
        }
    }
    let path = dir.join(cm_file_name(root));
    let temporary = crate::state_paths::temporary(&path);
    let bytes = serde_json::to_vec_pretty(file).map_err(io::Error::other)?;
    let result = agent::private_fs::write_private(&temporary, &bytes).and_then(|()| std::fs::rename(&temporary, &path));
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

enum Write {
    Changed(CmFile),
    Finish(CmFile, Sender<io::Result<()>>),
}

/// Updates only queue a snapshot. A single worker coalesces changes and owns every write.
pub struct CmWriter {
    send: Sender<Write>,
}

impl CmWriter {
    pub fn new(dir: PathBuf, root: PathBuf) -> io::Result<Self> {
        let (send, receive) = mpsc::channel();
        std::thread::Builder::new().name("cm-layout".into()).spawn(move || {
            let mut pending = None;
            loop {
                let next = if pending.is_some() {
                    receive.recv_timeout(DEBOUNCE)
                } else {
                    receive.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected)
                };
                match next {
                    Ok(Write::Changed(file)) => pending = Some(file),
                    Ok(Write::Finish(file, reply)) => {
                        let _ = reply.send(write_cm(&dir, &root, &file));
                        break;
                    }
                    Err(err) => {
                        if let Some(file) = pending.take() {
                            if let Err(err) = write_cm(&dir, &root, &file) {
                                eprintln!("[layout] compositor layout: {err}");
                            }
                        }
                        if err == mpsc::RecvTimeoutError::Disconnected {
                            break;
                        }
                    }
                }
            }
        })?;
        Ok(Self { send })
    }

    pub fn changed(&self, file: CmFile) {
        let _ = self.send.send(Write::Changed(file));
    }

    /// The host polls this answer while holding the application alive, bounded by [`crate::wm::persist::CLOSE_FLUSH_BOUND`].
    pub fn finish(&self, file: CmFile) -> Receiver<io::Result<()>> {
        let (reply, result) = mpsc::channel();
        let _ = self.send.send(Write::Finish(file, reply));
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_are_restored_in_order_and_invalid_entries_do_not_hide_valid_ones() {
        let (file, notes) = decode(
            r#"{
            "version":1,"terminal":{"shown":false,"px":180,"extra":true},
            "windows":[
                {"name":"p:markdown","output":"DP-2","order":8,"extra":true},
                {"name":"c3","output":"DP-1","order":1},
                {"name":"2","output":null,"order":2},
                {"name":"c3","output":null,"order":4},
                {"name":"c2","output":"a;b","order":7},
                {"name":"c9","order":"bad"}
            ],"future":true
        }"#,
        )
        .unwrap();
        assert_eq!(
            file.ordered_windows(),
            vec![
                WindowName::Chat(3),
                WindowName::Chat(2),
                WindowName::Page("markdown".into())
            ]
        );
        assert_eq!(file.output(&WindowName::Chat(2)), None);
        assert_eq!(notes.len(), 4);
        let layout = file.terminal.layout();
        assert_eq!(CmTerminal::of(&layout), file.terminal);
        assert_eq!(layout.modules(), vec![ModuleId::editor(), ModuleId::terminal()]);
        assert_eq!(layout.focus(), &ModuleId::editor());
    }

    #[test]
    fn a_final_snapshot_follows_queued_changes_on_the_writer_thread() {
        let base = std::env::temp_dir().join(format!("eitri-cm-writer-{}", uuid::Uuid::new_v4()));
        let dir = base.join("layout");
        let root = Path::new("/project");
        let mut file = CmFile::new(&crate::layout::hub_tree().layout, &[WindowName::Chat(1)]);
        let writer = CmWriter::new(dir.clone(), root.to_owned()).unwrap();
        writer.changed(file.clone());
        file.windows[0].output = Some("DP-2".into());
        file.terminal = CmTerminal {
            shown: false,
            px: Some(240),
        };
        writer
            .finish(file.clone())
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap();
        assert_eq!(read_cm(&dir, root), ReadCm::Read(file, vec![]));
        assert!(std::fs::read_dir(&dir).unwrap().all(|entry| !entry
            .unwrap()
            .path()
            .to_string_lossy()
            .ends_with(".tmp")));
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn the_cm_file_round_trips_and_an_unusable_one_is_set_aside() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("eitri-cm-{}", uuid::Uuid::new_v4()));
        let dir = base.join("layout");
        let root = Path::new("/project");
        assert_eq!(read_cm(&dir, root), ReadCm::Missing);
        let file = CmFile {
            version: 1,
            terminal: CmTerminal {
                shown: true,
                px: Some(240),
            },
            windows: vec![CmWindow {
                name: "c2".into(),
                output: Some("DP-2".into()),
                order: 0,
            }],
        };
        write_cm(&dir, root, &file).unwrap();
        let path = dir.join(cm_file_name(root));
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(read_cm(&dir, root), ReadCm::Read(file.clone(), vec![]));
        let mut json = serde_json::to_value(&file).unwrap();
        json["future"] = true.into();
        agent::private_fs::write_private(&path, json.to_string().as_bytes()).unwrap();
        assert_eq!(read_cm(&dir, root), ReadCm::Read(file, vec![]));
        agent::private_fs::write_private(&path, b"{").unwrap();
        assert!(matches!(read_cm(&dir, root), ReadCm::Unusable(_)));
        assert!(!path.exists());
        assert_eq!(std::fs::read(path.with_extension("json.unusable")).unwrap(), b"{");
        std::fs::remove_dir_all(base).unwrap();
    }
}
