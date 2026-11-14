//! Installing an executable from a test and then running it.
//!
//! Linux refuses to exec an inode that some process still holds open for writing (`ETXTBSY`). A test that copies a
//! binary with its own file descriptor can hit that while another test thread is between `fork` and `exec` and so
//! carries a copy of the descriptor. The copy is therefore made by a short-lived `cp` process, so the writable
//! descriptor never exists in the test process, and a run of the result still retries a busy exec for a bounded time.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

const ATTEMPTS: u32 = 50;
const BACKOFF: Duration = Duration::from_millis(15);

/// Puts a private (mode 0700), durable copy of `source` at `dest`, replacing what is there in one rename.
pub fn install_executable(source: &Path, dest: &Path) {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let name = dest.file_name().expect("destination has a file name").to_string_lossy();
    let staged = dest.with_file_name(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let status = Command::new("/bin/cp")
        .arg("--")
        .arg(source)
        .arg(&staged)
        .status()
        .unwrap();
    assert!(status.success(), "cp {} {}", source.display(), staged.display());
    std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o700)).unwrap();
    // A read-only descriptor does not make an inode busy to exec.
    std::fs::File::open(&staged).unwrap().sync_all().unwrap();
    std::fs::rename(&staged, dest).unwrap();
}

fn busy(error: &io::Error) -> bool {
    error.raw_os_error() == Some(libc::ETXTBSY)
}

/// `command.spawn()`, trying again while the executable is busy.
pub fn spawn(command: &mut Command) -> io::Result<Child> {
    for _ in 1..ATTEMPTS {
        match command.spawn() {
            Err(error) if busy(&error) => std::thread::sleep(BACKOFF),
            other => return other,
        }
    }
    command.spawn()
}

/// `command.status()`, trying again while the executable is busy.
pub fn status(command: &mut Command) -> io::Result<ExitStatus> {
    for _ in 1..ATTEMPTS {
        match command.status() {
            Err(error) if busy(&error) => std::thread::sleep(BACKOFF),
            other => return other,
        }
    }
    command.status()
}
