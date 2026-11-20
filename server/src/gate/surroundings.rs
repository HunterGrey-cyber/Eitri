//! What the disk half of the classifier judges a session's requests against on this server: its root, the home
//! directory, the helper's identity, whether the agent's environment names a git location, and the places that are
//! Eitri's own here, the helper's state among them.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use agent::permission_policy::{FactSurroundings, GIT_LOCATION_VARIABLES};
use agent::project_boundary::Identity;
use agent::{EitriPlaces, PLACE_VARIABLES};
use serde::{Deserialize, Deserializer, Serialize};

use crate::run_dir::Paths;

/// The most bytes [`StartedEnv`] may take in a sidecar's record, as JSON. The record is read back under a cap of its
/// own, and a record too long to read is a sidecar the helper never adopts again; so a start environment longer than
/// this is not kept, and the sidecar's requests get a refusal fact instead.
pub const STARTED_RECORD_MAX: usize = 16 * 1024;

/// What of the environment a sidecar was started with the classifier needs, kept in its record so that a restarted
/// daemon judges an adopted sidecar's requests against the environment that sidecar really has (the agent's shell
/// inherits it), not a later capture.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartedEnv {
    /// Any of `GIT_LOCATION_VARIABLES` is set, so git's repository is not the project root's to decide.
    pub git_location_from_environment: bool,
    /// Each of `agent::PLACE_VARIABLES` that is set, with its value's bytes: in the record, as text when it is UTF-8
    /// and as hex otherwise.
    #[serde(with = "compact")]
    pub places: BTreeMap<String, Vec<u8>>,
}

impl StartedEnv {
    /// The environment a command whose environment was cleared and then set will give its child, or `None` when it
    /// is longer than [`STARTED_RECORD_MAX`].
    pub fn of(command: &std::process::Command) -> Option<StartedEnv> {
        let set: BTreeMap<&OsStr, &OsStr> = command
            .get_envs()
            .filter_map(|(name, value)| value.map(|value| (name, value)))
            .collect();
        let started = StartedEnv {
            git_location_from_environment: GIT_LOCATION_VARIABLES
                .iter()
                .any(|name| set.contains_key(OsStr::new(name))),
            places: PLACE_VARIABLES
                .iter()
                .filter_map(|name| {
                    set.get(OsStr::new(name))
                        .map(|value| ((*name).to_owned(), value.as_bytes().to_vec()))
                })
                .collect(),
        };
        serde_json::to_vec(&started)
            .is_ok_and(|json| json.len() <= STARTED_RECORD_MAX)
            .then_some(started)
    }

    fn var(&self, name: &str) -> Option<OsString> {
        self.places.get(name).map(|bytes| OsString::from_vec(bytes.clone()))
    }
}

/// Reads a record's `started`, or `None` when it does not read as one: what the record keeps for the path facts must
/// never stop the helper adopting the sidecar.
pub fn started_or_none<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<StartedEnv>, D::Error> {
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value.and_then(|value| serde_json::from_value(value).ok()))
}

/// The record's form of the place variables: each value as a JSON string when it is UTF-8, else `{"hex": ...}`.
mod compact {
    use std::collections::BTreeMap;

    use serde::de::Error as _;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    #[derive(Serialize, Deserialize)]
    #[serde(untagged)]
    enum Stored {
        Text(String),
        Bytes { hex: String },
    }

    pub fn serialize<S: Serializer>(places: &BTreeMap<String, Vec<u8>>, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_map(places.iter().map(|(name, value)| {
            let stored = match std::str::from_utf8(value) {
                Ok(text) => Stored::Text(text.to_owned()),
                Err(_) => Stored::Bytes {
                    hex: value.iter().map(|byte| format!("{byte:02x}")).collect(),
                },
            };
            (name, stored)
        }))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<BTreeMap<String, Vec<u8>>, D::Error> {
        BTreeMap::<String, Stored>::deserialize(deserializer)?
            .into_iter()
            .map(|(name, stored)| match stored {
                Stored::Text(text) => Ok((name, text.into_bytes())),
                Stored::Bytes { hex } => unhex(&hex)
                    .map(|bytes| (name, bytes))
                    .ok_or_else(|| D::Error::custom("a place value is not hex")),
            })
            .collect()
    }

    fn unhex(hex: &str) -> Option<Vec<u8>> {
        if hex.len() % 2 != 0 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        (0..hex.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).ok())
            .collect()
    }
}

/// One session's surroundings on this server.
#[derive(Debug)]
pub struct Scene {
    /// The sidecar's recorded root: the directory its agent runs in, as `/proc/self/fd` named it at start.
    pub root: PathBuf,
    pub around: FactSurroundings,
    pub places: EitriPlaces,
}

/// The helper's own directories, which are Eitri's places here: its data (installations, sidecars), its state
/// (journals, pending requests, records) and the run directory with its sockets when that lies outside the state.
pub fn helper_places(paths: &Paths) -> Vec<PathBuf> {
    let mut dirs = vec![paths.data.clone(), paths.state.clone()];
    if !paths.run.starts_with(&paths.state) {
        dirs.push(paths.run.clone());
    }
    dirs
}

/// The scene for a sidecar started at `root` with `started`. Reads the account files once per process
/// (`Identity::current`) and resolves the installation directory, so it runs off the runtime threads.
pub fn scene(root: &Path, started: &StartedEnv, paths: &Paths) -> Scene {
    // The helper runs from its installation, whose sidecars sit beside its binary: that directory is the running
    // binary's place here, named as it resolves.
    let install = paths
        .install
        .as_ref()
        .map(|install| install.canonicalize().unwrap_or_else(|_| install.clone()));
    let extra = helper_places(paths);
    let places = EitriPlaces::from_variables(|name| started.var(name), install.as_deref(), &extra);
    Scene {
        root: root.to_path_buf(),
        around: FactSurroundings {
            home: started.var("HOME").map(PathBuf::from),
            identity: Identity::current(),
            git_location_from_environment: started.git_location_from_environment,
        },
        places,
    }
}
