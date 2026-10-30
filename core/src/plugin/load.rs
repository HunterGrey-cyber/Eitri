//! Turning a plugin directory and the user's `init.lua` options into a loaded plugin, and a task name plus
//! page-supplied variables into exactly one program and its arguments. Load refuses whatever is wrong with
//! the user's own configuration; a plugin that is simply not built yet still loads, and each start says so.

use std::collections::BTreeMap;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::manifest::{parse_manifest, Manifest, NetImages, Piece, SettingKind, SettingValue, Template};
use super::roots::{dir_inside, Roots};

#[derive(Debug, Clone)]
pub struct LoadedPlugin {
    pub manifest: Manifest,
    /// The plugin directory, canonical.
    pub dir: PathBuf,
    /// `<dir>/<site>`: canonical when it exists, else as joined (the site may not be built yet).
    pub site_root: PathBuf,
    /// Every `dir` setting, `~` expanded: what a template's `${<setting>}` becomes.
    pub settings: BTreeMap<String, String>,
    /// Every `bool` and `choice` setting's effective value.
    pub values: BTreeMap<String, SettingValue>,
    /// `[net] images` resolved to `https`: the page may load `https` images from any host.
    pub remote_images: bool,
    pub roots: Roots,
    /// The own window's declared root, resolved and checked at load on the startup worker.
    pub window_root: Option<PathBuf>,
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

/// A value as a refusal shows it: text quoted, a boolean bare.
fn shown(value: &SettingValue) -> String {
    match value {
        SettingValue::Text(text) => format!("{text:?}"),
        SettingValue::Bool(b) => b.to_string(),
    }
}

/// [`load_plugin_with`] with every option as text, as a `dir` setting takes it.
pub fn load_plugin(
    raw_dir: &str,
    given: &BTreeMap<String, String>,
    home: Option<&Path>,
) -> Result<LoadedPlugin, String> {
    let given: BTreeMap<String, SettingValue> = given
        .iter()
        .map(|(name, value)| (name.clone(), SettingValue::Text(value.clone())))
        .collect();
    load_plugin_with(raw_dir, &given, home)
}

pub fn load_plugin_with(
    raw_dir: &str,
    given: &BTreeMap<String, SettingValue>,
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
    let mut values = BTreeMap::new();
    for (name, setting) in &manifest.settings {
        let value = given.get(name).unwrap_or(&setting.default);
        match &setting.kind {
            SettingKind::Dir => {
                let SettingValue::Text(raw) = value else {
                    return Err(format!("setting {name} = {}: a dir setting takes a path", shown(value)));
                };
                let path = expand_home(raw, home)?;
                if !path.is_absolute() {
                    return Err(format!("setting {name} = {raw:?} is not an absolute path"));
                }
                if path.exists() {
                    if !path.is_dir() {
                        return Err(format!("setting {name} = {raw:?} exists and is not a directory"));
                    }
                } else {
                    std::fs::DirBuilder::new()
                        .recursive(true)
                        .mode(0o700)
                        .create(&path)
                        .map_err(|e| format!("setting {name}: creating {}: {e}", path.display()))?;
                    // The builder's mode passes through the umask; the leaf is made exactly private.
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
                        .map_err(|e| format!("setting {name}: {}: {e}", path.display()))?;
                }
                settings.insert(name.clone(), path.to_string_lossy().into_owned());
            }
            SettingKind::Bool => {
                if !matches!(value, SettingValue::Bool(_)) {
                    return Err(format!(
                        "setting {name} = {}: a bool setting takes true or false",
                        shown(value)
                    ));
                }
                values.insert(name.clone(), value.clone());
            }
            SettingKind::Choice(allowed) => match value {
                SettingValue::Text(text) if allowed.contains(text) => {
                    values.insert(name.clone(), value.clone());
                }
                _ => {
                    let listed: Vec<String> = allowed.iter().map(|v| format!("{v:?}")).collect();
                    return Err(format!(
                        "setting {name} = {}: a choice setting takes one of {}",
                        shown(value),
                        listed.join(", ")
                    ));
                }
            },
        }
    }
    let remote_images = match &manifest.images {
        Some(NetImages::Https) => true,
        Some(NetImages::Setting(name)) => values.get(name) == Some(&SettingValue::Text("https".to_string())),
        Some(NetImages::Off) | None => false,
    };

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

    let window_root = manifest
        .window
        .as_ref()
        .map(|window| {
            let raw = expand(&window.root, &dir, &settings, &no_vars)?;
            super::roots::lexical(&raw).map_err(|r| format!("window.root: {}", r.message(&raw)))?;
            let canonical = std::fs::canonicalize(&raw).map_err(|e| format!("window.root = {raw:?}: {e}"))?;
            if !roots.iter().any(|(_, root)| root == &canonical) {
                return Err(format!(
                    "window.root = {raw:?}: must equal a declared [roots] directory"
                ));
            }
            Ok(canonical)
        })
        .transpose()?;

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
        values,
        remote_images,
        roots,
        window_root,
    })
}

