//! Test helpers shared by the plugin modules' tests and `core/tests/plugin_*`.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// A fresh scratch directory under the system temp dir, removed on drop.
pub struct Scratch(pub PathBuf);

impl Scratch {
    pub fn new(label: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("eitri-plugin-{label}-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        // Canonical, so tests compare with the paths the code under test canonicalises.
        Scratch(std::fs::canonicalize(&dir).unwrap())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A stand-in for the plugin's CLI: `watch` prints one record and keeps running until signalled;
/// anything else prints one record and exits 0.
pub const FAKE_LEARN: &str = "#!/bin/sh\n\
case \"$1\" in\n\
  watch) printf '{\"type\":\"check_started\",\"course\":\"%s\",\"exercise\":\"%s\",\"input\":\"0\"}\\n' \"$2\" \"$3\"; exec sleep 300 ;;\n\
  *) printf '{\"type\":\"prepared\",\"course\":\"%s\",\"exercise\":\"%s\",\"dir\":\"%s\",\"entry\":\"x\",\"created\":true}\\n' \"$2\" \"$3\" \"$5\" ;;\n\
esac\n";

/// A demo plugin on disk: the example manifest, an executable `bin/learn` that is a link into the plugin's
/// own `target/`, and a site holding one page. Creates `<root>/plugin` and `<root>/work`; returns the plugin
/// directory and the settings that point the plugin at that work directory.
pub fn demo_plugin(root: &Path) -> (PathBuf, BTreeMap<String, String>) {
    let dir = root.join("plugin");
    std::fs::create_dir_all(dir.join("target")).unwrap();
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    std::fs::create_dir_all(dir.join("build/site")).unwrap();
    std::fs::create_dir_all(root.join("work")).unwrap();
    std::fs::write(
        dir.join("eitri-plugin.toml"),
        include_str!("testdata/demo-manifest.toml"),
    )
    .unwrap();
    let learn = dir.join("target/learn");
    script_fixture::write_executable_script(&learn, FAKE_LEARN, 0o755).unwrap();
    std::os::unix::fs::symlink("../target/learn", dir.join("bin/learn")).unwrap();
    std::fs::write(
        dir.join("build/site/index.html"),
        "<!doctype html><title>demo</title><p>demo</p>\n",
    )
    .unwrap();
    let settings = BTreeMap::from([("work".to_string(), root.join("work").display().to_string())]);
    (dir, settings)
}

mod script_fixture {
    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/executable.rs"));
}
