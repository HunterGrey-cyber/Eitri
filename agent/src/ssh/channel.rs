use std::ffi::{OsStr, OsString};
use std::path::Path;

use super::ControlMaster;

pub const CHANNEL_OPTIONS: &[&str] = &[
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
];

pub fn channel_argv(
    master: &ControlMaster,
    helper: &Path,
    subcommand: &str,
    args: &[&OsStr],
) -> Result<Vec<OsString>, String> {
    super::validate_host(&master.host)?;
    let helper = helper.to_str().filter(|path| super::safe_absolute_path(path));
    let helper = helper.ok_or_else(|| "remote helper path must match /[A-Za-z0-9._/+-]+".to_owned())?;
    if subcommand.is_empty()
        || !subcommand
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        || subcommand.starts_with('-')
    {
        return Err("remote helper subcommand must be an alphanumeric word with optional hyphens".to_owned());
    }
    let bytes = args
        .iter()
        .try_fold(0usize, |total, arg| total.checked_add(4 + arg.len()));
    if bytes.is_none_or(|bytes| bytes > eitri_server_proto::args_hex::MAX_ARGS_BYTES) {
        return Err("arguments exceed the 64 KiB limit".to_owned());
    }
    let hex = eitri_server_proto::args_hex::encode_args(args);
    let hex = if hex.is_empty() { "''" } else { &hex };
    let command = format!("exec {helper} {subcommand} --args-hex {hex}");
    if command.len() >= 120 * 1024 {
        return Err("encoded SSH command must be smaller than 120 KiB".to_owned());
    }
    super::validate_state_path(&master.control_path).map_err(|error| error.to_string())?;
    let mut argv = channel_prefix(master);
    argv.push(command.into());
    Ok(argv)
}

/// A fixed shell program with checked positional arguments; payload bytes go to stdin.
pub fn step_argv(master: &ControlMaster, literal: &'static str, args: &[&str]) -> Result<Vec<OsString>, String> {
    validate_step(literal, args)?;
    super::validate_host(&master.host).map_err(|error| error.to_string())?;
    super::validate_state_path(&master.control_path).map_err(|error| error.to_string())?;
    let mut argv = channel_prefix(master);
    let mut command = format!("sh -c '{literal}'");
    // Keep the no-argument form used by connection health checks stable.
    if !args.is_empty() {
        command.push_str(" sh");
        for arg in args {
            command.push(' ');
            command.push_str(arg);
        }
    }
    argv.push(command.into());
    Ok(argv)
}

pub(crate) fn validate_step(literal: &'static str, args: &[&str]) -> Result<(), String> {
    if literal.contains('\'') || literal.contains('\0') || literal.contains(['\n', '\r']) {
        return Err("step literal must be one line and fit single quotes".into());
    }
    for arg in args {
        let safe = if arg.starts_with('/') {
            super::safe_absolute_path(arg)
        } else {
            !arg.is_empty()
                && *arg != "."
                && *arg != ".."
                && arg.bytes().all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
        };
        if !safe {
            return Err("step argument must be a restricted absolute path or directory name".into());
        }
    }
    Ok(())
}

fn channel_prefix(master: &ControlMaster) -> Vec<OsString> {
    assert!(
        super::validate_state_path(&master.control_path).is_ok(),
        "invalid SSH control path"
    );
    let mut argv = vec!["-S".into(), master.control_path.clone().into_os_string()];
    argv.extend(CHANNEL_OPTIONS.iter().map(OsString::from));
    argv.extend([
        "-o".into(),
        "ControlMaster=no".into(),
        // A mux client ignores these; a fallback connection must fail host-key verification.
        "-o".into(),
        "StrictHostKeyChecking=yes".into(),
        "-o".into(),
        "VerifyHostKeyDNS=no".into(),
        "-o".into(),
        "UserKnownHostsFile=/dev/null".into(),
        "-o".into(),
        "GlobalKnownHostsFile=/dev/null".into(),
        "-o".into(),
        "KnownHostsCommand=none".into(),
        "--".into(),
        master.host.clone().into(),
    ]);
    argv
}

#[cfg(test)]
mod tests {
    use super::*;
    use eitri_server_proto::args_hex::decode_args;
    use std::os::unix::ffi::OsStringExt;

