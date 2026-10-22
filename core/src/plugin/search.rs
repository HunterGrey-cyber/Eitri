//! Where `eitri.plugin.load(<name>)` finds a plugin: the user's data directory, the copy beside the running binary
//! (and, in a debug build, the source tree the binary was built from), then the system data directories. Apart from
//! `stat` calls on the candidate directories everything here works on its arguments; only
//! [`SearchEnv::from_process`] reads the real environment.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::manifest::id_ok;

/// The file whose presence makes a directory a plugin directory.
pub const MANIFEST: &str = "eitri-plugin.toml";

/// Below a data directory, where plugin directories live: `<data dir>/eitri/plugins/<name>`.
const DATA_SUBDIR: &str = "eitri/plugins";

/// Which place of the search found a plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchStep {
    UserData,
    Install,
    SourceTree,
    SystemData,
}

impl SearchStep {
    /// The words the window's log line uses for the place.
    pub fn label(self) -> &'static str {
        match self {
            SearchStep::UserData => "user data directory",
            SearchStep::Install => "beside the eitri binary",
            SearchStep::SourceTree => "source tree",
            SearchStep::SystemData => "system data directory",
        }
    }
}

/// What `eitri.plugin.load`'s first argument is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadArg<'a> {
    Name(&'a str),
    Path(&'a str),
}

/// A valid plugin id is a name; anything else is a path. A path holds a `/`, a `.` or a `~`, or an upper-case
/// letter, none of which an id may hold, so no path a user could mean is ever taken for a name.
pub fn classify(arg: &str) -> LoadArg<'_> {
    if id_ok(arg) {
        LoadArg::Name(arg)
    } else {
        LoadArg::Path(arg)
    }
}

/// The parts of the process's environment the search reads, passed in so tests never read the real one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchEnv {
    pub xdg_data_home: Option<OsString>,
    pub home: Option<PathBuf>,
    pub xdg_data_dirs: Option<OsString>,
    /// The directory of the canonical current executable.
    pub exe_dir: Option<PathBuf>,
    /// `<workspace>/plugins`, in a debug build only.
    pub source_plugins: Option<PathBuf>,
}

impl SearchEnv {
    /// The running process's values. The only reader of the real environment here.
    pub fn from_process() -> SearchEnv {
        SearchEnv {
            xdg_data_home: std::env::var_os("XDG_DATA_HOME"),
            home: std::env::var_os("HOME").map(PathBuf::from),
            xdg_data_dirs: std::env::var_os("XDG_DATA_DIRS"),
            exe_dir: std::env::current_exe()
                .ok()
                .and_then(|exe| std::fs::canonicalize(exe).ok())
                .and_then(|exe| exe.parent().map(Path::to_path_buf)),
            source_plugins: source_plugins(),
        }
    }
}

/// The source tree's `plugins/` directory, so a debug build run from the tree finds the plugins built there.
#[cfg(debug_assertions)]
fn source_plugins() -> Option<PathBuf> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(|workspace| workspace.join("plugins"))
}

/// A release build carries no path of the machine it was built on.
#[cfg(not(debug_assertions))]
fn source_plugins() -> Option<PathBuf> {
    None
}

/// Every directory the search looks in for `name`, in order, with the step each belongs to.
pub fn candidates(name: &str, env: &SearchEnv) -> Vec<(PathBuf, SearchStep)> {
    let mut out = Vec::new();
    // XDG's rule: XDG_DATA_HOME only when it is absolute (an empty value is not), else ~/.local/share.
    let data_home = match env.xdg_data_home.as_deref().map(Path::new) {
        Some(dir) if dir.is_absolute() => Some(dir.to_path_buf()),
        _ => env
            .home
            .as_deref()
            .filter(|home| home.is_absolute())
            .map(|home| home.join(".local/share")),
    };
    if let Some(dir) = data_home {
        out.push((dir.join(DATA_SUBDIR).join(name), SearchStep::UserData));
    }
    if let Some(dir) = &env.exe_dir {
        out.push((dir.join("plugins").join(name), SearchStep::Install));
    }
    if let Some(dir) = &env.source_plugins {
        out.push((dir.join(name), SearchStep::SourceTree));
    }
    // XDG's defaults only when the variable is unset or empty; a set value's relative or empty entries are dropped.
    let system: Vec<PathBuf> = match env.xdg_data_dirs.as_deref() {
        Some(value) if !value.is_empty() => std::env::split_paths(value).filter(|dir| dir.is_absolute()).collect(),
        _ => vec![PathBuf::from("/usr/local/share"), PathBuf::from("/usr/share")],
    };
    for dir in system {
        out.push((dir.join(DATA_SUBDIR).join(name), SearchStep::SystemData));
    }
    out
}

