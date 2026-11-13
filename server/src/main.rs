mod audit;
mod daemon;
mod ensure;
mod gc;
mod peer;
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
    let mut args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if args.len() == 1 && args[0] == "--version" {
        println!("eitri-server {VERSION} {}", run_dir::rev7(REVISION));
        return Ok(());
    }
    if args.is_empty() {
        return Err(invalid("expected ensure, connect, list-daemons or stop-daemon"));
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
            runtime.block_on(daemon::run(run_dir::Paths::from_env(VERSION)?, idle))
        }
        Some("connect") => {
            let version = match args.as_slice() {
                [] => VERSION,
                [flag, version] if flag == "--daemon" => version
                    .to_str()
                    .ok_or_else(|| invalid("daemon version must be UTF-8"))?,
                _ => return Err(invalid("expected connect [--daemon <version>]")),
            };
            let result = runtime.block_on(ensure::connect(&run_dir::Paths::from_env(version)?));
            // Tokio's stdin may have an outstanding blocking read after the socket closes.
            runtime.shutdown_background();
            result
        }
        Some("list-daemons") if args.is_empty() => runtime.block_on(ensure::list_daemons()),
        Some("stop-daemon") if args.len() == 1 => {
            let version = args[0]
                .to_str()
                .ok_or_else(|| invalid("daemon version must be UTF-8"))?;
            ensure::stop_daemon(&run_dir::Paths::from_env(version)?)
        }
        _ => Err(invalid("unknown command or unexpected arguments")),
    }
}