    #[test]
    fn channel_argv_carries_hostile_arguments_only_as_hex() {
        let master = super::super::tests::master_fixture();
        let root = OsStr::new("/srv/a b/$(x)/*");
        let raw = OsString::from_vec(vec![0xff, b'\n', b'\'']);
        let argv = channel_argv(&master, Path::new("/srv/eitri-server"), "exec", &[root, &raw]).unwrap();
        let remote = argv.last().unwrap().to_str().unwrap();
        let hex = remote.strip_prefix("exec /srv/eitri-server exec --args-hex ").unwrap();
        assert!(hex
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()));
        assert_eq!(decode_args(hex).unwrap(), [root.to_os_string(), raw]);
        assert_eq!(argv[argv.len() - 2], "example-host");
        assert_eq!(argv[argv.len() - 3], "--");
        assert!(channel_argv(&master, Path::new("/srv/helper"), "exec;id", &[]).is_err());
    }

    #[test]
    fn encoded_command_stays_below_the_single_argument_limit() {
        let master = super::super::tests::master_fixture();
        let arg = OsString::from("a".repeat(60 * 1024));
        let error = channel_argv(&master, Path::new("/srv/helper"), "probe", &[&arg]).unwrap_err();
        assert!(error.contains("120 KiB"), "{error}");
        let arg = OsString::from("a".repeat(59 * 1024));
        let argv = channel_argv(&master, Path::new("/srv/helper"), "probe", &[&arg]).unwrap();
        assert!(argv.last().unwrap().len() < 120 * 1024);
        let long_helper = format!("/{}", "a".repeat(120 * 1024));
        assert!(channel_argv(&master, Path::new(&long_helper), "probe", &[]).is_err());
    }

    #[test]
    fn fake_ssh_requires_fail_closed_options_on_channels_and_steps() {
        super::super::tests::isolated(
            "ssh::channel::tests::fake_ssh_requires_fail_closed_options_on_channels_and_steps",
            |dir| {
                std::fs::write(dir.join("require-channel"), "").unwrap();
                let master = super::super::tests::master_fixture();
                for argv in [
                    channel_argv(&master, Path::new("/srv/helper"), "probe", &[]).unwrap(),
                    step_argv(&master, "true", &[]).unwrap(),
                ] {
                    let output = std::process::Command::new("ssh").args(argv).output().unwrap();
                    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
                }
            },
        );
    }

    #[test]
    fn helper_path_outside_the_charset_is_refused() {
        let master = super::super::tests::master_fixture();
        for path in ["/", "helper", "/srv/a b", "/srv/$(x)", "/srv/*", "/srv/a\n"] {
            assert!(channel_argv(&master, Path::new(path), "probe", &[]).is_err(), "{path}");
        }
    }

    #[test]
    fn fixed_steps_have_the_channel_options_and_no_data_interpolation() {
        let argv = step_argv(&super::super::tests::master_fixture(), "cat > helper.tmp", &[]).unwrap();
        assert_eq!(argv.last().unwrap(), "sh -c 'cat > helper.tmp'");
        super::super::tests::assert_channel_options(&argv);
    }

    #[test]
    fn step_arguments_are_checked_before_the_remote_shell_sees_them() {
        const STEP: &str = "printf %s \"$1/$2\"";
        let master = super::super::tests::master_fixture();
        let argv = step_argv(&master, STEP, &["/srv/project", "0.3.0-abcdef0"]).unwrap();
        assert_eq!(
            argv.last().unwrap(),
            "sh -c 'printf %s \"$1/$2\"' sh /srv/project 0.3.0-abcdef0"
        );
        for bad in [
            "", ".", "..", "a b", "a\n", "$(id)", "a;b", "a\"b", "a'b", "a\\b", "a/b",
        ] {
            assert!(step_argv(&master, STEP, &["/srv/project", bad]).is_err(), "{bad:?}");
        }
        assert!(step_argv(&master, "printf '%s'", &[]).is_err());
        assert!(step_argv(&master, "true\nfalse", &[]).is_err());
        assert!(step_argv(&master, "true\rfalse", &[]).is_err());
    }
}
