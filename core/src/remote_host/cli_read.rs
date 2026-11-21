//! `eitri host read <host>:<absolute path>`: the bytes of one file on a recorded host, printed to stdout. What
//! `eitri.nvim` reads an `eitri-remote://` buffer through (a file the mirror does not hold: a dependency, the standard
//! library). Nothing is printed unless the whole file arrived and its SHA-256 matches the one the helper sent; a
//! refusal or a failure is one line on stderr (hidden characters escaped) and exit code 1. 64 is a command line it does
//! not take.
//!
//! The helper's answer is untrusted: its size is held to [`AnswerBound::read_path`] and each chunk to
//! `LSP_CHUNK_MAX`, and the bytes are kept until the hash has been checked, so a refusal after data (a file that grew
//! past the cap while it was read) leaves no part of the file on stdout.

use std::io::Write;
use std::path::Path;

use agent::remote_client::terminal_transport::wire::{ReadPathChunk, ReadPathRequest, CAP_READ_PATH, LSP_CHUNK_MAX};
use agent::remote_client::HelperClient;
use sha2::{Digest, Sha256};

use super::cli::Context;
use crate::remote_project::RemoteProject;
use crate::remote_sync::terminal_text;

pub const USAGE: &str = "usage: eitri host read <host>:<absolute path>";

/// The most of the helper's refusal that is shown.
const REFUSAL_SHOWN: usize = 500;

/// `eitri host read <args>` with this process's stdout and stderr.
pub fn run_read(args: &[String], cx: &mut Context<'_>) -> i32 {
    run_read_to(args, cx, &mut std::io::stdout().lock(), &mut std::io::stderr())
}

/// `eitri host read <args>`, the file to `stdout` and the one line of a failure to `stderr`.
pub fn run_read_to(args: &[String], cx: &mut Context<'_>, stdout: &mut dyn Write, stderr: &mut dyn Write) -> i32 {
    let [spec] = args else {
        let _ = writeln!(stderr, "{USAGE}");
        return 64;
    };
    let fail = |stderr: &mut dyn Write, why: &str| {
        let _ = writeln!(stderr, "eitri host read: {}", terminal_text(why));
        1
    };
    let (host, path) = match super::cli::split_spec(spec).and_then(|(host, path)| {
        super::first_connect::validate_root(Path::new(&path)).map_err(|_| {
            format!(
                "{path:?}: the path must be absolute, with no `.` or `..` component and no doubled or trailing slash"
            )
        })?;
        Ok((host, path))
    }) {
        Ok(found) => found,
        Err(why) => return fail(stderr, &why),
    };
    let cwd = std::env::current_dir().unwrap_or_default();
    let project = match project_on(cx, &host, &cwd) {
        Ok(project) => project,
        Err(why) => return fail(stderr, &why),
    };
    let client = match cx.reach.exec_connection(&project) {
        Ok(connection) => connection.lsp_client().map_err(|error| error.to_string()),
        Err(why) => Err(why),
    };
    let bytes = match client.and_then(|client| fetch(&client, &path)) {
        Ok(bytes) => bytes,
        Err(why) => return fail(stderr, &why),
    };
    match stdout.write_all(&bytes).and_then(|()| stdout.flush()) {
        Ok(()) => 0,
        Err(error) => fail(stderr, &format!("stdout cannot be written: {error}")),
    }
}

/// The recorded project on `host` (as typed) to reach it through: the one holding `cwd`, else the first.
fn project_on(cx: &mut Context<'_>, host: &str, cwd: &Path) -> Result<RemoteProject, String> {
    let (projects, _) = cx.store.list().map_err(|error| error.to_string())?;
    let mut on_host: Vec<RemoteProject> = projects.into_iter().filter(|project| project.host == host).collect();
    let holding = on_host.iter().position(|project| cwd.starts_with(&project.root));
    match (holding, on_host.is_empty()) {
        (Some(at), _) => Ok(on_host.swap_remove(at)),
        (None, false) => Ok(on_host.swap_remove(0)),
        (None, true) => Err(format!("no remote project on {host} is recorded on this machine")),
    }
}

