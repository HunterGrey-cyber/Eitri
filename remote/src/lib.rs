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

/// Each command's own usage, for `eitri remote <command> --help`.
const COMMAND_USAGE: &[(&str, &str)] = &[
    (
        "serve",
        "eitri remote serve [--listen loopback|socket|<address>] [--port <port>] [--origin <name>]...",
    ),
    ("stop", "eitri remote stop"),
    ("status", "eitri remote status"),
    ("add-device", "eitri remote add-device <name>"),
    ("devices", "eitri remote devices"),
    ("revoke", "eitri remote revoke <device>"),
    ("grant", "eitri remote grant <device> watch|steer"),
    (
        "ssh-key",
        "eitri remote ssh-key add <file> [--name <name>] [--port <port>]",
    ),
];

/// Whether `word` asks for help.
fn is_help(word: &str) -> bool {
    word == "-h" || word == "--help"
}

/// What `eitri remote [<command>] --help` prints: the command's usage line and what it does, or [`USAGE`].
pub fn help(command: Option<&str>) -> String {
    let Some((name, line)) = command.and_then(|c| COMMAND_USAGE.iter().find(|(name, _)| *name == c)) else {
        return USAGE.to_owned();
    };
    let what = USAGE
        .lines()
        .find_map(|l| l.trim_start().strip_prefix(name).filter(|rest| rest.starts_with(' ')))
        .map(|rest| {
            // The overview line's description, after its argument words and the padding.
            rest.trim_start()
                .split_once("  ")
                .map_or(rest.trim(), |(_, what)| what.trim())
        })
        .unwrap_or_default();
    format!("usage: {line}\n  {what}")
}

/// `eitri remote <args>`: runs the subcommand `args` names and returns the process's exit code (0 to 255). `-h` or
/// `--help`, alone or anywhere after a command, prints the help on stdout and returns 0 without running anything. An
/// unknown or missing subcommand prints [`USAGE`] and returns 2.
pub fn run(args: &[String]) -> i32 {
    let rest = args.get(1..).unwrap_or_default();
    if args.first().is_some_and(|a| is_help(a)) {
        println!("{USAGE}");
        return 0;
    }
    if let Some(command) = args.first().filter(|c| COMMAND_USAGE.iter().any(|(name, _)| name == c)) {
        if rest.iter().any(|a| is_help(a)) {
            println!("{}", help(Some(command)));
            return 0;
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_owned()).collect()
    }

    #[test]
    fn a_subcommand_without_its_argument_and_the_rest_are_usage() {
        // `serve`, `stop` and `status` need no argument and start a real server: their tests are in `serve/`.
        for sub in [
            "add-device",
            // `devices` takes no argument and reads the user's own store: its test reads a store of its own.
            "revoke",
            "grant",
            "ssh-key",
        ] {
            assert_eq!(run(&args(&[sub])), 2, "{sub}");
        }
        assert_eq!(run(&args(&[])), 2, "no subcommand");
        assert_eq!(run(&args(&["enable"])), 2, "an unknown subcommand");
    }

    #[test]
    fn help_at_the_top_level_is_zero() {
        assert_eq!(run(&args(&["-h"])), 0);
        assert_eq!(run(&args(&["--help"])), 0);
    }

    #[test]
    fn help_after_a_command_acts_on_nothing() {
        for help in ["-h", "--help"] {
            // `add-device` would ask a window for an enrollment, `ssh-key add` read a file of that name.
            for words in [
                vec!["add-device", help],
                vec!["devices", help],
                vec!["grant", help],
                vec!["grant", "d1", help],
                vec!["ssh-key", help],
                vec!["ssh-key", "add", help],
            ] {
                assert_eq!(run(&args(&words)), 0, "{words:?}");
            }
        }
    }

    #[test]
    fn help_for_revoke_and_the_server_commands_acts_on_nothing() {
        // Each would act otherwise: take a device named so out of the store, start or stop the server.
        for sub in ["revoke", "serve", "stop", "status"] {
            assert_eq!(run(&args(&[sub, "--help"])), 0, "{sub}");
            assert_eq!(run(&args(&[sub, "-h"])), 0, "{sub}");
        }
    }

    #[test]
    fn each_command_has_its_usage_and_what_it_does() {
        assert_eq!(
            help(Some("add-device")),
            "usage: eitri remote add-device <name>\n  enroll a phone through the focused window"
        );
        assert!(
            help(Some("serve")).ends_with("\n  run the server phones connect to"),
            "{}",
            help(Some("serve"))
        );
        assert!(help(Some("ssh-key")).contains("ssh-key add <file>"));
        for (name, _) in COMMAND_USAGE {
            let text = help(Some(name));
            let what = text.split_once("\n  ").map(|(_, what)| what).unwrap_or_default();
            assert!(!what.is_empty() && !what.contains("  "), "{name}: {text:?}");
        }
        assert_eq!(help(None), USAGE);
    }

    #[test]
    fn a_device_name_that_reads_as_an_option_is_refused() {
        for name in ["-x", "--name", "-"] {
            assert_eq!(cli::add_device(&args(&[name])), 2, "{name}");
        }
    }
}
