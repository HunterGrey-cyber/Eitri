fn main() -> std::process::ExitCode {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() == Some(std::ffi::OsStr::new("--version")) && args.next().is_none() {
        let commit = env!("EITRI_BUILD_COMMIT");
        let rev7: String = commit.chars().take(7).collect();
        println!("eitri-server {} {rev7}", env!("CARGO_PKG_VERSION"));
        std::process::ExitCode::SUCCESS
    } else {
        eprintln!("not built yet");
        std::process::ExitCode::from(64)
    }
}