/// The file at `path` on the helper's host, read through `client`: nothing but the whole file with its hash checked.
pub fn fetch(client: &HelperClient, path: &str) -> Result<Vec<u8>, String> {
    let capabilities = client
        .hello_capabilities()
        .map_err(|status| status.message().to_owned())?;
    if !capabilities.iter().any(|capability| capability == CAP_READ_PATH) {
        return Err(
            "the Eitri server on this host is older than reading files outside the project; \
                    reopen the project to update it"
                .to_owned(),
        );
    }
    let chunks = client.collect_read_path(ReadPathRequest {
        path: path.to_owned(),
        limit: 0,
    })?;
    judge(&chunks)
}

/// The file an answer holds: data chunks, then one chunk with the hash of all of them. A refusal anywhere, a chunk
/// that holds more than one kind of thing, one past `LSP_CHUNK_MAX`, no closing chunk, a chunk after it or a hash that
/// is not the data's is no file.
pub fn judge(chunks: &[ReadPathChunk]) -> Result<Vec<u8>, String> {
    let mut data = Vec::new();
    let mut closed = None;
    for chunk in chunks {
        if let Some(refusal) = chunk_refusal(chunk) {
            return Err(refusal);
        }
        if closed.is_some() {
            return Err("the server sent more after the end of the file".to_owned());
        }
        match (chunk.data.is_empty(), chunk.sha256.is_empty()) {
            (false, true) if chunk.data.len() <= LSP_CHUNK_MAX => data.extend_from_slice(&chunk.data),
            (false, true) => return Err("the server sent a piece of the file that is too large".to_owned()),
            (true, false) => closed = Some(chunk.sha256.as_str()),
            // An empty file is data of none and its hash; an empty chunk says nothing.
            (true, true) => {}
            (false, false) => return Err("the server sent data and a hash in one piece".to_owned()),
        }
    }
    let Some(claimed) = closed else {
        return Err("the answer ended before the end of the file".to_owned());
    };
    let digest = Sha256::digest(&data);
    let actual: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    if claimed != actual {
        return Err("the file's hash does not match what the server sent".to_owned());
    }
    Ok(data)
}

