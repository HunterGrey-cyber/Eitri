use super::SyncKey;
use crate::remote_project::{RemoteProject, RemoteProjects};
use agent::private_fs;
use agent::ssh::{ChannelLauncher, ControlMaster, SshLauncher, CHANNEL_OPTIONS};
use std::convert::Infallible;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const SCRIPTS: [(&str, &str); 2] = [
    (
        "ssh",
        "#!/bin/sh\nexec \"$EITRI_MUTAGEN_TRANSPORT_EXE\" host mutagen-transport ssh -- \"$@\"\n",
    ),
    (
        "scp",
        "#!/bin/sh\nexec \"$EITRI_MUTAGEN_TRANSPORT_EXE\" host mutagen-transport scp -- \"$@\"\n",
    ),
];

pub(super) fn install(state_home: &Path, transport: &Path) -> io::Result<()> {
    if !state_home.is_absolute() || transport != state_home.join("eitri/mutagen/transport") {
        return Err(io::Error::other("expected Eitri's private Mutagen transport directory"));
    }
    let root = state_home.join("eitri");
    private_fs::require_safe_state_parent(&root, "Mutagen transport scripts")?;
    private_fs::create_private_dir_all(transport, &root)?;
    for (name, script) in SCRIPTS {
        let path = transport.join(name);
        if fs::symlink_metadata(&path).is_ok_and(|meta| meta.mode() & 0o7777 == 0o700)
            && private_fs::read_private_nofollow_capped(&path, script.len() as u64).is_ok_and(|text| text == script)
        {
            continue;
        }
        let temp = transport.with_file_name(format!("transport-{}", uuid::Uuid::new_v4()));
        let result = (|| {
            let mut file = private_fs::open_private(&temp)?;
            file.write_all(script.as_bytes())?;
            file.set_permissions(fs::Permissions::from_mode(0o700))?;
            drop(file);
            fs::rename(&temp, path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
            result?;
        }
    }
    Ok(())
}

const MUTAGEN_OPTIONS: [&str; 3] = [
    "-oConnectTimeout=5",
    "-oServerAliveInterval=10",
    "-oServerAliveCountMax=1",
];

struct Invocation<'a> {
    tool: &'a str,
    key: &'a str,
    options: &'a [OsString],
    operands: &'a [OsString],
}

impl<'a> Invocation<'a> {
    fn parse(args: &'a [OsString]) -> io::Result<Self> {
        let invalid = || io::Error::other("expected ssh|scp -- <Mutagen options> <operands for eitri-<sync key>>");
        let [tool, separator, rest @ ..] = args else {
            return Err(invalid());
        };
        let tool = tool
            .to_str()
            .filter(|tool| matches!(*tool, "ssh" | "scp"))
            .ok_or_else(invalid)?;
        if separator != "--" {
            return Err(invalid());
        }
        let count = rest.iter().take_while(|arg| arg.as_bytes().starts_with(b"-")).count();
        let (options, operands) = rest.split_at(count);
        if !options.iter().all(|arg| {
            arg.to_str().is_some_and(|arg| {
                MUTAGEN_OPTIONS.contains(&arg) || tool == "scp" && matches!(arg, "-C" | "-oCompression=yes")
            })
        }) {
            return Err(io::Error::other("unknown Mutagen transport option"));
        }
        let [first, second] = operands else {
            return Err(invalid());
        };
        let destination = if tool == "ssh" {
            if second.is_empty() || second.as_bytes().starts_with(b"-") {
                return Err(invalid());
            }
            first.to_str().ok_or_else(invalid)?
        } else {
            if first.is_empty() || first.as_bytes().contains(&b':') {
                return Err(invalid());
            }
            let split = second
                .as_bytes()
                .iter()
                .position(|byte| *byte == b':')
                .ok_or_else(invalid)?;
            if split + 1 == second.len() {
                return Err(invalid());
            }
            std::str::from_utf8(&second.as_bytes()[..split]).map_err(|_| invalid())?
        };
        let key = destination
            .strip_prefix("eitri-")
            .filter(|key| {
                key.len() == 16
                    && key
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            })
            .ok_or_else(invalid)?;
        Ok(Self {
            tool,
            key,
            options,
            operands,
        })
    }

    fn command(&self, control: &Path) -> Command {
        let mut command = Command::new(self.tool);
        if self.tool == "ssh" {
            command.arg("-S").arg(control);
        } else {
            let mut option = OsString::from("ControlPath=");
            option.push(control);
            command.arg("-o").arg(option);
        }
        command.args(["-F", "/dev/null", "-o", "ControlMaster=no"]);
        command.args(
            CHANNEL_OPTIONS
                .iter()
                .filter(|option| **option != "-T" || self.tool == "ssh"),
        );
        for option in [
            "ProxyCommand=none",
            "ProxyJump=none",
            "StrictHostKeyChecking=yes",
            "UserKnownHostsFile=/dev/null",
            "GlobalKnownHostsFile=/dev/null",
            "KnownHostsCommand=none",
            "VerifyHostKeyDNS=no",
        ] {
            command.arg("-o").arg(option);
        }
        command.args(self.options).arg("--").args(self.operands);
        command
    }
}

fn changed(host: &str) -> io::Error {
    io::Error::other(format!(
        "the server behind {host} is not the one this mirror syncs with"
    ))
}

fn project_for(store: &RemoteProjects, key: &str) -> io::Result<RemoteProject> {
    let tree = format!("eitri-{key}-tree");
    let git = format!("eitri-{key}-git");
    for project in store.list()?.0 {
        let Ok(Some(sync)) = project.sync_record() else {
            continue;
        };
        if sync.engine == "mutagen"
            && (sync.tree_session.as_ref() == Some(&tree) || sync.git_session.as_ref() == Some(&git))
        {
            if project.host_key_sha256 != sync.host_key_sha256
                || SyncKey::new(&sync.host_key_sha256, &project.root).as_str() != key
            {
                return Err(changed(&project.host));
            }
            return Ok(project);
        }
    }
    Err(io::Error::other(format!(
        "no remote project records a session for eitri-{key}"
    )))
}

fn owned_socket(path: &Path) -> io::Result<fs::Metadata> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.file_type().is_socket() || meta.uid() != private_fs::euid() {
        return Err(io::Error::other("SSH control path is not a socket owned by this user"));
    }
    Ok(meta)
}