/// Checks one kernel's loaded plugins, with the load call that declared each one.
/// All paths were resolved on the startup worker; this check performs no filesystem work.
pub fn check_window_roots(plugins: &[(&LoadedPlugin, &str)]) -> Result<(), String> {
    let mut owners = BTreeMap::new();
    for (plugin, call) in plugins {
        if let Some(root) = &plugin.window_root {
            if let Some(earlier) = owners.insert(root, call) {
                return Err(format!(
                    "{earlier} and {call}: [window] root {} is declared twice",
                    root.display()
                ));
            }
        }
    }
    Ok(())
}

impl LoadedPlugin {
    /// Every `bool` and `choice` setting as the page reads it in `host.hello`: a bool as a JSON boolean, a
    /// choice as a JSON string. `dir` settings are not here; a page sees them as roots.
    pub fn page_settings(&self) -> serde_json::Map<String, serde_json::Value> {
        self.values
            .iter()
            .map(|(name, value)| {
                let value = match value {
                    SettingValue::Bool(b) => serde_json::Value::Bool(*b),
                    SettingValue::Text(text) => serde_json::Value::String(text.clone()),
                };
                (name.clone(), value)
            })
            .collect()
    }

    /// A `bool` setting's effective value; `None` for a name that is not a bool setting.
    pub fn bool_setting(&self, name: &str) -> Option<bool> {
        match self.values.get(name) {
            Some(SettingValue::Bool(b)) => Some(*b),
            _ => None,
        }
    }

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

    fn with_window(dir: &Path, root: &str) {
        let path = dir.join("eitri-plugin.toml");
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(
            path,
            format!("{text}\n[window]\nroot = {root:?}\ntitle = 'Demo'\nlayout = {{ module = 'page' }}\n"),
        )
        .unwrap();
    }

    #[test]
    fn window_root_must_equal_a_declared_root() {
        let s = Scratch::new("window-root");
        let (dir, settings) = demo_plugin(&s.0);
        std::fs::create_dir_all(s.0.join("work/child")).unwrap();
        with_window(&dir, "${work}/child");
        let error = load_plugin(dir.to_str().unwrap(), &settings, None).unwrap_err();
        assert!(
            error.contains("window.root") && error.contains("equal a declared"),
            "{error}"
        );
    }

    #[test]
    fn window_root_equality_is_checked_after_expansion_and_canonicalization() {
        let s = Scratch::new("window-link");
        let (dir, settings) = demo_plugin(&s.0);
        std::os::unix::fs::symlink(s.0.join("work"), dir.join("work-link")).unwrap();
        with_window(&dir, "${plugin}/work-link");
        let plugin = load_plugin(dir.to_str().unwrap(), &settings, None).unwrap();
        assert_eq!(plugin.window_root, Some(s.0.join("work")));
    }

