//! A sidecar handshake check that exits before the window and project startup paths.

use std::ffi::OsString;
use std::path::Path;

pub fn run(args: &[OsString]) -> gtk4::glib::ExitCode {
    let [path] = args else {
        eprintln!("usage: shell --sidecar-probe <path>");
        return gtk4::glib::ExitCode::from(64);
    };
    match agent::providers::claude_sidecar::probe(Path::new(path)) {
        Ok(report) => {
            println!(
                "ok {} protocol {}.{}",
                report.sidecar_version, report.protocol_major, report.protocol_minor
            );
            gtk4::glib::ExitCode::SUCCESS
        }
        Err(reason) => {
            eprintln!("{reason}");
            gtk4::glib::ExitCode::FAILURE
        }
    }
}
