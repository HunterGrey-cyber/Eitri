#![allow(dead_code)]
#[path = "../src/audit.rs"]
mod audit;
#[path = "../src/daemon.rs"]
mod daemon;
#[path = "../src/peer.rs"]
mod peer;
#[path = "../src/run_dir.rs"]
mod run_dir;

use daemon::login_env::{Captured, Environment};
use daemon::sync::ignores::{SyncIgnores, NON_GIT_WARNING};
use eitri_core::remote_sync::ignores::{Matcher, NON_GIT_DEFAULTS};
use std::path::{Path, PathBuf};
use std::process::Command;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const REVISION: &str = env!("EITRI_BUILD_COMMIT");
fn invalid(message: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message.into())
}

struct Fixture {
    dir: PathBuf,
    root: PathBuf,
    env: Environment,
}
impl Fixture {
    fn new(git: bool) -> Self {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "sync-ignores-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        agent::private_fs::create_private_dir_all(&dir, &dir).unwrap();
        let root = dir.join("project");
        std::fs::create_dir(&root).unwrap();
        let home = dir.join("home");
        std::fs::create_dir(&home).unwrap();
        let fixture = Self {
            dir,
            root,
            env: Environment::from([
                ("PATH".into(), "/usr/bin:/bin".into()),
                ("HOME".into(), home.into_os_string()),
            ]),
        };
        if git {
            fixture.git(&["init", "-q"]);
        }
        fixture
    }
    fn git(&self, args: &[&str]) {
        let out = Command::new("/usr/bin/git")
            .args(["-c", "core.fsmonitor=false"])
            .args(args)
            .current_dir(&self.root)
            .env_clear()
            .envs(&self.env)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }
    fn write(&self, rel: &str, bytes: impl AsRef<[u8]>) {
        let path = self.root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
    fn handler(&self) -> SyncIgnores {
        let handler = SyncIgnores::default();
        handler.set_environment(&Ok(Captured {
            env: self.env.clone(),
            fallback: None,
        }));
        handler
    }
    async fn response(&self) -> eitri_server_proto::SyncIgnoresResponse {
        let result = self.handler().response(self.root.to_str().unwrap()).await;
        assert!(result.refusal.is_empty(), "{}", result.refusal);
        result
    }
    fn script(&mut self, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        let bin = self.dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("git"), format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(bin.join("git"), std::fs::Permissions::from_mode(0o700)).unwrap();
        self.env.insert("PATH".into(), bin.into_os_string());
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[tokio::test]
async fn helper_reads_only_gitignores_git_reads() {
    let f = Fixture::new(true);
    f.write(".gitignore", "blocked/\n*.log\n");
    f.write("blocked/.gitignore", "*.secret\n");
    f.write("sub/.gitignore", "/private\n");
    f.write("sub/private", "secret");
    f.write("file.log", "tracked");
    f.git(&["add", "-f", "file.log", "blocked/.gitignore"]);
    let response = f.response().await;
    assert!(!response.patterns.iter().any(|p| p.contains("*.secret")));
    assert_eq!(response.tracked_ignored, ["blocked/.gitignore", "file.log"]);
    let matcher = Matcher::new(&response.patterns);
    assert!(matcher.ignored("sub/private", false));
    assert!(!matcher.ignored("file.log", false));
    assert!(!matcher.ignored("blocked/.gitignore", false));
}

#[tokio::test]
async fn helper_git_runs_with_a_cleared_environment() {
    let mut f = Fixture::new(true);
    f.write(".gitignore", ".env\n");
    f.env.insert("GIT_DIR".into(), "/srv/other".into());
    f.env.insert("GIT_CONFIG_COUNT".into(), "1".into());
    f.env.insert("GIT_CONFIG_KEY_0".into(), "core.excludesFile".into());
    f.env.insert("GIT_CONFIG_VALUE_0".into(), "/srv/other".into());
    f.env.insert("EITRI_TEST_POISON".into(), "poison".into());
    f.script("[ -z \"${GIT_DIR+x}${GIT_CONFIG_COUNT+x}${GIT_CONFIG_KEY_0+x}${GIT_CONFIG_VALUE_0+x}${EITRI_TEST_POISON+x}\" ] || exit 71\n[ \"$1\" = -c ] && [ \"$2\" = core.fsmonitor=false ] || exit 72\n[ -d \"$HOME\" ] || exit 73\nexec /usr/bin/git \"$@\"");
    assert!(Matcher::new(&f.response().await.patterns).ignored(".env", false));
}

#[tokio::test]
async fn oversized_or_non_utf8_gitignore_excludes_its_directory_with_a_problem() {
    let f = Fixture::new(true);
    f.write("sub/.gitignore", vec![b'x'; 1024 * 1024 + 1]);
    f.write("other/.gitignore", b"bad\xff\n");
    let answer = f.response().await;
    assert_eq!(answer.problems.len(), 2);
    assert!(answer
        .problems
        .iter()
        .any(|p| p.contains("sub") && p.contains("exceeds 1 MiB")));
    assert!(answer
        .problems
        .iter()
        .any(|p| p.contains("other") && p.contains("non-UTF-8")));
    let matcher = Matcher::new(&answer.patterns);
    assert!(matcher.ignored("sub/file", false));
    assert!(matcher.ignored("other/file", false));
}

#[tokio::test]
async fn invalid_root_ignore_fails_closed_even_for_tracked_paths() {
    let f = Fixture::new(true);
    f.write(".gitignore", b"*.log\n\xff\n");
    f.write("a.log", "tracked");
    f.git(&["add", "-f", "a.log"]);
    let answer = f.response().await;
    assert!(Matcher::new(&answer.patterns).ignored("a.log", false));
    assert!(answer
        .problems
        .iter()
        .any(|p| p.contains("a.log is tracked but will not sync")));
}

#[tokio::test]
async fn non_git_root_gets_the_defaults_and_the_warning() {
    let f = Fixture::new(false);
    let response = f.response().await;
    assert_eq!(response.patterns, NON_GIT_DEFAULTS);
    assert_eq!(response.problems, [NON_GIT_WARNING]);
    assert!(!response.digest.is_empty());
}

#[tokio::test]
async fn root_below_toplevel_applies_ancestor_gitignores() {
    let f = Fixture::new(true);
    f.write(".gitignore", "/app/secret\n/elsewhere/public\n*.log\n");
    f.write("app/.gitignore", "/nested\n");
    let answer = f.handler().response(f.root.join("app").to_str().unwrap()).await;
    assert!(answer.refusal.is_empty(), "{}", answer.refusal);
    let matcher = Matcher::new(&answer.patterns);
    assert!(matcher.ignored("secret", false));
    assert!(matcher.ignored("nested", false));
    assert!(matcher.ignored("deep/a.log", false));
    assert!(!matcher.ignored("public", false));
}

#[tokio::test]
async fn global_and_info_sources_precede_per_directory_sources() {
    let f = Fixture::new(true);
    let global = f.dir.join("global-ignore");
    std::fs::write(&global, "a\n*.log\n").unwrap();
    f.git(&["config", "core.excludesFile", global.to_str().unwrap()]);
    f.write(".git/info/exclude", "!a\n");
    f.write(".gitignore", "a\n!keep.log\n");
    let answer = f.response().await;
    assert_eq!(answer.patterns, ["a", "*.log", "!a", "a", "!keep.log"]);
}

#[tokio::test]
async fn git_expands_tilde_in_excludes_file() {
    let f = Fixture::new(true);
    std::fs::write(Path::new(&f.env[std::ffi::OsStr::new("HOME")]).join("ignore"), ".env\n").unwrap();
    f.git(&["config", "core.excludesFile", "~/ignore"]);
    assert!(Matcher::new(&f.response().await.patterns).ignored(".env", false));
}

#[tokio::test]
async fn symlinked_gitignore_is_not_read() {
    let f = Fixture::new(true);
    std::fs::write(f.dir.join("ignore"), ".env\n").unwrap();
    std::os::unix::fs::symlink(f.dir.join("ignore"), f.root.join(".gitignore")).unwrap();
    assert!(Matcher::new(&f.response().await.patterns).ignored(".env", false));
}

#[tokio::test]
async fn roots_use_the_same_absolute_and_normal_checks_as_starts() {
    let f = Fixture::new(false);
    for root in [
        "relative",
        "/srv/project/../other",
        "/srv//project",
        "/srv/project/",
        "/srv/project\0",
    ] {
        assert!(!f.handler().response(root).await.refusal.is_empty(), "{root}");
    }
}

#[tokio::test]
async fn git_failure_refuses_instead_of_falling_back_to_defaults() {
    let mut f = Fixture::new(false);
    f.script("echo 'broken repository' >&2\nexit 128");
    let answer = f.handler().response(f.root.to_str().unwrap()).await;
    assert!(answer.refusal.contains("broken repository"));
    assert!(answer.patterns.is_empty());
}

#[tokio::test]
async fn git_output_is_bounded() {
    let mut f = Fixture::new(false);
    f.script("exec /usr/bin/head -c 16777217 /dev/zero");
    let answer = f.handler().response(f.root.to_str().unwrap()).await;
    assert!(answer.refusal.contains("16 MiB"), "{}", answer.refusal);
    f.script("/usr/bin/head -c 10000000 /dev/zero\nexec /usr/bin/head -c 10000000 /dev/zero >&2");
    let answer = f.handler().response(f.root.to_str().unwrap()).await;
    assert!(answer.refusal.contains("16 MiB"), "{}", answer.refusal);
}

#[tokio::test]
async fn unavailable_login_environment_refuses() {
    let handler = SyncIgnores::default();
    handler.set_environment(&Err(std::io::Error::other("login capture failed")));
    assert_eq!(handler.response("/srv/project").await.refusal, "login capture failed");
}

#[tokio::test]
async fn git_deadline_refuses_and_kills_the_child() {
    let mut f = Fixture::new(false);
    f.script(&format!(
        "echo $$ > '{}'\nexec /bin/sleep 60",
        f.dir.join("child-pid").display()
    ));
    let answer = f.handler().response(f.root.to_str().unwrap()).await;
    assert!(answer.refusal.contains("10 s deadline"), "{}", answer.refusal);
    let pid: i32 = std::fs::read_to_string(f.dir.join("child-pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while unsafe { libc::kill(pid, 0) } == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("timed-out git child is still alive");
}

#[tokio::test]
async fn nested_root_rebases_global_and_info_excludes_and_drops_unreachable_negations() {
    let f = Fixture::new(true);
    let global = f.dir.join("global");
    std::fs::write(&global, "/app/from-global\n").unwrap();
    f.git(&["config", "core.excludesFile", global.to_str().unwrap()]);
    f.write(".git/info/exclude", "/app/from-info\n");
    f.write(".gitignore", "/app/blocked/\n!/app/blocked/keep\n");
    f.write("app/blocked/.gitignore", "bad-pattern\n");
    let answer = f.handler().response(f.root.join("app").to_str().unwrap()).await;
    assert!(answer.refusal.is_empty(), "{}", answer.refusal);
    let matcher = Matcher::new(&answer.patterns);
    for path in ["from-global", "from-info", "blocked/keep"] {
        assert!(matcher.ignored(path, false), "{path}");
    }
    assert!(!answer.patterns.iter().any(|p| p.contains("bad-pattern")));
}

#[tokio::test]
async fn excluded_sync_root_never_reads_its_own_gitignore_or_unreachable_negation() {
    let f = Fixture::new(true);
    f.write(".gitignore", "/app/\n!/app/keep\n");
    f.write("app/.gitignore", "!keep\n");
    f.git(&["add", "-f", "app/.gitignore"]);
    let answer = f.handler().response(f.root.join("app").to_str().unwrap()).await;
    assert!(answer.refusal.is_empty(), "{}", answer.refusal);
    assert!(Matcher::new(&answer.patterns).ignored("keep", false));
}

#[tokio::test]
async fn helper_reads_an_ignore_file_even_when_its_filename_is_ignored() {
    let f = Fixture::new(true);
    f.write(".gitignore", ".gitignore\n.env\nblocked/\n");
    f.write("sub/.gitignore", "secret\n");
    f.write("blocked/.gitignore", "unreachable\n");
    let answer = f.response().await;
    let matcher = Matcher::new(&answer.patterns);
    assert!(matcher.ignored(".env", false));
    assert!(matcher.ignored("sub/secret", false));
    assert!(!answer.patterns.iter().any(|p| p.contains("unreachable")));
}

#[tokio::test]
async fn a_broken_repository_refuses_instead_of_becoming_a_non_git_root() {
    let f = Fixture::new(true);
    std::fs::remove_file(f.root.join(".git/HEAD")).unwrap();
    let answer = f.handler().response(f.root.to_str().unwrap()).await;
    assert!(answer.refusal.contains("git rev-parse failed"), "{}", answer.refusal);
    assert!(answer.patterns.is_empty());
}

#[tokio::test]
async fn excluded_root_ignores_later_unanchored_negations_and_reports_every_tracked_file() {
    let f = Fixture::new(true);
    f.write(".gitignore", "/app/\n!*.rs\n");
    f.write("app/x.rs", "tracked");
    f.git(&["add", "-f", "app/x.rs"]);
    let answer = f.handler().response(f.root.join("app").to_str().unwrap()).await;
    assert!(answer.refusal.is_empty(), "{}", answer.refusal);
    assert_eq!(answer.patterns, ["/**"]);
    assert!(answer
        .problems
        .iter()
        .any(|p| p.contains("x.rs is tracked but will not sync")));
}

#[tokio::test]
async fn relative_excludes_file_uses_repository_top_and_missing_configured_file_is_reported() {
    let f = Fixture::new(true);
    f.write("global-ignore", ".env\n");
    f.write("app/README", "data");
    f.git(&["config", "core.excludesFile", "global-ignore"]);
    let answer = f.handler().response(f.root.join("app").to_str().unwrap()).await;
    assert!(Matcher::new(&answer.patterns).ignored(".env", false));
    f.git(&["config", "core.excludesFile", "missing-ignore"]);
    assert!(f.response().await.problems.iter().any(|p| p.contains("missing-ignore")));
    std::fs::create_dir(f.root.join("exclude-directory")).unwrap();
    f.git(&["config", "core.excludesFile", "exclude-directory"]);
    let answer = f.response().await;
    assert!(answer
        .problems
        .iter()
        .any(|p| p.contains("exclude-directory") && p.contains("cannot be read")));
    assert_eq!(answer.patterns, ["/**"]);
    std::os::unix::fs::symlink("global-ignore", f.root.join("symlinked-excludes")).unwrap();
    f.git(&["config", "core.excludesFile", "symlinked-excludes"]);
    let answer = f.response().await;
    assert_eq!(answer.patterns, [".env"]);
    assert!(answer.problems.is_empty(), "{:?}", answer.problems);
}

#[tokio::test]
async fn symlinked_gitignore_reports_and_excludes_its_directory() {
    let f = Fixture::new(true);
    f.write("sub/file", "data");
    std::fs::write(f.dir.join("ignore"), ".env\n").unwrap();
    std::os::unix::fs::symlink(f.dir.join("ignore"), f.root.join("sub/.gitignore")).unwrap();
    let answer = f.response().await;
    assert!(answer
        .problems
        .iter()
        .any(|p| p.contains("sub") && p.contains("symbolic link")));
    assert!(Matcher::new(&answer.patterns).ignored("sub/file", false));
}

#[tokio::test]
async fn fifo_ignore_sources_are_rejected_without_waiting_for_a_writer() {
    use std::os::unix::ffi::OsStrExt;
    for rel in ["sub/.gitignore", "global-ignore"] {
        let f = Fixture::new(true);
        f.write("sub/README", "data");
        let path = f.root.join(rel);
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        if rel == "global-ignore" {
            f.git(&["config", "core.excludesFile", rel]);
        }
        let answer = f.handler().response(f.root.to_str().unwrap()).await;
        assert!(!answer.refusal.contains("deadline"), "{}", answer.refusal);
        if rel == "global-ignore" {
            assert!(answer.refusal.is_empty(), "{}", answer.refusal);
            assert!(answer.problems.iter().any(|p| p.contains("regular file")));
            assert_eq!(answer.patterns, ["/**"]);
        } else {
            assert!(answer.refusal.contains("regular file"), "{}", answer.refusal);
        }
    }
}

#[tokio::test]
async fn submodule_and_nested_repository_use_their_own_ignore_sources() {
    let f = Fixture::new(true);
    f.write(".gitignore", "build/\n*.outer\n");
    for name in ["module", "nested"] {
        f.write(&format!("{name}/.gitignore"), ".env\nnode_modules/\n");
        f.write(&format!("{name}/.env"), "secret");
        f.write(&format!("{name}/node_modules/a"), "data");
        f.write(&format!("{name}/README"), "data");
        f.write(&format!("{name}/build/x"), "tracked");
        f.write(&format!("{name}/file.outer"), "data");
        f.git(&["-C", name, "init", "-q"]);
        f.git(&["-C", name, "add", ".gitignore", "README", "build/x"]);
    }
    f.git(&[
        "update-index",
        "--add",
        "--cacheinfo",
        "160000,1111111111111111111111111111111111111111,module",
    ]);
    let answer = f.response().await;
    let matcher = Matcher::new(&answer.patterns);
    for name in ["module", "nested"] {
        assert!(matcher.ignored(&format!("{name}/.env"), false), "{name}");
        assert!(matcher.ignored(&format!("{name}/node_modules/a"), false), "{name}");
        assert!(!matcher.ignored(&format!("{name}/README"), false), "{name}");
        assert!(!matcher.ignored(&format!("{name}/build/x"), false), "{name}");
        assert!(!matcher.ignored(&format!("{name}/file.outer"), false), "{name}");
    }
}

#[tokio::test]
async fn more_than_ten_thousand_tracked_ignored_paths_refuses_with_the_count() {
    let mut f = Fixture::new(true);
    f.script("case \"$*\" in\n*'ls-files -z --cached -i'*) /usr/bin/seq -f 'file%g' 10001 | /usr/bin/tr '\\n' '\\000';;\n*) exec /usr/bin/git \"$@\";;\nesac");
    let answer = f.handler().response(f.root.to_str().unwrap()).await;
    assert!(answer.refusal.contains("10001"), "{}", answer.refusal);
    assert!(answer.patterns.is_empty());
}

#[tokio::test]
async fn ancestor_whitelist_preserves_secrets_inside_the_reincluded_root() {
    let f = Fixture::new(true);
    f.write("app/.env", "secret");
    f.write("app/key.pem", "secret");
    for text in [".env\n*.pem\n/*\n!/app/\n!.gitignore", "*\n!*/\n!*.rs"] {
        f.write(".gitignore", text);
        let answer = f.handler().response(f.root.join("app").to_str().unwrap()).await;
        assert!(answer.refusal.is_empty(), "{}", answer.refusal);
        let matcher = Matcher::new(&answer.patterns);
        assert!(matcher.ignored(".env", false));
        assert!(matcher.ignored("key.pem", false));
        assert!(!answer.patterns.iter().any(|p| p == "!/**"));
    }
}

#[tokio::test]
async fn failed_nested_repository_discovery_excludes_it_whole_with_a_problem() {
    let f = Fixture::new(true);
    f.write("nested/.env", "secret");
    f.git(&["-C", "nested", "init", "-q"]);
    std::fs::remove_file(f.root.join("nested/.git/HEAD")).unwrap();
    let answer = f.response().await;
    assert!(answer.problems.iter().any(|p| p.contains("nested repository excluded")));
    assert!(Matcher::new(&answer.patterns).ignored("nested/.env", false));
}

#[tokio::test]
async fn ignored_directory_sources_are_never_opened_even_when_tracked_or_nonregular() {
    use std::os::unix::ffi::OsStrExt;
    let f = Fixture::new(true);
    f.write(".gitignore", "blocked/\n");
    f.write("blocked/.gitignore", "data");
    f.git(&["add", "-f", "blocked/.gitignore"]);
    std::fs::remove_file(f.root.join("blocked/.gitignore")).unwrap();
    let c = std::ffi::CString::new(f.root.join("blocked/.gitignore").as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    let answer = f.handler().response(f.root.to_str().unwrap()).await;
    assert!(answer.refusal.is_empty(), "{}", answer.refusal);
    assert!(answer.problems.is_empty(), "{:?}", answer.problems);
}

#[tokio::test]
async fn linked_global_and_info_excludes_are_followed() {
    let f = Fixture::new(true);
    let global = f.dir.join("global-ignore");
    let info = f.dir.join("info-ignore");
    std::fs::write(&global, "*.global\n").unwrap();
    std::fs::write(&info, "*.info\n").unwrap();
    let link = f.dir.join("global-link");
    std::os::unix::fs::symlink(&global, &link).unwrap();
    f.git(&["config", "core.excludesFile", link.to_str().unwrap()]);
    std::fs::remove_file(f.root.join(".git/info/exclude")).unwrap();
    std::os::unix::fs::symlink(&info, f.root.join(".git/info/exclude")).unwrap();
    let answer = f.response().await;
    assert!(answer.problems.is_empty(), "{:?}", answer.problems);
    let matcher = Matcher::new(&answer.patterns);
    assert!(matcher.ignored("deep/a.global", false));
    assert!(matcher.ignored("deep/a.info", false));
}

#[tokio::test]
async fn unsupported_names_are_skipped_without_refusing_other_paths() {
    use std::os::unix::ffi::OsStrExt;
    let f = Fixture::new(true);
    f.write("good/.gitignore", "secret\n");
    for name in [b"control\x01".as_slice(), b"nonutf8\xff"] {
        let dir = f.root.join(std::ffi::OsStr::from_bytes(name));
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join(".gitignore"), "unreachable\n").unwrap();
    }
    f.write("tracked\x02/file", "tracked");
    f.write(".gitignore", "tracked*/\n");
    f.git(&["add", "-f", "tracked\x02/file"]);
    let answer = f.response().await;
    assert!(answer
        .problems
        .iter()
        .any(|p| p.contains("control\\u{1}") && p.contains("is not synced: its name")));
    assert!(answer
        .problems
        .iter()
        .any(|p| p.contains("nonutf8\\xff") && p.contains("not UTF-8")));
    assert!(!answer.patterns.iter().any(|p| p.contains("unreachable")));
    assert!(answer.tracked_ignored.is_empty());
    let matcher = Matcher::new(&answer.patterns);
    assert!(matcher.ignored("control\x01/file", false));
    assert!(matcher.ignored("tracked\x02/file", false));
    assert!(matcher.ignored("good/secret", false));
    assert!(!matcher.ignored("good/file", false));
}

#[tokio::test]
async fn uninitialised_submodule_is_quiet() {
    let f = Fixture::new(true);
    std::fs::create_dir(f.root.join("module")).unwrap();
    f.git(&[
        "update-index",
        "--add",
        "--cacheinfo",
        "160000,1111111111111111111111111111111111111111,module",
    ]);
    let answer = f.response().await;
    assert!(answer.problems.is_empty(), "{:?}", answer.problems);
}

#[tokio::test]
async fn excluded_roots_count_only_git_ignored_paths_and_summarise_problems() {
    for broken_global in [false, true] {
        let mut f = Fixture::new(true);
        f.write(".gitignore", "/app/\n!*.rs\n");
        f.write("app/README", "data");
        if broken_global {
            f.write("broken-global", b"bad\xff\n");
            f.git(&["config", "core.excludesFile", "broken-global"]);
        }
        f.script("case \"$*\" in\n*'ls-files -z --cached -i'*) exit 0;;\n*'ls-files -z --cached'*) /usr/bin/seq -f 'file%g.rs' 10001 | /usr/bin/tr '\\n' '\\000';;\n*) exec /usr/bin/git \"$@\";;\nesac");
        let root = if broken_global {
            f.root.clone()
        } else {
            f.root.join("app")
        };
        let answer = f.handler().response(root.to_str().unwrap()).await;
        assert!(answer.refusal.is_empty(), "{}", answer.refusal);
        assert_eq!(answer.patterns, ["/**"]);
        assert!(answer.tracked_ignored.is_empty());
        assert_eq!(answer.problems.len(), 51);
        assert_eq!(
            answer.problems.last().unwrap(),
            if broken_global {
                "and 9952 more"
            } else {
                "and 9951 more"
            }
        );
    }
}