fn pinned_socket(master: ControlMaster, expected: &str, key: &str) -> io::Result<PathBuf> {
    let launcher = SshLauncher {
        master,
        helper: PathBuf::new(),
    };
    let _guard = launcher.hold_host_key(expected).map_err(|error| {
        if error
            .get_ref()
            .is_some_and(|cause| cause.is::<agent::ssh::known_hosts::HostKeyRefusal>())
        {
            changed(&launcher.master.host)
        } else {
            error
        }
    })?;
    let source = &launcher.master.control_path;
    let meta = owned_socket(source)?;
    let pinned = agent::socket_path::in_dir(source.parent().expect("master directory"), &format!("mx-{key}"))?;
    match owned_socket(&pinned) {
        Ok(previous) if previous.dev() == meta.dev() && previous.ino() == meta.ino() => return Ok(pinned),
        Ok(_) => fs::remove_file(&pinned)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => (),
        Err(error) => return Err(error),
    }
    fs::hard_link(source, &pinned)?;
    Ok(pinned)
}

pub fn exec(args: &[OsString]) -> io::Result<Infallible> {
    let invocation = Invocation::parse(args)?;
    let state = std::env::var_os("XDG_STATE_HOME");
    let home = std::env::var_os("HOME");
    let ssh_dir = crate::remote_host::cli::ssh_dir(state.as_deref(), home.as_deref())
        .ok_or_else(|| io::Error::other("no SSH state directory"))?;
    let transport = ssh_dir.with_file_name("mutagen").join("transport");
    let exe = std::env::current_exe()?;
    let path = std::env::var_os("PATH").unwrap_or_default();
    let entries: Vec<_> = super::daemon::absolute_path_entries(&path)
        .into_iter()
        .filter(|entry| entry != &transport && !same_directory(entry, &transport))
        .collect();
    if entries.is_empty() {
        return Err(io::Error::other(
            "no executable search path outside the Mutagen transport",
        ));
    }
    let clean = std::env::join_paths(entries).map_err(io::Error::other)?;
    if path != clean {
        return Err(Command::new(exe)
            .args(["host", "mutagen-transport"])
            .args(args)
            .env("PATH", clean)
            .exec());
    }
    let project = project_for(&RemoteProjects::new(state.as_deref(), home.as_deref()), invocation.key)?;
    let master = ControlMaster::ensure(&project.host, &ssh_dir, &exe).map_err(io::Error::other)?;
    let control = pinned_socket(master, &project.host_key_sha256, invocation.key)?;
    Err(invocation.command(&control).exec())
}

