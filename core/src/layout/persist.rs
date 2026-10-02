//! The layout a project was left in, kept between launches (modules spec §4.6).
//!
//! **Where:** `$XDG_STATE_HOME/eitri/layout/<first 16 hex of sha256(canonical project root)>.json`
//! (`~/.local/state` when `XDG_STATE_HOME` is unset, as `agent::state_dirs` resolves its own two).
//! The hash is `agent::conversation_id_for_cwd`'s, cut to 16: the same digest of the same canonical
//! path. Nothing is written into the project directory -- the rule since `4a6a933`.
//!
//! **What:** `{version: 1, project_root, root, hidden, focus}`. Ratios are stored, and a pinned
//! split's pixels (modules P2's pinned bottom row -- the one place this departs from the spec's
//! "ratios are stored, not pixels", because keeping a bottom row's height IS a pixel length). Zoom
//! and the MRU order are not stored: a window opens unzoomed.
//!
//! **Read through a DTO, never by deriving `Deserialize` on the model** (the P1 plan's deferred item
//! 11): every id goes through `ModuleId::parse`, and the tree through `reconcile`, which ends in
//! `Layout::from_parts` -- so a file can neither name a module kind that does not exist nor skip an
//! invariant.
//!
//! **This is state, not config** (spec §4.6): a file that cannot be used -- corrupt, from another
//! version, written for another project root, or naming no editor or agent -- degrades to the
//! default layout with one log line, instead of refusing to start (a missing file is simply a
//! project whose layout has not been changed, and says nothing). That exception to the project's
//! hard-fail discipline covers this file only; `init.lua` still fails hard. **Such a file is never
//! overwritten**: `shell` reads the file again before every write, and one it cannot use then --
//! found so when the window opened, or made so since, by a hand-edit or by another build -- is
//! moved whole by [`set_aside`] to `<name>.json.unusable`, where the person who edited it (or the
//! newer build that wrote it) can still find it. What is not covered is the microseconds between
//! that read and the write's rename: another process writing in exactly that gap is replaced.
//!
//! **A key this build does not know is ignored**, serde's default: a file a later build wrote with
//! an extra field is still read. The cost is that a misspelt optional key is read as absent with no
//! note -- `"pinn"` for `"pin"` loses the pin. Every other key is required, so a misspelling of one
//! of those makes the file unusable, and it is set aside.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::module::{ModuleDecl, ModuleId};
use super::reconcile::{reconcile, Reconciled};
use super::tree::{Axis, Branch, Layout, Node, Pin};

/// The file format's version. A file of any other version is not read.
pub const VERSION: u32 = 1;

/// How long after the last change the file is written (spec §4.6). In `shell`
/// (`layout_state::LayoutSaver`) only a change of the arrangement arms the timer, not a move of the
/// keys; a change while it waits pushes back when it fires, and a timer that fires early is armed
/// again for what is left rather than restarted. The close writes synchronously, and only a change
/// still unwritten.
pub const SAVE_DEBOUNCE_MS: u64 = 500;

