//! Toolkit-free remote observation of Eitri agents, and the `eitri remote` command line.

#[cfg(target_os = "linux")]
pub mod agents_feed;
pub mod cli;
pub mod serve;

/// What `eitri remote` takes, printed when it is given something else.
pub const USAGE: &str = "usage: eitri remote <command>
  serve                       run the server phones connect to
  stop                        stop the server and close every phone
  status                      say whether the server runs, and how
  add-device <name>           enroll a phone through the focused window
  devices                     list the enrolled devices
  revoke <device>             take a device out
  grant <device> watch|steer  change what a device may do, after the window's yes
  ssh-key add <file>          let a phone's SSH key forward to the server";

/// `eitri remote <args>`: runs the subcommand `args` names and returns the process's exit code (0 to 255). An
/// unknown or missing subcommand prints [`USAGE`] and returns 2.
pub fn run(args: &[String]) -> i32 {
    let rest = args.get(1..).unwrap_or_default();
    match args.first().map(String::as_str) {
        Some("serve") => serve::serve(rest),
        Some("stop") => serve::stop(rest),
        Some("status") => serve::status(rest),
        Some("add-device") => cli::add_device(rest),
        Some("devices") => cli::devices(rest),
        Some("revoke") => cli::revoke(rest),
        Some("grant") => cli::grant(rest),
        Some("ssh-key") => cli::ssh_key(rest),
        _ => {
            eprintln!("{USAGE}");
            2
        }
    }
}

/// What a subcommand this build does not carry out yet prints and returns.
pub(crate) fn not_yet(subcommand: &str) -> i32 {
    eprintln!("eitri remote {subcommand}: not yet");
    2
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_owned()).collect()
    }

    #[test]
    fn remote_subcommands_are_stubs_until_built_and_the_rest_is_usage() {
        for sub in [
            "serve",
            "stop",
            "status",
            "add-device",
            "devices",
            "revoke",
            "grant",
            "ssh-key",
        ] {
            assert_eq!(run(&args(&[sub])), 2, "{sub}");
        }
        assert_eq!(run(&args(&[])), 2, "no subcommand");
        assert_eq!(run(&args(&["enable"])), 2, "an unknown subcommand");
    }
}
