//! Run a libtest case with a child environment, without changing the test runner's environment.

use std::ffi::OsStr;

const CHILD: &str = "EITRI_TEST_ENVIRONMENT_CHILD";

/// Returns true in the child running this exact test. In the parent, waits for that one test to
/// pass and returns false, so the caller can return before doing the child's work. Ignored tests
/// can use this too: they remain ignored in an ordinary run, and only their own child is selected.
pub fn in_child(vars: &[(&str, Option<&OsStr>)]) -> bool {
    let thread = std::thread::current();
    let test = thread.name().expect("a named libtest thread");
    if std::env::var_os(CHILD).as_deref() == Some(OsStr::new(test)) {
        return true;
    }
    let mut command = std::process::Command::new(std::env::current_exe().expect("the test binary"));
    command
        .args([test, "--exact", "--include-ignored", "--nocapture"])
        .env(CHILD, test);
    for (key, value) in vars {
        match value {
            Some(value) => command.env(key, value),
            None => command.env_remove(key),
        };
    }
    let output = command.output().expect("the child test starts");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "child {test} failed or ran no test:\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    print!("{stdout}");
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isolated_environment_fixture() {
        const KEY: &str = "EITRI_ENVIRONMENT_FIXTURE_VALUE";
        let before = std::env::var_os(KEY);
        if in_child(&[
            (KEY, Some(OsStr::new("child-only"))),
            ("EITRI_ENVIRONMENT_ABSENT", None),
        ]) {
            assert_eq!(std::env::var_os(KEY).as_deref(), Some(OsStr::new("child-only")));
            assert!(std::env::var_os("EITRI_ENVIRONMENT_ABSENT").is_none());
            println!("isolated child stdout marker");
            eprintln!("isolated child stderr marker");
            return;
        }
        assert_eq!(std::env::var_os(KEY), before);
    }

    #[test]
    fn a_passing_childs_output_reaches_its_parent() {
        let output = std::process::Command::new(std::env::current_exe().expect("the test binary"))
            .args([
                "testkit::environment::tests::isolated_environment_fixture",
                "--exact",
                "--nocapture",
            ])
            .env_remove(CHILD)
            .output()
            .expect("the parent test starts");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stdout}\n{stderr}");
        assert_eq!(stdout.matches("isolated child stdout marker").count(), 1, "{stdout}");
        assert_eq!(stderr.matches("isolated child stderr marker").count(), 1, "{stderr}");
    }
}