    #[test]
    fn two_window_roots_on_one_directory_are_refused_naming_both_calls() {
        let s = Scratch::new("window-duplicates");
        let (dir, settings) = demo_plugin(&s.0);
        with_window(&dir, "${work}");
        let first = load_plugin(dir.to_str().unwrap(), &settings, None).unwrap();
        let second = first.clone();
        let calls = [
            (&first, "eitri.plugin.load('demo') at init.lua:1"),
            (&second, "eitri.plugin.load('other') at init.lua:2"),
        ];
        let error = check_window_roots(&calls).unwrap_err();
        assert!(error.contains(calls[0].1) && error.contains(calls[1].1), "{error}");
        assert!(check_window_roots(&calls[..1]).is_ok());
        let mut other_root = second.clone();
        other_root.window_root = Some(s.0.join("other"));
        assert!(check_window_roots(&[calls[0], (&other_root, calls[1].1)]).is_ok());
        other_root.window_root = None;
        assert!(check_window_roots(&[calls[0], (&other_root, calls[1].1)]).is_ok());
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
    #[test]
    fn bool_and_choice_settings_take_their_defaults_and_a_value_of_their_kind() {
        let s = Scratch::new("kinds");
        let (dir, settings) = demo_plugin(&s.0);
        let p = load_plugin(dir.to_str().unwrap(), &settings, None).unwrap();
        assert_eq!(p.values["auto"], SettingValue::Bool(false));
        assert_eq!(p.values["images"], SettingValue::Text("https".into()));
        assert!(!p.values.contains_key("work"), "a dir setting is not a value");
        assert!(p.settings.contains_key("work") && !p.settings.contains_key("auto"));
        assert!(p.remote_images, "the choice's default is https");
        assert_eq!(p.bool_setting("auto"), Some(false));
        assert_eq!(p.bool_setting("images"), None);
        assert_eq!(p.bool_setting("nope"), None);
        assert_eq!(
            serde_json::Value::Object(p.page_settings()),
            serde_json::json!({"auto": false, "images": "https"})
        );

        let mut given: BTreeMap<String, SettingValue> = settings
            .iter()
            .map(|(k, v)| (k.clone(), SettingValue::Text(v.clone())))
            .collect();
        given.insert("auto".into(), SettingValue::Bool(true));
        given.insert("images".into(), SettingValue::Text("off".into()));
        let p = load_plugin_with(dir.to_str().unwrap(), &given, None).unwrap();
        assert_eq!(p.bool_setting("auto"), Some(true));
        assert!(!p.remote_images);
        assert_eq!(
            serde_json::Value::Object(p.page_settings()),
            serde_json::json!({"auto": true, "images": "off"})
        );
    }

    #[test]
    fn a_value_of_the_wrong_kind_is_refused_naming_the_setting() {
        let s = Scratch::new("wrong-kind");
        let (dir, settings) = demo_plugin(&s.0);
        let with = |name: &str, value: SettingValue| {
            let mut given: BTreeMap<String, SettingValue> = settings
                .iter()
                .map(|(k, v)| (k.clone(), SettingValue::Text(v.clone())))
                .collect();
            given.insert(name.into(), value);
            load_plugin_with(dir.to_str().unwrap(), &given, None).unwrap_err()
        };
        assert_eq!(
            with("auto", SettingValue::Text("yes".into())),
            "setting auto = \"yes\": a bool setting takes true or false"
        );
        assert_eq!(
            with("images", SettingValue::Text("http".into())),
            "setting images = \"http\": a choice setting takes one of \"https\", \"off\""
        );
        assert_eq!(
            with("images", SettingValue::Bool(true)),
            "setting images = true: a choice setting takes one of \"https\", \"off\""
        );
        assert_eq!(
            with("work", SettingValue::Bool(true)),
            "setting work = true: a dir setting takes a path"
        );
    }

    #[test]
    fn net_images_resolve_to_remote_images() {
        let s = Scratch::new("net-images");
        let (dir, settings) = demo_plugin(&s.0);
        let manifest = std::fs::read_to_string(dir.join("eitri-plugin.toml")).unwrap();
        for (images, want) in [("\"https\"", true), ("\"off\"", false)] {
            let text = manifest.replacen("images = \"${images}\"", &format!("images = {images}"), 1);
            std::fs::write(dir.join("eitri-plugin.toml"), text).unwrap();
            let p = load_plugin(dir.to_str().unwrap(), &settings, None).unwrap();
            assert_eq!(p.remote_images, want, "{images}");
        }
        let text = manifest.replacen("images = \"${images}\"\n", "", 1);
        std::fs::write(dir.join("eitri-plugin.toml"), text).unwrap();
        assert!(
            !load_plugin(dir.to_str().unwrap(), &settings, None)
                .unwrap()
                .remote_images
        );
    }
}
