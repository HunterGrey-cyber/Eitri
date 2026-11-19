mod audit;
mod daemon;
mod ensure;
mod gc;
mod peer;
mod probe;
mod run_dir;

use std::ffi::OsString;
use std::io;
use std::process::ExitCode;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const REVISION: &str = env!("EITRI_BUILD_COMMIT");

fn main() -> ExitCode {
    run_dir::save_inherited_umask();
    // A handler, rather than SIG_IGN, lets exec'd children retain the default disposition.
    unsafe {
        libc::signal(libc::SIGHUP, ignore_hup as *const () as libc::sighandler_t);
    }
    match dispatch() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("eitri-server: {}", error.to_string().replace(['\r', '\n'], " "));
            ExitCode::from(1)
        }
    }
}

extern "C" fn ignore_hup(_: libc::c_int) {}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn dispatch() -> io::Result<()> {
    dispatch_with_probe(std::env::args_os().skip(1).collect(), |args| async move {
        probe::run(&args).await
    })
}

fn dispatch_with_probe<F: std::future::Future<Output = io::Result<()>>>(
    mut args: Vec<OsString>,
    probe: impl FnOnce(Vec<OsString>) -> F,
) -> io::Result<()> {
    if args.len() == 1 && args[0] == "--version" {
        println!("eitri-server {VERSION} {}", run_dir::rev7(REVISION));
        return Ok(());
    }
    if args.is_empty() {
        return Err(invalid("expected ensure, connect, list-daemons, stop-daemon or probe"));
    }
    let command = args.remove(0);
    if args.first().is_some_and(|arg| arg == "--args-hex") {
        if args.len() != 2 {
            return Err(invalid("expected one --args-hex value"));
        }
        args = eitri_server_proto::args_hex::decode_args(
            args[1]
                .to_str()
                .ok_or_else(|| invalid("arguments must be hexadecimal"))?,
        )
        .map_err(|error| invalid(error.to_string()))?;
    }
    if command == "complete-folders" {
        return probe::complete_folders(&args);
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    match command.to_str() {
        Some("ensure") if args.is_empty() => runtime.block_on(ensure::ensure()),
        Some("daemon") => {
            let idle = match args.as_slice() {
                [] => daemon::IDLE_EXIT,
                [flag, seconds] if flag == "--idle-seconds" => std::time::Duration::from_secs(
                    seconds
                        .to_str()
                        .and_then(|value| value.parse().ok())
                        .filter(|value| *value > 0)
                        .ok_or_else(|| invalid("idle seconds must be a positive integer"))?,
                ),
                _ => return Err(invalid("unexpected daemon arguments")),
            };
            finish_runtime(runtime, daemon::run(run_dir::Paths::own_from_env()?, idle))
        }
        Some("connect") => {
            // This installation's own daemon, or the one keyed `--daemon <key>` (as `list-daemons` names it in `run`).
            let paths = match args.as_slice() {
                [] => run_dir::Paths::own_from_env()?,
                [flag, key] if flag == "--daemon" => {
                    let name = key.to_str().ok_or_else(|| invalid("daemon key must be UTF-8"))?;
                    let key = daemon_key(name);
                    run_dir::Paths::from_env(&key)?
                }
                _ => return Err(invalid("expected connect [--daemon <key>]")),
            };
            finish_runtime(runtime, ensure::connect(&paths))
        }
        // `--terminals` says the caller saw terminal hosts' records: a helper from before them refuses the flag, so
        // it cannot report a run clear while a terminal it does not know of still runs.
        Some("check-sidecars")
            if (args.len() == 2 && args[1] == "--live-runs")
                || (args.len() == 3 && args[1] == "--live-runs" && args[2] == "--terminals") =>
        {
            ensure::live_sidecar_runs(&std::path::Path::new(&args[0]).join("run"))
        }
        Some("check-sidecars") if args.len() == 1 || (args.len() == 2 && args[1] == "--terminals") => {
            let run = std::path::Path::new(&args[0]).join("run");
            ensure::check_sidecars_stopped(&run)
        }
        Some("list-daemons") if args.is_empty() => runtime.block_on(ensure::list_daemons()),
        // The daemon keyed as `list-daemons` names it in `run`: its installation's name, or its version.
        Some("stop-daemon") if args.len() == 1 => {
            let key = args[0].to_str().ok_or_else(|| invalid("daemon key must be UTF-8"))?;
            ensure::stop_daemon(&run_dir::Paths::from_env(key)?)
        }
        Some("probe") => finish_runtime(runtime, probe(args)),
        // One terminal's process, started by the daemon; not a command a person runs.
        Some("term-host") if args.len() == 2 && args[0] == "--args-fd" && args[1] == "3" => {
            finish_runtime(runtime, async move {
                daemon::terminals::host::run(daemon::terminals::host::read_args()?).await
            })
        }
        _ => Err(invalid("unknown command or unexpected arguments")),
    }
}

fn daemon_key(name: &str) -> String {
    let run_key = name.strip_prefix("i-").is_some_and(|hex| {
        hex.len() == 16
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    });
    if !run_key && run_dir::installation_identity(name).is_some() {
        run_dir::run_key_of(name)
    } else {
        name.to_owned()
    }
}

fn finish_runtime(
    runtime: tokio::runtime::Runtime,
    operation: impl std::future::Future<Output = io::Result<()>>,
) -> io::Result<()> {
    let result = runtime.block_on(operation);
    // A timed-out NSS lookup or an outstanding stdin read may still own a blocking worker.
    runtime.shutdown_background();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_accepts_listed_run_keys_without_hashing_them_again() {
        let installation = "0.3.0-abcdef0";
        let key = run_dir::run_key_of(installation);
        assert_eq!(daemon_key(&key), key);
        assert_eq!(daemon_key(installation), key);
        assert_eq!(daemon_key("0.3.0"), "0.3.0");
    }

    #[test]
    fn probe_shutdown_does_not_join_a_stalled_lookup() {
        const CHILD: &str = "EITRI_TEST_STALLED_PROBE";
        if std::env::var_os(CHILD).is_some() {
            let error = dispatch_with_probe(vec!["probe".into()], |_| async {
                let (entered, ready) = tokio::sync::oneshot::channel();
                tokio::task::spawn_blocking(move || {
                    let _ = entered.send(());
                    std::thread::park();
                });
                ready.await.unwrap();
                Err(io::Error::new(io::ErrorKind::TimedOut, "passwd lookup timed out"))
            })
            .unwrap_err();
            eprintln!("eitri-server: {error}");
            return;
        }
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::probe_shutdown_does_not_join_a_stalled_lookup",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while child.try_wait().unwrap().is_none() {
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                panic!("probe runtime waited for the stalled lookup");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("passwd lookup timed out"));
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
    }
}
