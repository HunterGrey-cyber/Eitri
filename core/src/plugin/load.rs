//! Turning a plugin directory and the user's `init.lua` options into a loaded plugin, and a task name plus
//! page-supplied variables into exactly one program and its arguments. Load refuses whatever is wrong with
//! the user's own configuration; a plugin that is simply not built yet still loads, and each start says so.

use std::collections::BTreeMap;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::manifest::{parse_manifest, Manifest, Piece, SettingKind, Template};
use super::roots::{dir_inside, Roots};

#[derive(Debug, Clone)]
pub struct LoadedPlugin {
    pub manifest: Manifest,
    /// The plugin directory, canonical.
    pub dir: PathBuf,
    /// `<dir>/<site>`: canonical when it exists, else as joined (the site may not be built yet).
    pub site_root: PathBuf,
    /// Every declared setting, `~` expanded.
    pub settings: BTreeMap<String, String>,
    pub roots: Roots,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTask {
    /// The canonical target of `argv[0]`: the file that is executed, inside the plugin directory.
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub timeout: Option<Duration>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskRefusal {
    NoSuchTask,
    BadVar(String),
    OutsideRoots(String),
    /// `argv[0]` is missing or not executable: the plugin is not built. Reported as a run that failed to
    /// spawn, never as a refusal of the page's call.
    NotBuilt(String),
}

/// `~` and `~/...` against `home`; `~user` is refused (no user database lookups).
pub fn expand_home(raw: &str, home: Option<&Path>) -> Result<PathBuf, String> {
    if raw == "~" || raw.starts_with("~/") {
        let home = home.ok_or_else(|| format!("{raw:?}: HOME is not set"))?;
        return Ok(home.join(raw.trim_start_matches('~').trim_start_matches('/')));
    }
    if raw.starts_with('~') {
        return Err(format!("{raw:?}: only ~ and ~/ are expanded"));
    }
    Ok(PathBuf::from(raw))
}

/// A template with every placeholder replaced. A variable missing from `vars` is an error.
pub fn expand(
    template: &Template,
    plugin: &Path,
    settings: &BTreeMap<String, String>,
    vars: &BTreeMap<String, String>,
) -> Result<String, String> {
    let mut out = String::new();
    for piece in template.pieces() {
        match piece {
            Piece::Text(text) => out.push_str(text),
            Piece::Plugin => out.push_str(&plugin.to_string_lossy()),
            Piece::Setting(name) => out.push_str(
                settings
                    .get(name)
                    .ok_or_else(|| format!("setting {name} has no value"))?,
            ),
            Piece::Var(name) => out.push_str(vars.get(name).ok_or_else(|| format!("variable {name} is missing"))?),
        }
    }
    Ok(out)
}

pub fn load_plugin(
    raw_dir: &str,
    given: &BTreeMap<String, String>,
    home: Option<&Path>,
) -> Result<LoadedPlugin, String> {
    let dir = expand_home(raw_dir, home)?;
    if !dir.is_absolute() {
        return Err(format!("{raw_dir:?} is not an absolute path"));
    }
    let dir = std::fs::canonicalize(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let manifest_path = dir.join("eitri-plugin.toml");
    let text = std::fs::read_to_string(&manifest_path).map_err(|e| format!("{}: {e}", manifest_path.display()))?;
    let manifest = parse_manifest(&text).map_err(|e| format!("{}: {e}", dir.display()))?;

    if let Some(unknown) = given.keys().find(|k| !manifest.settings.contains_key(*k)) {
        let known: Vec<&str> = manifest.settings.keys().map(String::as_str).collect();
        return Err(format!(
            "unknown setting {unknown:?} (the manifest declares: {})",
            known.join(", ")
        ));
    }
    let mut settings = BTreeMap::new();
    for (name, setting) in &manifest.settings {
        let raw = given.get(name).unwrap_or(&setting.default);
        let value = expand_home(raw, home)?;
        match setting.kind {
            SettingKind::Dir => {
                if !value.is_absolute() {
                    return Err(format!("setting {name} = {raw:?} is not an absolute path"));
                }
                if value.exists() {
                    if !value.is_dir() {
                        return Err(format!("setting {name} = {raw:?} exists and is not a directory"));
                    }
                } else {
                    std::fs::DirBuilder::new()
                        .recursive(true)
                        .mode(0o700)
                        .create(&value)
                        .map_err(|e| format!("setting {name}: creating {}: {e}", value.display()))?;
                    // The builder's mode passes through the umask; the leaf is made exactly private.
                    std::fs::set_permissions(&value, std::fs::Permissions::from_mode(0o700))
                        .map_err(|e| format!("setting {name}: {}: {e}", value.display()))?;
                }
            }
        }
        settings.insert(name.clone(), value.to_string_lossy().into_owned());
    }

    let no_vars = BTreeMap::new();
    let mut roots = BTreeMap::new();
    for (name, template) in &manifest.roots {
        let raw = expand(template, &dir, &settings, &no_vars)?;
        if !Path::new(&raw).is_absolute() {
            return Err(format!("root {name} = {raw:?} is not absolute"));
        }
        let canonical = std::fs::canonicalize(&raw).map_err(|e| format!("root {name} = {raw:?}: {e}"))?;
        if !canonical.is_dir() {
            return Err(format!("root {name} = {raw:?} is not a directory"));
        }
        roots.insert(name.clone(), canonical);
    }
    let roots = Roots::new(roots);

    for (name, task) in &manifest.tasks {
        let cwd = expand(&task.cwd, &dir, &settings, &no_vars)?;
        dir_inside(&roots, &cwd).map_err(|r| format!("tasks.{name}.cwd: {}", r.message(&cwd)))?;
    }

    let joined = dir.join(&manifest.site);
    let site_root = std::fs::canonicalize(&joined).unwrap_or(joined);
    Ok(LoadedPlugin {
        manifest,
        dir,
        site_root,
        settings,
        roots,
    })
}

impl LoadedPlugin {
    pub fn resolve_task(&self, task: &str, vars: &BTreeMap<String, String>) -> Result<ResolvedTask, TaskRefusal> {
        let decl = self.manifest.tasks.get(task).ok_or(TaskRefusal::NoSuchTask)?;
        for (name, value) in vars {
            let rule = self
                .manifest
                .vars
                .get(name)
                .ok_or_else(|| TaskRefusal::BadVar(format!("variable {name} is not declared")))?;
            if value.len() > rule.max_len || !rule.chars.admits(value) {
                // The refusal echoes at most what would have been admitted, however much the page sent.
                let shown: String = value.chars().take(rule.max_len).collect();
                let ellipsis = if shown.len() < value.len() { "..." } else { "" };
                return Err(TaskRefusal::BadVar(format!(
                    "variable {name} = \"{}{ellipsis}\" is not {:?} of at most {}",
                    shown.escape_debug(),
                    rule.chars,
                    rule.max_len
                )));
            }
        }
        for template in &decl.argv {
            if let Some(missing) = template.vars().into_iter().find(|v| !vars.contains_key(*v)) {
                return Err(TaskRefusal::BadVar(format!("variable {missing} is missing")));
            }
        }
        let expand_arg = |t: &Template| expand(t, &self.dir, &self.settings, vars).map_err(TaskRefusal::BadVar);
        let first = expand_arg(&decl.argv[0])?;
        let program = std::fs::canonicalize(&first).map_err(|e| TaskRefusal::NotBuilt(format!("{first}: {e}")))?;
        if !program.starts_with(&self.dir) {
            return Err(TaskRefusal::OutsideRoots(format!(
                "{first} resolves outside the plugin ({})",
                program.display()
            )));
        }
        let meta = std::fs::metadata(&program).map_err(|e| TaskRefusal::NotBuilt(format!("{first}: {e}")))?;
        if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
            return Err(TaskRefusal::NotBuilt(format!("{first} is not an executable file")));
        }
        let args = decl.argv[1..].iter().map(expand_arg).collect::<Result<Vec<_>, _>>()?;
        let cwd_raw = expand(&decl.cwd, &self.dir, &self.settings, vars).map_err(TaskRefusal::BadVar)?;
        let cwd = dir_inside(&self.roots, &cwd_raw).map_err(|r| TaskRefusal::OutsideRoots(r.message(&cwd_raw)))?;
        Ok(ResolvedTask {
            program,
            args,
            cwd,
            timeout: decl.timeout,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::test_support::{demo_plugin, Scratch};

    fn vars(course: &str, exercise: &str) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("course".to_string(), course.to_string()),
            ("exercise".to_string(), exercise.to_string()),
        ])
    }

    #[test]
    fn a_plugin_loads_with_canonical_dir_and_roots() {
        let s = Scratch::new("load");
        let (dir, settings) = demo_plugin(&s.0);
        let p = load_plugin(dir.to_str().unwrap(), &settings, None).unwrap();
        assert_eq!(p.dir, dir);
        assert_eq!(p.site_root, dir.join("build/site"));
        assert_eq!(
            p.roots.iter().next().map(|(n, r)| (n.as_str(), r.clone())),
            Some(("work", s.0.join("work")))
        );
    }

    #[test]
    fn a_missing_work_dir_is_created_private() {
        use std::os::unix::fs::PermissionsExt;
        let s = Scratch::new("mkwork");
        let (dir, _) = demo_plugin(&s.0);
        let fresh = s.0.join("fresh/work");
        let settings = BTreeMap::from([("work".to_string(), fresh.display().to_string())]);
        load_plugin(dir.to_str().unwrap(), &settings, None).unwrap();
        assert_eq!(std::fs::metadata(&fresh).unwrap().permissions().mode() & 0o777, 0o700);
    }

    #[test]
    fn home_is_expanded_and_a_relative_or_unknown_setting_is_refused() {
        let s = Scratch::new("home");
        let (dir, _) = demo_plugin(&s.0);
        let home = s.0.clone();
        let tilde = BTreeMap::from([("work".to_string(), "~/work".to_string())]);
        let p = load_plugin(dir.to_str().unwrap(), &tilde, Some(home.as_path())).unwrap();
        assert_eq!(p.settings["work"], s.0.join("work").display().to_string());
        let relative = BTreeMap::from([("work".to_string(), "work".to_string())]);
        assert!(load_plugin(dir.to_str().unwrap(), &relative, None)
            .unwrap_err()
            .contains("absolute"));
        let unknown = BTreeMap::from([("wrok".to_string(), "/x".to_string())]);
        assert!(load_plugin(dir.to_str().unwrap(), &unknown, None)
            .unwrap_err()
            .contains("wrok"));
        assert!(expand_home("~other/x", Some(home.as_path())).is_err());
    }

    #[test]
    fn a_task_resolves_to_the_plugins_own_executable_and_arguments() {
        let s = Scratch::new("resolve");
        let (dir, settings) = demo_plugin(&s.0);
        let p = load_plugin(dir.to_str().unwrap(), &settings, None).unwrap();
        let t = p.resolve_task("watch", &vars("rust", "ch04-02-fix-total-len")).unwrap();
        assert_eq!(
            t.program,
            dir.join("target/learn"),
            "the link is resolved and the target is run"
        );
        let work = s.0.join("work").display().to_string();
        assert_eq!(
            t.args,
            [
                "watch",
                "rust",
                "ch04-02-fix-total-len",
                "--work",
                work.as_str(),
                "--json"
            ]
        );
        assert_eq!(t.cwd, s.0.join("work"));
        assert_eq!(t.timeout, None);
    }

    #[test]
    fn a_value_starting_with_a_dash_is_refused() {
        let s = Scratch::new("dash");
        let (dir, settings) = demo_plugin(&s.0);
        let p = load_plugin(dir.to_str().unwrap(), &settings, None).unwrap();
        let long = "a".repeat(41);
        for bad in ["--work", "-rf", "Rust", "", "a/b", long.as_str()] {
            assert!(
                matches!(p.resolve_task("prepare", &vars(bad, "x")), Err(TaskRefusal::BadVar(_))),
                "{bad:?}"
            );
        }
        let mut extra = vars("rust", "x");
        extra.insert("shell".into(), "x".into());
        assert!(matches!(p.resolve_task("prepare", &extra), Err(TaskRefusal::BadVar(m)) if m.contains("shell")));
        let mut missing = vars("rust", "x");
        missing.remove("exercise");
        assert!(matches!(p.resolve_task("prepare", &missing), Err(TaskRefusal::BadVar(m)) if m.contains("exercise")));
        assert!(matches!(
            p.resolve_task("deploy", &vars("a", "b")),
            Err(TaskRefusal::NoSuchTask)
        ));
    }

    #[test]
    fn a_missing_build_or_a_link_out_of_the_plugin_is_refused_at_start_not_at_load() {
        let s = Scratch::new("build");
        let (dir, settings) = demo_plugin(&s.0);
        std::fs::remove_file(dir.join("target/learn")).unwrap();
        let p = load_plugin(dir.to_str().unwrap(), &settings, None).expect("a missing build never fails the load");
        assert!(matches!(
            p.resolve_task("prepare", &vars("a", "b")),
            Err(TaskRefusal::NotBuilt(_))
        ));
        std::fs::remove_file(dir.join("bin/learn")).unwrap();
        std::os::unix::fs::symlink("/bin/sh", dir.join("bin/learn")).unwrap();
        assert!(
            matches!(p.resolve_task("prepare", &vars("a", "b")), Err(TaskRefusal::OutsideRoots(m)) if m.contains("outside the plugin"))
        );
    }

    #[test]
    fn a_dir_setting_that_is_an_existing_file_is_refused() {
        let s = Scratch::new("filedir");
        let (dir, _) = demo_plugin(&s.0);
        let file = s.0.join("not-a-dir");
        std::fs::write(&file, "").unwrap();
        let settings = BTreeMap::from([("work".to_string(), file.display().to_string())]);
        assert!(load_plugin(dir.to_str().unwrap(), &settings, None)
            .unwrap_err()
            .contains("not a directory"));
    }

    #[test]
    fn a_refusal_echoes_a_capped_copy_of_a_long_value() {
        let s = Scratch::new("echo");
        let (dir, settings) = demo_plugin(&s.0);
        let p = load_plugin(dir.to_str().unwrap(), &settings, None).unwrap();
        let huge = "a".repeat(100_000);
        let Err(TaskRefusal::BadVar(message)) = p.resolve_task("prepare", &vars(&huge, "x")) else {
            panic!("a value that long must be refused");
        };
        assert!(message.len() < 500, "{}", message.len());
        assert!(message.contains("..."));
    }
}