/// The directory that holds the plugin, and the step that found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub dir: PathBuf,
    pub step: SearchStep,
    /// Directories looked at before `dir` that exist but hold no regular-file manifest.
    pub skipped: Vec<PathBuf>,
}

/// No candidate holds a manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotFound {
    /// Every candidate, in the order looked at.
    pub tried: Vec<PathBuf>,
    /// The candidates among `tried` that exist but hold no regular-file manifest.
    pub skipped: Vec<PathBuf>,
}

/// The first candidate holding an `eitri-plugin.toml` regular file. Its manifest is not read here: one that fails
/// later is the plugin all the same, and the search never falls through to a later copy.
pub fn find_plugin(name: &str, env: &SearchEnv) -> Result<Found, NotFound> {
    let mut tried = Vec::new();
    let mut skipped = Vec::new();
    for (dir, step) in candidates(name, env) {
        if std::fs::metadata(dir.join(MANIFEST)).is_ok_and(|meta| meta.is_file()) {
            return Ok(Found { dir, step, skipped });
        }
        // An empty directory left behind must not hide a later copy, and the window says it looked past it.
        if std::fs::metadata(&dir).is_ok_and(|meta| meta.is_dir()) {
            skipped.push(dir.clone());
        }
        tried.push(dir);
    }
    Err(NotFound { tried, skipped })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::test_support::Scratch;

    const NAME: &str = "md";

    fn os(path: &Path) -> OsString {
        path.as_os_str().to_os_string()
    }

    /// Every step present, each under `root`: the user data directory, the binary's directory, the source tree and
    /// two system data directories.
    fn env_in(root: &Path) -> SearchEnv {
        SearchEnv {
            xdg_data_home: Some(os(&root.join("data-home"))),
            home: Some(root.join("home")),
            xdg_data_dirs: Some(std::env::join_paths([root.join("sys1"), root.join("sys2")]).unwrap()),
            exe_dir: Some(root.join("lib/eitri")),
            source_plugins: Some(root.join("src/plugins")),
        }
    }

    /// The five candidates of `env_in(root)`, in search order.
    fn five(root: &Path) -> Vec<(PathBuf, SearchStep)> {
        vec![
            (root.join("data-home/eitri/plugins/md"), SearchStep::UserData),
            (root.join("lib/eitri/plugins/md"), SearchStep::Install),
            (root.join("src/plugins/md"), SearchStep::SourceTree),
            (root.join("sys1/eitri/plugins/md"), SearchStep::SystemData),
            (root.join("sys2/eitri/plugins/md"), SearchStep::SystemData),
        ]
    }

    fn plugin_at(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(MANIFEST), "id = \"md\"\n").unwrap();
    }

    #[test]
    fn a_valid_plugin_id_is_a_name_and_anything_else_a_path() {
        let forty = "a".repeat(40);
        for name in ["markdown", "my-plugin2", "x", "0", forty.as_str()] {
            assert_eq!(classify(name), LoadArg::Name(name), "{name:?}");
        }
        let forty_one = "a".repeat(41);
        for path in [
            "./x",
            "~/x",
            "~",
            "/x",
            "a/b",
            "x.y",
            "X",
            "Markdown",
            "",
            "-x",
            "x-",
            "my_plugin",
            forty_one.as_str(),
        ] {
            assert_eq!(classify(path), LoadArg::Path(path), "{path:?}");
        }
    }

    #[test]
    fn the_labels_are_the_words_the_log_line_uses() {
        assert_eq!(SearchStep::UserData.label(), "user data directory");
        assert_eq!(SearchStep::Install.label(), "beside the eitri binary");
        assert_eq!(SearchStep::SourceTree.label(), "source tree");
        assert_eq!(SearchStep::SystemData.label(), "system data directory");
    }

    #[test]
    fn the_candidates_are_user_data_then_beside_the_binary_then_the_source_tree_then_each_system_directory() {
        let root = Path::new("/r");
        assert_eq!(candidates(NAME, &env_in(root)), five(root));
    }

    #[test]
    fn an_unset_empty_or_relative_xdg_data_home_falls_back_to_home_local_share() {
        let root = Path::new("/r");
        for value in [None, Some(OsString::new()), Some(OsString::from("rel/data"))] {
            let env = SearchEnv {
                xdg_data_home: value.clone(),
                ..env_in(root)
            };
            assert_eq!(
                candidates(NAME, &env)[0],
                (root.join("home/.local/share/eitri/plugins/md"), SearchStep::UserData),
                "{value:?}"
            );
        }
    }

    #[test]
    fn without_a_usable_home_there_is_no_user_step() {
        let root = Path::new("/r");
        for home in [None, Some(PathBuf::from("relative/home"))] {
            let env = SearchEnv {
                xdg_data_home: None,
                home: home.clone(),
                ..env_in(root)
            };
            let steps: Vec<SearchStep> = candidates(NAME, &env).into_iter().map(|(_, s)| s).collect();
            assert!(!steps.contains(&SearchStep::UserData), "{home:?}: {steps:?}");
            assert_eq!(steps.len(), 4, "{home:?}");
        }
    }

    #[test]
    fn an_unset_or_empty_xdg_data_dirs_gives_usr_local_share_then_usr_share() {
        let root = Path::new("/r");
        for value in [None, Some(OsString::new())] {
            let env = SearchEnv {
                xdg_data_dirs: value.clone(),
                ..env_in(root)
            };
            let system: Vec<(PathBuf, SearchStep)> = candidates(NAME, &env)
                .into_iter()
                .filter(|(_, s)| *s == SearchStep::SystemData)
                .collect();
            assert_eq!(
                system,
                vec![
                    (
                        PathBuf::from("/usr/local/share/eitri/plugins/md"),
                        SearchStep::SystemData
                    ),
                    (PathBuf::from("/usr/share/eitri/plugins/md"), SearchStep::SystemData),
                ],
                "{value:?}"
            );
        }
    }

    #[test]
    fn relative_and_empty_entries_of_xdg_data_dirs_are_skipped_and_no_default_is_added() {
        let root = Path::new("/r");
        let env = SearchEnv {
            xdg_data_dirs: Some(OsString::from("rel/share::/r/sys1::/r/sys2:")),
            ..env_in(root)
        };
        let system: Vec<PathBuf> = candidates(NAME, &env)
            .into_iter()
            .filter(|(_, s)| *s == SearchStep::SystemData)
            .map(|(d, _)| d)
            .collect();
        assert_eq!(
            system,
            vec![root.join("sys1/eitri/plugins/md"), root.join("sys2/eitri/plugins/md")]
        );
        let only_relative = SearchEnv {
            xdg_data_dirs: Some(OsString::from("rel/share:")),
            ..env_in(root)
        };
        assert!(candidates(NAME, &only_relative)
            .iter()
            .all(|(_, s)| *s != SearchStep::SystemData));
    }

    #[test]
    fn the_install_and_source_tree_steps_are_absent_when_not_passed() {
        let root = Path::new("/r");
        let env = SearchEnv {
            exe_dir: None,
            source_plugins: None,
            ..env_in(root)
        };
        let all = five(root);
        assert_eq!(
            candidates(NAME, &env),
            vec![all[0].clone(), all[3].clone(), all[4].clone()]
        );
    }

    #[test]
    fn each_step_alone_finds_its_copy() {
        for (index, (dir, step)) in five(Path::new("/unused")).into_iter().enumerate() {
            let s = Scratch::new("search-alone");
            let env = env_in(&s.0);
            let (want, want_step) = five(&s.0)[index].clone();
            assert_eq!(want_step, step);
            plugin_at(&want);
            assert_eq!(
                find_plugin(NAME, &env),
                Ok(Found {
                    dir: want,
                    step,
                    skipped: vec![]
                }),
                "{dir:?}"
            );
        }
    }

    #[test]
    fn the_first_copy_in_search_order_wins() {
        let s = Scratch::new("search-order");
        let env = env_in(&s.0);
        let all = five(&s.0);
        for (dir, _) in &all {
            plugin_at(dir);
        }
        // Taken away one at a time from the front: each next one wins, and the first system entry beats the second.
        for (index, (dir, step)) in all.iter().enumerate() {
            let found = find_plugin(NAME, &env).unwrap();
            assert_eq!((&found.dir, found.step), (dir, *step), "index {index}");
            std::fs::remove_dir_all(dir).unwrap();
        }
        assert!(find_plugin(NAME, &env).is_err());
    }

    #[test]
    fn a_directory_without_a_regular_manifest_is_skipped_and_reported() {
        let s = Scratch::new("search-skip");
        let env = env_in(&s.0);
        let all = five(&s.0);
        // The user's directory exists and is empty; the binary's holds a directory named like the manifest.
        std::fs::create_dir_all(&all[0].0).unwrap();
        std::fs::create_dir_all(all[1].0.join(MANIFEST)).unwrap();
        plugin_at(&all[3].0);
        assert_eq!(
            find_plugin(NAME, &env),
            Ok(Found {
                dir: all[3].0.clone(),
                step: SearchStep::SystemData,
                skipped: vec![all[0].0.clone(), all[1].0.clone()],
            })
        );
    }

    #[test]
    fn a_manifest_reached_through_a_symlink_to_a_regular_file_counts() {
        let s = Scratch::new("search-link");
        let env = env_in(&s.0);
        let all = five(&s.0);
        std::fs::create_dir_all(&all[0].0).unwrap();
        std::fs::write(s.0.join("real.toml"), "id = \"md\"\n").unwrap();
        std::os::unix::fs::symlink(s.0.join("real.toml"), all[0].0.join(MANIFEST)).unwrap();
        assert_eq!(find_plugin(NAME, &env).unwrap().step, SearchStep::UserData);
    }

    #[test]
    fn a_found_manifest_is_not_read_so_a_broken_one_does_not_fall_through() {
        let s = Scratch::new("search-broken");
        let env = env_in(&s.0);
        let all = five(&s.0);
        std::fs::create_dir_all(&all[0].0).unwrap();
        std::fs::write(all[0].0.join(MANIFEST), "this is not toml [[[").unwrap();
        plugin_at(&all[3].0);
        let found = find_plugin(NAME, &env).unwrap();
        assert_eq!((found.dir, found.step), (all[0].0.clone(), SearchStep::UserData));
    }

    #[test]
    fn an_unknown_name_returns_every_directory_tried_in_order() {
        let s = Scratch::new("search-unknown");
        let env = env_in(&s.0);
        let all: Vec<PathBuf> = five(&s.0).into_iter().map(|(d, _)| d).collect();
        assert_eq!(
            find_plugin(NAME, &env),
            Err(NotFound {
                tried: all.clone(),
                skipped: vec![]
            })
        );
        std::fs::create_dir_all(&all[2]).unwrap();
        assert_eq!(
            find_plugin(NAME, &env),
            Err(NotFound {
                tried: all.clone(),
                skipped: vec![all[2].clone()]
            })
        );
    }

    #[test]
    fn a_candidate_that_is_a_file_is_tried_but_not_reported_as_skipped() {
        let s = Scratch::new("search-file");
        let env = env_in(&s.0);
        let all: Vec<PathBuf> = five(&s.0).into_iter().map(|(d, _)| d).collect();
        std::fs::create_dir_all(all[0].parent().unwrap()).unwrap();
        std::fs::write(&all[0], "not a directory").unwrap();
        assert_eq!(
            find_plugin(NAME, &env),
            Err(NotFound {
                tried: all,
                skipped: vec![]
            })
        );
    }
}