fn chunk_refusal(chunk: &ReadPathChunk) -> Option<String> {
    (!chunk.refusal.is_empty()).then(|| chunk.refusal.chars().take(REFUSAL_SHOWN).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote_host::cli::{Agent, Contact, Reach, Terminal};
    use crate::remote_host::install::InstallPlan;
    use crate::remote_project::RemoteProjects;
    use agent::ssh::{ChannelLauncher, ConnectError};
    use std::sync::Arc;

    /// A reach that connects to nothing: what the arguments and the records decide is all these cases look at.
    struct NoReach;

    impl Reach for NoReach {
        fn connect(&mut self, _: &str) -> Result<String, ConnectError> {
            unreachable!()
        }
        fn first_contact(&mut self, _: &str) -> Result<Contact, String> {
            unreachable!()
        }
        fn launcher(&self, _: &Path) -> Arc<dyn ChannelLauncher> {
            unreachable!()
        }
        fn plan(&mut self) -> Result<InstallPlan, String> {
            unreachable!()
        }
        fn agents(&mut self, _: &str, _: &str, _: &Path) -> Result<Vec<Agent>, String> {
            unreachable!()
        }
    }

    struct Quiet;

    impl Terminal for Quiet {
        fn say(&mut self, _: &str) {}
        fn ask(&mut self, _: &str) -> bool {
            false
        }
    }

    /// `eitri host read <args>` against `store`: its exit code, stdout and stderr.
    fn run(store: &RemoteProjects, args: &[&str]) -> (i32, Vec<u8>, String) {
        let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        let enabled = || true;
        let code = run_read_to(
            &args,
            &mut Context {
                store,
                reach: &mut NoReach,
                term: &mut Quiet,
                remote_dev_enabled: &enabled,
                now: 0,
            },
            &mut stdout,
            &mut stderr,
        );
        (code, stdout, String::from_utf8(stderr).unwrap())
    }

    #[test]
    fn a_command_line_it_does_not_take_prints_the_usage_and_nothing_else() {
        let base = std::env::temp_dir().join(format!("cli-read-{}", uuid::Uuid::new_v4()));
        let store = RemoteProjects::new(Some(base.as_os_str()), None);
        for args in [&[][..], &["a", "b"][..]] {
            let (code, stdout, stderr) = run(&store, args);
            assert_eq!((code, stdout.len()), (64, 0));
            assert_eq!(stderr.trim_end(), USAGE);
        }
        // A spec that is not `<host>:<absolute path in normal form>` is one line and exit 1.
        for spec in [
            "example-host",
            "example-host:relative",
            "example-host:/a/../b",
            "example-host:/a//b",
            "example-host:/a/",
        ] {
            let (code, stdout, stderr) = run(&store, &[spec]);
            assert_eq!((code, stdout.len()), (1, 0), "{spec}");
            assert!(
                stderr.starts_with("eitri host read: ") && stderr.lines().count() == 1,
                "{spec}: {stderr}"
            );
        }
    }

    #[test]
    fn a_host_with_no_recorded_project_and_one_that_cannot_be_reached_each_say_so() {
        let base = std::env::temp_dir().join(format!("cli-read-{}", uuid::Uuid::new_v4()));
        let hosts = base.join("eitri/hosts");
        agent::private_fs::create_private_dir_all(&hosts, &base).unwrap();
        let store = RemoteProjects::new(Some(base.as_os_str()), None);
        let (code, stdout, stderr) = run(&store, &["example-host:/opt/dep/lib.rs"]);
        assert_eq!((code, stdout.len()), (1, 0));
        assert_eq!(
            stderr,
            "eitri host read: no remote project on example-host is recorded on this machine\n"
        );
        store
            .save(&crate::remote_project::RemoteProject {
                version: 1,
                host: "example-host".into(),
                host_key_sha256: "SHA256:example".into(),
                root: "/srv/project".into(),
                helper_version: "v".into(),
                sync: None,
                created_at: 0,
                queued_stops: vec![],
            })
            .unwrap();
        // The project of another host is no help; this reach has no connection to give.
        let (code, stdout, stderr) = run(&store, &["other-host:/opt/dep/lib.rs"]);
        assert_eq!((code, stdout.len()), (1, 0));
        assert!(stderr.contains("no remote project on other-host"), "{stderr}");
        let (code, stdout, stderr) = run(&store, &["example-host:/opt/dep/lib.rs"]);
        assert_eq!((code, stdout.len()), (1, 0));
        assert!(
            stderr.starts_with("eitri host read: ") && stderr.lines().count() == 1,
            "{stderr}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    fn data(bytes: &[u8]) -> ReadPathChunk {
        ReadPathChunk {
            data: bytes.to_vec(),
            ..Default::default()
        }
    }

    fn end(bytes: &[u8]) -> ReadPathChunk {
        ReadPathChunk {
            sha256: Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect(),
            ..Default::default()
        }
    }

    fn refusal(text: &str) -> ReadPathChunk {
        ReadPathChunk {
            refusal: text.to_owned(),
            ..Default::default()
        }
    }

    #[test]
    fn a_file_is_its_data_and_a_hash_that_matches() {
        assert_eq!(
            judge(&[
                data(b"fn a() {}\n"),
                data(b"fn b() {}\n"),
                end(b"fn a() {}\nfn b() {}\n")
            ])
            .unwrap(),
            b"fn a() {}\nfn b() {}\n"
        );
        assert_eq!(judge(&[end(b"")]).unwrap(), b"");
    }

    #[test]
    fn a_refusal_after_data_voids_it_and_nothing_else_is_a_file() {
        assert_eq!(
            judge(&[data(b"abc"), refusal("grew past the cap")]),
            Err("grew past the cap".to_owned())
        );
        for (chunks, why) in [
            (vec![data(b"abc")], "ended before"),
            (vec![data(b"abc"), end(b"abd")], "does not match"),
            (vec![data(b"abc"), end(b"abc"), data(b"x")], "after the end"),
            (vec![data(b"abc"), end(b"abc"), end(b"abc")], "after the end"),
            (
                vec![ReadPathChunk {
                    data: b"abc".to_vec(),
                    sha256: "0".repeat(64),
                    ..Default::default()
                }],
                "in one piece",
            ),
            (vec![data(&vec![0; LSP_CHUNK_MAX + 1]), end(b"")], "too large"),
            (vec![], "ended before"),
        ] {
            let error = judge(&chunks).unwrap_err();
            assert!(error.contains(why), "{error}");
        }
    }

    #[test]
    fn a_refusal_is_cut_before_it_is_shown() {
        let error = judge(&[refusal(&"x".repeat(5000))]).unwrap_err();
        assert_eq!(error.chars().count(), REFUSAL_SHOWN);
    }
}
