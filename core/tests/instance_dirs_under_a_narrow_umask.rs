//! The per-instance directories nvim's feeds live in, made under a umask that takes the owner's own bits
//! away (0277). The directory must still be exactly 0700, so its socket and snippet can be created, and the
//! socket exactly 0600, so nvim can connect to it; otherwise each feature turns itself off.
//!
//! The umask is the whole process's, so this is the one test in its own binary: no other test runs while
//! it is narrowed.

use eitri_core::editor_context::feed::EditorContextFeed;
use eitri_core::nvim_keys::feed::NvimKeysFeed;
use eitri_core::pane_switch::PaneSwitchChannel;
use eitri_core::plugin::follow_feed::FollowSocket;
use eitri_core::theme::feed::ThemeFeed;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

/// Restores the umask it replaced when dropped, a failed assertion included.
struct Umask(libc::mode_t);

impl Umask {
    fn set(mask: libc::mode_t) -> Self {
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
    std::fs::symlink_metadata(path).expect("stat").mode() & 0o7777
}

/// The directory holds the socket, at exactly 0700; the socket is exactly 0600 and takes a connection.
fn usable(name: &str, socket: &Path) -> PathBuf {
    let dir = socket.parent().expect("the socket is in its directory").to_path_buf();
    assert_eq!(mode(&dir), 0o700, "{name}: its directory");
    assert_eq!(mode(socket), 0o600, "{name}: its socket");
    UnixStream::connect(socket).unwrap_or_else(|e| panic!("{name}: nvim could not connect: {e}"));
    dir
}

#[test]
fn every_instance_directory_is_exactly_private_under_umask_0277() {
    agent::state_dirs::redirect_state_to_a_test_root();
    let _umask = Umask::set(0o277);
    let tmp = std::env::temp_dir();

    let created = eitri_core::instance_dir::create_instance_dir(&tmp, "eitri-umask-test-", "s.sock", "test")
        .expect("created under umask 0277");
    assert_eq!(mode(&created), 0o700, "create_instance_dir");
    std::fs::write(created.join("probe"), b"x").expect("something can be created in it");
    std::fs::remove_dir_all(&created).unwrap();

    let editor = EditorContextFeed::new().expect("the editor-context feed starts under umask 0277");
    let keys = NvimKeysFeed::new().expect("the nvim-keys feed starts under umask 0277");
    let theme = ThemeFeed::new().expect("the theme feed starts under umask 0277");
    let follow = FollowSocket::bind().expect("the plugin follow socket binds under umask 0277");
    // With a shim: its `bin/` directory is made the same way, and the link to the shim goes into it.
    let shim = Path::new("/bin/sh");
    let switch = PaneSwitchChannel::bind(shim).expect("the pane switch binds under umask 0277");

    let scratch =
        eitri_core::scratch::ScratchDir::in_dir(&tmp).expect("the scratch directory is made under umask 0277");
    assert_eq!(mode(scratch.path()), 0o700, "scratch");
    std::fs::write(scratch.path().join("probe"), b"x").expect("a draft can be written in it");
    std::fs::remove_dir_all(scratch.path()).unwrap();

    // The tutorial's lessons' directory, which nvim writes the lessons into when `:EitriTutor` runs (its snippet
    // sets each directory it makes to 0700 itself).
    let lessons = eitri_core::tutor::TutorDir::in_dir(&tmp).expect("the lessons' directory is made under umask 0277");
    assert_eq!(mode(lessons.path()), 0o700, "tutor");
    std::fs::write(lessons.path().join("probe"), b"x").expect("a file can be written in it");
    lessons.cleanup();

    usable("editor context", editor.socket_path());
    usable("nvim keys", keys.socket_path());
    usable("theme", theme.socket_path());
    usable("plugin follow", follow.socket_path());
    let switch_dir = usable("pane switch", switch.socket_path());
    assert_eq!(mode(&switch_dir.join("bin")), 0o700, "pane switch: bin/");
    assert_eq!(std::fs::read_link(switch_dir.join("bin").join("tmux")).unwrap(), shim);
}
