use std::process::ExitCode;

#[cfg(not(target_os = "macos"))]
fn main() -> ExitCode {
    eprintln!("eitri-mac runs on macOS");
    ExitCode::from(2)
}

#[cfg(target_os = "macos")]
fn main() -> ExitCode {
    eprintln!("eitri-mac: not assembled yet");
    ExitCode::from(2)
}
