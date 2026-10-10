use std::process::ExitCode;

#[cfg(not(target_os = "macos"))]
fn main() -> ExitCode {
    eitri_mac::log_error!("eitri-mac runs on macOS");
    ExitCode::from(2)
}

#[cfg(target_os = "macos")]
fn main() -> ExitCode {
    eitri_mac::appkit::app::run_main()
}
