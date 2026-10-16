//! A plugin's state and log made under a umask that takes the owner's own bits away (0277): directories are
//! still exactly 0700 and files exactly 0600, and everything reads back. The umask is the whole process's,
//! so this is the only test in its binary.

use std::os::unix::fs::MetadataExt;
use std::path::Path;

use eitri_core::plugin::state::PluginStore;
use serde_json::json;

struct Umask(libc::mode_t);

impl Umask {
    fn set(mask: libc::mode_t) -> Umask {
        // SAFETY: `umask` takes a mode and cannot fail.
        Umask(unsafe { libc::umask(mask) })
    }
}

impl Drop for Umask {
    fn drop(&mut self) {
        // SAFETY: as above.
        unsafe { libc::umask(self.0) };
    }
}

fn mode(path: &Path) -> u32 {
    std::fs::symlink_metadata(path).unwrap().mode() & 0o7777
}

#[test]
fn plugin_state_and_logs_are_exactly_private_under_umask_0277() {
    let root = std::env::temp_dir().join(format!("eitri-plugin-umask-{}", uuid::Uuid::new_v4().simple()));
    let eitri = root.join("eitri");
    {
        let _umask = Umask::set(0o277);
        let st = PluginStore::new(eitri.clone(), "demo");
        st.write("k", &json!({"a": 1})).unwrap();
        st.append("events", &[json!(1), json!(2)]).unwrap();
        assert_eq!(st.read("k").unwrap(), Some(json!({"a": 1})));
        assert_eq!(st.read_log("events", None).unwrap().0, [json!(1), json!(2)]);
    }
    for dir in ["", "plugins", "plugins/demo", "plugins/demo/state", "plugins/demo/log"] {
        assert_eq!(mode(&eitri.join(dir)), 0o700, "{dir:?}");
    }
    assert_eq!(mode(&eitri.join("plugins/demo/state/k.json")), 0o600);
    assert_eq!(mode(&eitri.join("plugins/demo/log/events.0000.jsonl")), 0o600);
    assert_eq!(mode(&eitri.join("plugins/demo/log/events.lock")), 0o600);
    let _ = std::fs::remove_dir_all(&root);
}