fn same_directory(first: &Path, second: &Path) -> bool {
    matches!((fs::metadata(first), fs::metadata(second)), (Ok(first), Ok(second)) if first.dev() == second.dev() && first.ino() == second.ino())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::DirBuilderExt;
    use std::process::Output;

    const HOST: &str = "example-host";
    const KEY: &str = "SHA256:example";
    const ROOT: &str = "/srv/project";
    const FAKE: &str = r#"#!/usr/bin/env python3
import json, os, pathlib, socket, stat, sys
state = pathlib.Path(os.environ['XDG_STATE_HOME'])
args = sys.argv[1:]
if args == ['-V']:
    sys.exit(print('OpenSSH_9.9p1', file=sys.stderr))
if '-O' in args or 'ControlMaster=yes' in args:
    assert args[-1] == 'example-host'
    path = pathlib.Path(args[args.index('-S') + 1])
    if '-O' in args:
        assert args[args.index('-O') + 1] == 'check'
        sys.exit(0 if path.is_socket() else 1)
    if sys.platform == 'linux':
        os.mknod(path, stat.S_IFSOCK | 0o600)
    else:
        socket.socket(socket.AF_UNIX).bind(str(path))
    observed = os.environ['OBSERVED']
    if observed:
        path.with_name('kh-' + path.name[3:]).write_text(json.dumps(['example-host', 'ssh-ed25519', observed]))
    (state / 'master').write_text(str(path))
else:
    (state / 'channel').write_text(json.dumps([pathlib.Path(sys.argv[0]).name, *args]))
"#;

    struct Fixture {
        state: PathBuf,
        dest: String,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.state);
        }
    }

    impl Fixture {
        fn new() -> Self {
            let mut base = agent::state_dirs::test_temp_dir();
            if base.as_os_str().len() > 36 {
                base = PathBuf::from("/tmp");
            }
            let state = base.join(format!("mt-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]));
            fs::DirBuilder::new().mode(0o700).create(&state).unwrap();
            let dest = format!("eitri-{}", SyncKey::new(KEY, Path::new(ROOT)));
            let fixture = Self { state, dest };
            let project = serde_json::from_value(serde_json::json!({
                "version": 1, "host": HOST, "host_key_sha256": KEY, "root": ROOT, "created_at": 0,
                "helper_version": "example", "sync": {"engine": "mutagen", "host_key_sha256": KEY,
                "tree_session": format!("{}-tree", fixture.dest)}
            }))
            .unwrap();
            RemoteProjects::new(Some(fixture.state.as_os_str()), None)
                .save(&project)
                .unwrap();
            for tool in ["ssh", "scp"] {
                let path = fixture.state.join(tool);
                private_fs::write_private(&path, FAKE.as_bytes()).unwrap();
                fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
            }
            fixture
        }

        fn run(&self, tool: &str, args: &[&str], key: &str) -> Output {
            let mut argv = vec![tool, "--"];
            argv.extend_from_slice(&MUTAGEN_OPTIONS);
            argv.extend_from_slice(args);
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "remote_sync::transport::tests::transport_child",
                    "--nocapture",
                ])
                .env("TRANSPORT_TEST_ARGV", serde_json::to_string(&argv).unwrap())
                .env("XDG_STATE_HOME", &self.state)
                .env("HOME", &self.state)
                .env(
                    "PATH",
                    std::env::join_paths([self.state.as_path(), Path::new("/usr/bin")]).unwrap(),
                )
                .env("OBSERVED", key)
                .output()
                .unwrap()
        }

        fn channel(&self) -> Vec<String> {
            serde_json::from_slice(&fs::read(self.state.join("channel")).unwrap()).unwrap()
        }
    }

    #[test]
    fn transport_child() {
        let Ok(args) = std::env::var("TRANSPORT_TEST_ARGV") else {
            return;
        };
        let args: Vec<String> = serde_json::from_str(&args).unwrap();
        let error = exec(&args.into_iter().map(OsString::from).collect::<Vec<_>>()).unwrap_err();
        eprintln!("{error}");
        std::process::exit(255);
    }

    #[test]
    fn transport_maps_the_destination_to_the_records_master_and_adds_the_options() {
        let fixture = Fixture::new();
        let remote = ".mutagen/agents/0.18.1/mutagen-agent synchronizer --log-level=info";
        let output = fixture.run("ssh", &[&fixture.dest, remote], KEY);
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let args = fixture.channel();
        assert_eq!(args[..2], ["ssh", "-S"]);
        let master = fs::read_to_string(fixture.state.join("master")).unwrap();
        assert_eq!(
            owned_socket(Path::new(&master)).unwrap().ino(),
            owned_socket(Path::new(&args[2])).unwrap().ino()
        );
        let expected = [
            "-F",
            "/dev/null",
            "-o",
            "ControlMaster=no",
            "-T",
            "-o",
            "BatchMode=yes",
            "-o",
            "RemoteCommand=none",
            "-o",
            "RequestTTY=no",
            "-o",
            "ClearAllForwardings=yes",
            "-o",
            "ForwardAgent=no",
            "-o",
            "ForwardX11=no",
            "-o",
            "PermitLocalCommand=no",
            "-o",
            "ProxyCommand=none",
            "-o",
            "ProxyJump=none",
            "-o",
            "StrictHostKeyChecking=yes",
            "-o",
            "UserKnownHostsFile=/dev/null",
            "-o",
            "GlobalKnownHostsFile=/dev/null",
            "-o",
            "KnownHostsCommand=none",
            "-o",
            "VerifyHostKeyDNS=no",
        ];
        assert_eq!(
            args[3..],
            [expected.as_slice(), &MUTAGEN_OPTIONS, &["--", &fixture.dest, remote]].concat()
        );
    }

    #[test]
    fn transport_refuses_any_other_destination() {
        let fixture = Fixture::new();
        for (tool, args) in [
            ("ssh", vec![HOST, "cmd"]),
            ("ssh", vec!["-oProxyCommand=true", &fixture.dest, "cmd"]),
            ("ssh", vec![&fixture.dest, HOST, "cmd"]),
            ("ssh", vec!["eitri-0000000000000000", "cmd"]),
            ("scp", vec!["example-host:src", &format!("{}:dst", fixture.dest)]),
            ("scp", vec!["src", "example-host:dst"]),
        ] {
            assert_eq!(fixture.run(tool, &args, KEY).status.code(), Some(255));
        }
        assert!(!fixture.state.join("master").exists());
        assert!(!fixture.state.join("channel").exists());
    }

    #[test]
    fn transport_refuses_when_the_master_verified_another_key() {
        let fixture = Fixture::new();
        let output = fixture.run("ssh", &[&fixture.dest, "cmd"], "SHA256:other");
        assert_eq!(output.status.code(), Some(255));
        assert!(String::from_utf8_lossy(&output.stderr)
            .contains("the server behind example-host is not the one this mirror syncs with"));
        assert!(!fixture.state.join("channel").exists());
    }

    #[test]
    fn transport_refuses_without_a_kh_record() {
        let fixture = Fixture::new();
        let output = fixture.run("ssh", &[&fixture.dest, "cmd"], "");
        assert_eq!(output.status.code(), Some(255));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(&io::Error::from_raw_os_error(libc::ENOENT).to_string()),
            "{stderr}"
        );
        assert!(!stderr.contains("not the one this mirror syncs with"));
        assert!(!fixture.state.join("channel").exists());
    }

    #[test]
    fn transport_pauses_for_a_record_without_a_key() {
        let fixture = Fixture::new();
        let output = fixture.run("ssh", &[&fixture.dest, "cmd"], "NONE");
        assert_eq!(output.status.code(), Some(255));
        assert!(String::from_utf8_lossy(&output.stderr)
            .contains("the server behind example-host is not the one this mirror syncs with"));
        assert!(!fixture.state.join("channel").exists());
    }

    #[test]
    fn transport_record_io_denial_is_not_a_key_change() {
        let fixture = Fixture::new();
        assert!(fixture.run("ssh", &[&fixture.dest, "cmd"], KEY).status.success());
        let master = PathBuf::from(fs::read_to_string(fixture.state.join("master")).unwrap());
        let id = master
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .strip_prefix("cm-")
            .unwrap();
        fs::set_permissions(
            master.with_file_name(format!("kh-{id}")),
            fs::Permissions::from_mode(0o660),
        )
        .unwrap();
        fs::remove_file(fixture.state.join("channel")).unwrap();
        let output = fixture.run("ssh", &[&fixture.dest, "cmd"], KEY);
        assert_eq!(output.status.code(), Some(255));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("writable by its group"), "{stderr}");
        assert!(!stderr.contains("not the one this mirror syncs with"));
        assert!(!fixture.state.join("channel").exists());
    }

    #[test]
    fn transport_skips_an_unreadable_unrelated_sync_record() {
        let fixture = Fixture::new();
        let store = RemoteProjects::new(Some(fixture.state.as_os_str()), None);
        let project = store.find(Path::new(ROOT)).unwrap().unwrap();
        let mut unrelated = project.clone();
        unrelated.root = PathBuf::from("/srv/earlier");
        unrelated.sync = Some(serde_json::json!({"engine": 42}));
        store.save(&unrelated).unwrap();
        let key = fixture.dest.strip_prefix("eitri-").unwrap();
        assert_eq!(project_for(&store, key).unwrap(), project);
    }

    #[test]
    fn scp_gets_control_path_not_dash_s() {
        let fixture = Fixture::new();
        let dest = format!("{}:agent", fixture.dest);
        let output = fixture.run("scp", &["-C", "agent", &dest], KEY);
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let args = fixture.channel();
        assert_eq!(args[..2], ["scp", "-o"]);
        let control = args[2].strip_prefix("ControlPath=").unwrap();
        assert!(owned_socket(Path::new(control)).is_ok());
        assert!(!args.iter().any(|arg| arg == "-S" || arg == "-T"));
        assert!(args.ends_with(&["-C".into(), "--".into(), "agent".into(), dest]));
    }

    #[test]
    fn transport_scripts_are_fixed_text() {
        let scratch = crate::test_scratch_dir::ScratchDir::new("mx", "scripts");
        let dir = scratch.join("eitri/mutagen/transport");
        install(&scratch, &dir).unwrap();
        install(&scratch, &dir).unwrap();
        for name in ["ssh", "scp"] {
            let path = dir.join(name);
            assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o700);
            assert_eq!(
                fs::read_to_string(path).unwrap(),
                format!("#!/bin/sh\nexec \"$EITRI_MUTAGEN_TRANSPORT_EXE\" host mutagen-transport {name} -- \"$@\"\n")
            );
        }
    }

    #[test]
    fn transport_refuses_a_replaceable_state_root() {
        let scratch = crate::test_scratch_dir::ScratchDir::new("mx", "parent");
        fs::set_permissions(&*scratch, fs::Permissions::from_mode(0o770)).unwrap();
        let dir = scratch.join("eitri/mutagen/transport");
        assert_eq!(
            install(&scratch, &dir).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(!dir.exists());
        assert_eq!(fs::metadata(&*scratch).unwrap().mode() & 0o7777, 0o770);
    }
}