#[derive(Debug, Serialize, Deserialize)]
struct LayoutFile {
    version: u32,
    project_root: String,
    root: NodeFile,
    hidden: Vec<String>,
    focus: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum NodeFile {
    Leaf {
        module: String,
    },
    Split {
        axis: AxisFile,
        ratio: f32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pin: Option<PinFile>,
        first: Box<NodeFile>,
        second: Box<NodeFile>,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AxisFile {
    Row,
    Column,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SideFile {
    First,
    Second,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct PinFile {
    side: SideFile,
    px: Option<i32>,
}

impl NodeFile {
    fn of(node: &Node) -> NodeFile {
        match node {
            Node::Leaf(id) => NodeFile::Leaf {
                module: id.as_str().to_string(),
            },
            Node::Split {
                axis,
                ratio,
                pin,
                first,
                second,
            } => NodeFile::Split {
                axis: match axis {
                    Axis::Row => AxisFile::Row,
                    Axis::Column => AxisFile::Column,
                },
                ratio: *ratio,
                pin: pin.map(|p| PinFile {
                    side: match p.side {
                        Branch::First => SideFile::First,
                        Branch::Second => SideFile::Second,
                    },
                    px: p.px,
                }),
                first: Box::new(NodeFile::of(first)),
                second: Box::new(NodeFile::of(second)),
            },
        }
    }

    fn into_node(self) -> Result<Node, String> {
        Ok(match self {
            NodeFile::Leaf { module } => Node::Leaf(ModuleId::parse(&module).map_err(|e| e.to_string())?),
            NodeFile::Split {
                axis,
                ratio,
                pin,
                first,
                second,
            } => Node::Split {
                axis: match axis {
                    AxisFile::Row => Axis::Row,
                    AxisFile::Column => Axis::Column,
                },
                ratio,
                pin: pin.map(|p| Pin {
                    side: match p.side {
                        SideFile::First => Branch::First,
                        SideFile::Second => Branch::Second,
                    },
                    px: p.px,
                }),
                first: Box::new(first.into_node()?),
                second: Box::new(second.into_node()?),
            },
        })
    }
}

/// `<state home>/eitri/<sub>`: the rule `state_dir` states, for any of Eitri's state
/// directories (`layout`, and the session tabs' `agent`).
pub fn state_subdir(xdg_state_home: Option<&OsStr>, home: Option<&OsStr>, sub: &str) -> Option<PathBuf> {
    match (xdg_state_home, home) {
        (Some(state), _) if Path::new(state).is_absolute() => Some(PathBuf::from(state).join("eitri").join(sub)),
        (_, Some(home)) if Path::new(home).is_absolute() => {
            Some(PathBuf::from(home).join(".local/state/eitri").join(sub))
        }
        _ => None,
    }
}

/// Creates `dir` -- a [`state_subdir`], `<state home>/eitri/<sub>` -- and anything missing above
/// it 0700, and tightens `dir` and its `eitri` parent to 0700 where an older build left either
/// open (ruling R5; `agent::private_fs`'s module doc). Every writer of Eitri's own state in this
/// crate creates its directory through this, and writes its files 0600
/// (`agent::private_fs::write_private`).
pub fn create_state_dir(dir: &Path) -> std::io::Result<()> {
    agent::private_fs::create_private_dir_all(dir, dir.parent().unwrap_or(dir))
}

/// `<state home>/eitri/layout`, from the two variables that decide it: `XDG_STATE_HOME`, else
/// `$HOME/.local/state` (the XDG spec's own default). An `XDG_STATE_HOME` that is empty or not an
/// absolute path is ignored, as the XDG spec says to. A `HOME` that is empty or not absolute is
/// refused too, as `agent::state_dirs` refuses a relative home directory: relative, it would resolve
/// against the process's cwd, which for a window started with no project argument IS the project --
/// and nothing is written into the project. `None` when neither gives a directory.
pub fn state_dir(xdg_state_home: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    state_subdir(xdg_state_home, home, "layout")
}

/// The file's name for this canonical project root: 16 hex digits of its SHA-256, `.json`.
pub fn file_name(project_root: &Path) -> String {
    format!("{}.json", &agent::conversation_id_for_cwd(project_root)[..16])
}

/// The file's text for `layout`: pretty JSON, so a person can read and fix it.
pub fn encode(layout: &Layout, project_root: &Path) -> String {
    let file = LayoutFile {
        version: VERSION,
        project_root: project_root.to_string_lossy().into_owned(),
        root: NodeFile::of(layout.root()),
        hidden: layout.saved_hidden().iter().map(|id| id.as_str().to_string()).collect(),
        focus: layout.focus().as_str().to_string(),
    };
    serde_json::to_string_pretty(&file).expect("a layout always serializes")
}

/// `text` read back as a layout of this window: this project, this file version, every id a
/// module this window has (`reconcile`, whose notes come back with it). `Err` says why it cannot
/// be used.
pub fn decode(text: &str, project_root: &Path, lua: &[ModuleDecl]) -> Result<Reconciled, String> {
    let file: LayoutFile = serde_json::from_str(text).map_err(|e| format!("not a layout file ({e})"))?;
    if file.version != VERSION {
        return Err(format!("version {} (this build reads {VERSION})", file.version));
    }
    let root = project_root.to_string_lossy();
    if file.project_root != root {
        return Err(format!("written for {}, not {root}", file.project_root));
    }
    let tree = file.root.into_node()?;
    let hidden = file
        .hidden
        .iter()
        .map(|id| ModuleId::parse(id).map_err(|e| e.to_string()))
        .collect::<Result<BTreeSet<_>, _>>()?;
    let focus = ModuleId::parse(&file.focus).map_err(|e| e.to_string())?;
    reconcile(tree, &hidden, Some(&focus), lua).map_err(|e| e.to_string())
}

/// What [`load`] found.
#[derive(Debug)]
pub enum Loaded {
    /// The file, reconciled against this window.
    Restored(Reconciled),
    /// No file: this project's arrangement has not been changed in a window since the file was last
    /// deleted or set aside -- a project opened and closed untouched, or only clicked around in, gets
    /// none (`shell::layout_state`). The normal first launch, and not logged.
    Missing,
    /// A file that cannot be used, and why; the caller logs it and opens the default layout.
    Unusable(String),
}

/// Reads this project's file from `dir`.
pub fn load(dir: &Path, project_root: &Path, lua: &[ModuleDecl]) -> Loaded {
    let path = dir.join(file_name(project_root));
    match agent::private_fs::read_private_to_string(&path) {
        Ok(text) => match decode(&text, project_root, lua) {
            Ok(reconciled) => Loaded::Restored(reconciled),
            Err(why) => Loaded::Unusable(format!("{}: {why}", path.display())),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Loaded::Missing,
        Err(e) => Loaded::Unusable(format!("{}: {e}", path.display())),
    }
}

/// Writes this project's file into `dir`, creating it: to a `.tmp` beside it first and then renamed
/// over it, so a process that dies mid-write leaves the previous file, never half of one. (There is
/// no `fsync`, so a power loss can still leave a torn file on some filesystems; that file is then
/// unusable, and is set aside like any other.) The `.tmp` is this process's own ([`temporary`]):
/// two windows on one project are two processes (`NON_UNIQUE`), and one shared name would let one
/// rename the other's half-written file into place. The last to write still wins. A write or a
/// rename that fails removes its `.tmp`; only a process killed between the two leaves one, and
/// nothing sweeps those: telling a dead window's from a live one's would mean asking whether its
/// process still runs, for a file of a few hundred bytes that `load` never reads.
pub fn save(dir: &Path, project_root: &Path, layout: &Layout) -> std::io::Result<PathBuf> {
    create_state_dir(dir)?;
    let path = dir.join(file_name(project_root));
    let tmp = temporary(&path);
    let written = agent::private_fs::write_private(&tmp, encode(layout, project_root).as_bytes())
        .and_then(|()| std::fs::rename(&tmp, &path));
    if let Err(err) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }
    Ok(path)
}

/// Where [`save`] writes `path` first: `<name>.json.<this process's id>.tmp`, beside it.
pub fn temporary(path: &Path) -> PathBuf {
    path.with_extension(format!("json.{}.tmp", std::process::id()))
}

/// Moves this project's file, whole, to `<name>.json.unusable` beside it and returns where it went:
/// what the window does before it writes over a file [`load`] finds [`Loaded::Unusable`]. A
/// hand-edit with one typo, or a file a newer build wrote, is kept for a person to read; an earlier
/// `.unusable` is replaced.
pub fn set_aside(dir: &Path, project_root: &Path) -> std::io::Result<PathBuf> {
    let path = dir.join(file_name(project_root));
    let aside = path.with_extension("json.unusable");
    std::fs::rename(&path, &aside)?;
    Ok(aside)
}

#[cfg(test)]
mod tests {
    use super::super::geometry::{settle_pins, Frame, Size};
    use super::super::module::Placement;
    use super::*;

    fn root() -> PathBuf {
        PathBuf::from("/home/user/project")
    }

    /// A window as it may be left: a Lua side panel, the terminal shown and its pin settled, the
    /// agent hidden, the keys in the terminal.
    fn left_as() -> (Layout, Vec<ModuleDecl>) {
        let lua = vec![ModuleDecl {
            id: ModuleId::lua("notes"),
            placement: Placement::RightOfRoot,
        }];
        let mut decls = vec![ModuleDecl {
            id: ModuleId::terminal(),
            placement: Placement::BelowRoot,
        }];
        decls.extend(lua.iter().cloned());
        let mut layout = Layout::initial(&decls).unwrap();
        assert!(settle_pins(&mut layout, &Frame::new(Size { w: 1280, h: 721 }, 1)));
        layout.set_focus(&ModuleId::terminal()).unwrap();
        super::super::geometry::hide(
            &mut layout,
            &ModuleId::agent(),
            &Frame::new(Size { w: 1280, h: 721 }, 1),
        )
        .unwrap();
        (layout, lua)
    }

    fn a_dir(label: &str) -> PathBuf {
        agent::state_dirs::test_workspace_dir(label)
    }

    #[test]
    fn a_layout_comes_back_as_it_was_left_pins_and_all() {
        let (layout, lua) = left_as();
        let back = decode(&encode(&layout, &root()), &root(), &lua).unwrap();
        assert_eq!(back.layout.root(), layout.root());
        assert_eq!(back.layout.hidden(), layout.hidden());
        assert_eq!(back.layout.focus(), &ModuleId::terminal());
        assert!(back.notes.is_empty(), "{:?}", back.notes);
        let text = encode(&layout, &root());
        assert!(text.contains("\"px\": 240"), "the pinned height is in the file: {text}");
    }

    /// The Opus review's T6-5: an editor retired because nvim ended (`prefix x`, `:qa`, a crash) is
    /// saved as it was before -- shown -- so a relaunch opens with the editor, not without it until
    /// `prefix e`. One the user had hidden stays hidden: the retirement is not an arrangement choice,
    /// the hide was (the rule `kill_pane::Reveal::rehide` keeps for nvim's quit prompt).
    #[test]
    fn a_retired_editor_is_saved_as_it_was_before_nvim_ended() {
        let frame = Frame::new(Size { w: 1280, h: 721 }, 1);
        let mut layout = Layout::initial(&[]).unwrap();
        super::super::kill(&mut layout, &ModuleId::editor(), super::super::Reopen::Never, &frame).unwrap();
        assert!(layout.is_gone(&ModuleId::editor()), "the case");
        let back = decode(&encode(&layout, &root()), &root(), &[]).unwrap();
        assert!(
            !back.layout.hidden().contains(&ModuleId::editor()),
            "a relaunch shows the editor"
        );

        let mut layout = Layout::initial(&[]).unwrap();
        super::super::geometry::hide(&mut layout, &ModuleId::editor(), &frame).unwrap();
        super::super::kill(&mut layout, &ModuleId::editor(), super::super::Reopen::Never, &frame).unwrap();
        let back = decode(&encode(&layout, &root()), &root(), &[]).unwrap();
        assert!(
            back.layout.hidden().contains(&ModuleId::editor()),
            "one the user had hidden stays hidden"
        );
    }

    /// Spec §4.6: "zoom is not stored".
    #[test]
    fn a_zoom_is_not_stored() {
        let (mut layout, lua) = left_as();
        layout.toggle_zoom(&ModuleId::terminal());
        assert!(layout.zoomed().is_some());
        let back = decode(&encode(&layout, &root()), &root(), &lua).unwrap();
        assert_eq!(back.layout.zoomed(), None);
    }

    /// Every way a file can be unusable degrades to an `Err` that says why -- never a panic, never
    /// a layout that breaks an invariant.
    #[test]
    fn an_unusable_file_says_why_and_is_never_half_read() {
        let (layout, lua) = left_as();
        let good = encode(&layout, &root());
        let cases: Vec<(String, &str)> = vec![
            ("{ not json".to_string(), "not a layout file"),
            (good.replace("\"version\": 1", "\"version\": 2"), "version 2"),
            (
                good.replace("/home/user/project", "/home/user/other"),
                "written for /home/user/other",
            ),
            (
                good.replace("\"module\": \"agent\"", "\"module\": \"chat\""),
                "unknown module 'chat'",
            ),
            (
                good.replace("\"module\": \"editor\"", "\"module\": \"lua:gone\""),
                "no 'editor'",
            ),
            (
                good.replace("\"focus\": \"terminal\"", "\"focus\": \"\""),
                "unknown module ''",
            ),
        ];
        for (text, why) in cases {
            let err = decode(&text, &root(), &lua).unwrap_err();
            assert!(err.contains(why), "expected {why:?} in {err:?}");
        }
        // A ratio out of range goes through `Layout::from_parts` and is refused there.
        let mut value: serde_json::Value = serde_json::from_str(&good).unwrap();
        value["root"]["first"]["ratio"] = serde_json::json!(0.99);
        let err = decode(&value.to_string(), &root(), &lua).unwrap_err();
        assert!(err.contains("ratio 0.99"), "{err}");
    }

    /// A Lua panel removed from `init.lua` since the file was written is left out, and the notes say
    /// so; the rest of the window comes back.
    #[test]
    fn a_panel_no_longer_registered_is_left_out_and_said() {
        let (layout, _) = left_as();
        let back = decode(&encode(&layout, &root()), &root(), &[]).unwrap();
        assert!(!back.layout.contains(&ModuleId::lua("notes")));
        assert_eq!(back.notes, ["left out 'lua:notes': this window has no such module"]);
    }

    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// Into a state directory that does not exist yet -- the owner's first save: his
    /// `~/.local/state/eitri` has `conversations/` and `history/` and no `layout/` (the
    /// whole-branch review's M1, which deleted `create_dir_all` and failed nothing).
    #[test]
    fn save_then_load_round_trips_and_leaves_no_temporary_file() {
        let dir = a_dir("layout-persist").join("eitri/layout");
        assert!(!dir.exists());
        let (layout, lua) = left_as();
        let path = save(&dir, &root(), &layout).unwrap();
        assert_eq!(path, dir.join(file_name(&root())));
        match load(&dir, &root(), &lua) {
            Loaded::Restored(back) => assert_eq!(back.layout.root(), layout.root()),
            other => panic!("{other:?}"),
        }
        assert_eq!(names_in(&dir), [file_name(&root())]);
    }

    /// Local-IPC review finding 8 (ruling R5): every writer of Eitri's own state in this crate --
    /// the layout, the prompt history, the permission rules, the empty tab's mode -- creates its
    /// directory 0700 and its file 0600, and tightens the `eitri` root an older build left at
    /// `0755`. Under the usual umask 022 each of these was `0755`/`0644` before, readable by every
    /// local user wherever the path down to the state home is traversable.
    #[test]
    fn every_state_writer_here_keeps_its_directory_and_file_private() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o777;
        let state = a_dir("state-modes").join("eitri");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (layout, _) = left_as();

        let layout_file = save(&state.join("layout"), &root(), &layout).unwrap();
        crate::prompt_history::append(&state.join("history"), &root(), &["a prompt".to_string()]).unwrap();
        let history_file = crate::prompt_history::path(&state.join("history"), &root());
        crate::permission_store::add(
            &state.join("permissions"),
            &root(),
            &agent::PrefixRule::parse("Bash(git push *)").unwrap(),
        )
        .unwrap();
        let rules_file = crate::permission_store::path(&state.join("permissions"), &root());
        let mode_file = crate::agent_prefs::save_mode(
            &state.join("agent"),
            &root(),
            crate::agent_bridge::SessionModeChoice::Auto,
        )
        .unwrap();

        assert_eq!(mode(&state), 0o700, "the eitri root an older build left open");
        for sub in ["layout", "history", "permissions", "agent"] {
            assert_eq!(mode(&state.join(sub)), 0o700, "{sub}/");
        }
        for file in [&layout_file, &history_file, &rules_file, &mode_file] {
            assert_eq!(mode(file), 0o600, "{}", file.display());
        }
    }

    /// The `.tmp` is this process's own (the whole-branch review's M2, a shared `json.tmp`, failed
    /// nothing): another window's half-written file beside it is neither renamed into place nor
    /// overwritten.
    #[test]
    fn the_temporary_file_is_this_processs_own() {
        let dir = a_dir("layout-persist-tmp");
        let path = dir.join(file_name(&root()));
        assert_eq!(
            temporary(&path),
            dir.join(format!("{}.{}.tmp", file_name(&root()), std::process::id()))
        );
        // A shared name, and another process's own (pid 1 is never a window).
        let others = [
            dir.join(format!("{}.tmp", file_name(&root()))),
            dir.join(format!("{}.1.tmp", file_name(&root()))),
        ];
        for other in &others {
            std::fs::write(other, "{ half of another window's").unwrap();
        }
        let (layout, lua) = left_as();
        save(&dir, &root(), &layout).unwrap();
        for other in &others {
            assert_eq!(std::fs::read_to_string(other).unwrap(), "{ half of another window's");
        }
        assert!(matches!(load(&dir, &root(), &lua), Loaded::Restored(_)));
        assert!(!temporary(&path).exists());
    }

    /// A rename that fails -- here because a directory sits where the file goes -- removes its
    /// `.tmp` rather than leaving one per failed save (Task 3's review, minor 3).
    #[test]
    fn a_failed_save_leaves_no_temporary_file() {
        let dir = a_dir("layout-persist-failed");
        let path = dir.join(file_name(&root()));
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("in the way"), "").unwrap();
        let (layout, _) = left_as();
        assert!(save(&dir, &root(), &layout).is_err());
        assert_eq!(names_in(&dir), [file_name(&root())], "no .tmp is left behind");
    }

    /// The on-disk format, byte for byte: every file already written depends on these names
    /// (`"type"`, `"split"`, `"column"`, `"second"`), and a change to a `rename_all` would pass every
    /// round trip while stranding them with `VERSION` unchanged (Task 3's review, minor 5).
    #[test]
    fn the_on_disk_format_is_pinned() {
        let golden = r#"{
  "version": 1,
  "project_root": "/home/user/project",
  "root": {
    "type": "split",
    "axis": "column",
    "ratio": 0.5,
    "pin": {
      "side": "second",
      "px": 240
    },
    "first": {
      "type": "split",
      "axis": "row",
      "ratio": 0.25,
      "first": {
        "type": "leaf",
        "module": "editor"
      },
      "second": {
        "type": "leaf",
        "module": "agent"
      }
    },
    "second": {
      "type": "leaf",
      "module": "terminal"
    }
  },
  "hidden": [
    "terminal"
  ],
  "focus": "agent"
}"#;
        let tree = Node::Split {
            axis: Axis::Column,
            ratio: 0.5,
            pin: Some(Pin {
                side: Branch::Second,
                px: Some(240),
            }),
            first: Box::new(Node::split(
                Axis::Row,
                0.25,
                Node::Leaf(ModuleId::editor()),
                Node::Leaf(ModuleId::agent()),
            )),
            second: Box::new(Node::Leaf(ModuleId::terminal())),
        };
        let layout = Layout::from_parts(tree, [ModuleId::terminal()].into(), ModuleId::agent()).unwrap();
        assert_eq!(encode(&layout, &root()), golden);
        let back = decode(golden, &root(), &[]).unwrap();
        assert_eq!(back.layout, layout);
        assert!(back.notes.is_empty(), "{:?}", back.notes);
        let unsettled = golden.replace(r#""px": 240"#, r#""px": null"#);
        let back = decode(&unsettled, &root(), &[]).unwrap();
        assert!(
            matches!(
                back.layout.root(),
                Node::Split {
                    pin: Some(Pin { px: None, .. }),
                    ..
                }
            ),
            "a pin with no length yet is `null`"
        );
    }

    #[test]
    fn a_missing_file_is_missing_and_a_corrupt_one_is_unusable() {
        let dir = a_dir("layout-persist-missing");
        assert!(matches!(load(&dir, &root(), &[]), Loaded::Missing));
        std::fs::write(dir.join(file_name(&root())), "\u{0}garbage").unwrap();
        match load(&dir, &root(), &[]) {
            Loaded::Unusable(why) => assert!(why.contains("not a layout file"), "{why}"),
            other => panic!("{other:?}"),
        }
    }

    /// A file that could not be used is moved aside whole, never overwritten (the plan review's
    /// finding 1): afterwards the project has no file, and the `.unusable` beside it holds every byte.
    #[test]
    fn an_unusable_file_is_set_aside_whole() {
        let dir = a_dir("layout-persist-aside");
        let text = "{ \"version\": 1, \"a typo\" ";
        std::fs::write(dir.join(file_name(&root())), text).unwrap();
        assert!(matches!(load(&dir, &root(), &[]), Loaded::Unusable(_)));
        let aside = set_aside(&dir, &root()).unwrap();
        assert_eq!(aside, dir.join(format!("{}.unusable", file_name(&root()))));
        assert_eq!(std::fs::read_to_string(&aside).unwrap(), text);
        assert!(matches!(load(&dir, &root(), &[]), Loaded::Missing));
    }

    #[test]
    fn the_file_is_named_by_the_roots_hash_and_lives_under_the_state_home() {
        let name = file_name(&root());
        assert_eq!(name.len(), 16 + ".json".len());
        assert!(name[..16].chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(&name[..16], &agent::conversation_id_for_cwd(&root())[..16]);
        assert_ne!(name, file_name(Path::new("/home/user/other")));
        assert_eq!(
            state_dir(Some(OsStr::new("/s")), Some(OsStr::new("/h"))),
            Some(PathBuf::from("/s/eitri/layout"))
        );
        assert_eq!(
            state_dir(None, Some(OsStr::new("/h"))),
            Some(PathBuf::from("/h/.local/state/eitri/layout"))
        );
        assert_eq!(
            state_dir(Some(OsStr::new("")), Some(OsStr::new("/h"))),
            Some(PathBuf::from("/h/.local/state/eitri/layout")),
            "an empty XDG_STATE_HOME is unset"
        );
        assert_eq!(
            state_dir(Some(OsStr::new("state")), Some(OsStr::new("/h"))),
            Some(PathBuf::from("/h/.local/state/eitri/layout")),
            "a relative XDG_STATE_HOME is ignored, as the XDG spec says"
        );
        assert_eq!(state_dir(None, None), None);
        assert_eq!(state_dir(None, Some(OsStr::new(""))), None, "an empty HOME");
        assert_eq!(
            state_dir(Some(OsStr::new("state")), Some(OsStr::new("h"))),
            None,
            "a relative HOME would put the file under the cwd -- the project, launched with no argument"
        );
    }
}
