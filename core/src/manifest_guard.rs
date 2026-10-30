//! Asserts this crate's own `Cargo.toml` never grows a GTK/WebKit dependency.
//!
//! `cargo tree -p eitri-core | grep -ci gtk` checks the dependency tree, but the compiler alone
//! cannot reject an unused toolkit dependency added to the manifest. A GTK dependency, used or
//! not, prevents the macOS host from building on this crate. This scan makes that boundary a
//! test failure without a crate-parsing dependency; it lives apart from the code it checks.

const MANIFEST: &str = include_str!("../Cargo.toml");

/// Package names (or the start of one, for a `gtk4-sys`-shaped transitive) that must never appear
/// as a dependency of this crate. `gdk4` needs its own entry: neither `gdk` nor `gdk-` matches it.
const FORBIDDEN: &[&str] = &["gtk4", "webkit6", "glib", "gio", "gdk", "gdk4", "gsk4"];

fn is_forbidden(package: &str) -> bool {
    FORBIDDEN
        .iter()
        .any(|forbidden| package == *forbidden || package.starts_with(&format!("{forbidden}-")))
}

/// The value of `package = "..."` inside `text`, if there is one: a renamed dependency's real
/// package name.
fn package_rename(text: &str) -> Option<&str> {
    let at = text.find("package")?;
    let rest = text[at + "package".len()..].trim_start().strip_prefix('=')?;
    let rest = rest.trim_start().strip_prefix('"')?;
    rest.split('"').next()
}

/// What a table header says about the lines under it.
enum Table {
    /// `[dependencies]`, `[dev-dependencies]`, `[build-dependencies]`, `[target.'cfg(..)'.dependencies]`:
    /// every key is a dependency.
    Dependencies,
    /// `[dependencies.<name>]` (or its dev/build/target forms): one dependency, whose own
    /// `package = "..."` line may rename it.
    OneDependency,
    Other,
}

/// Every line declaring a forbidden dependency, as `core/Cargo.toml:<line>: <text>`.
/// Covers a key in a dependency table (`gtk4 = "0.11"`,
/// `gtk4.version = "0.11"`), a `[dependencies.gtk4]` sub-table (with or without a trailing
/// `# comment` on the header), and a rename (`ui = { package = "gtk4", ... }`, or
/// `package = "gtk4"` in a sub-table).
///
/// **Known gaps, not covered here:** a dotted-key rename (`[dependencies]\nui.package = "gtk4"`)
/// and a `package = "..."` whose value contains the substring `"package"` earlier in the same line
/// (this scan's `package_rename` finds the first `package` token, not necessarily the key). Both
/// are TOML-legal and neither trips this scan. `cargo tree -p eitri-core -e normal | grep -E
/// 'gtk|gdk|glib|webkit'` is the backstop that catches what this text scan does not: it asks Cargo
/// itself, after it has resolved every rename and dotted key, rather than parsing TOML by hand.
fn forbidden_dependencies(manifest: &str) -> Vec<String> {
    let mut table = Table::Other;
    let mut offenders = Vec::new();
    for (n, line) in manifest.lines().enumerate() {
        let trimmed = line.trim();
        let mut offend = || offenders.push(format!("core/Cargo.toml:{}: {trimmed}", n + 1));
        if let Some(name) = trimmed.strip_prefix('[') {
            // A header may carry a trailing `# comment` after its closing `]`
            // (`[dependencies.gtk4] # comment`); take only up to the first `]`, not the end of
            // the line, before trimming the leading `[` a `[[...]]` array-of-tables would leave.
            let name = name.split(']').next().unwrap_or("").trim_start_matches('[').trim();
            table = if name.ends_with("dependencies") {
                Table::Dependencies
            } else if let Some(at) = name.rfind("dependencies.") {
                let package = name[at + "dependencies.".len()..].trim_matches(|c| c == '"' || c == '\'');
                if is_forbidden(package) {
                    offend();
                }
                Table::OneDependency
            } else {
                Table::Other
            };
            continue;
        }
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((key, value)) = trimmed.split_once('=') else {
            continue;
        };
        let key = key.trim().trim_matches(|c| c == '"' || c == '\'');
        let forbidden = match table {
            Table::Dependencies => {
                is_forbidden(key.split('.').next().unwrap_or(key)) || package_rename(value).is_some_and(is_forbidden)
            }
            Table::OneDependency => key == "package" && package_rename(trimmed).is_some_and(is_forbidden),
            Table::Other => false,
        };
        if forbidden {
            offend();
        }
    }
    offenders
}

#[test]
fn manifest_declares_no_gtk_or_webkit_dependency() {
    let offenders = forbidden_dependencies(MANIFEST);
    assert!(
        offenders.is_empty(),
        "eitri-core's whole reason to exist is having no GTK/WebKit dependency -- this crate \
         is what the macOS port builds instead of `shell`, and a toolkit dependency here (used or \
         not) would silently break that:\n{}",
        offenders.join("\n")
    );
}

/// Every shape the scan must see, one manifest each, and the shapes it must not mistake for one.
/// Sub-tables and both rename forms must be checked as well as ordinary dependency keys.
#[test]
fn every_way_a_manifest_can_declare_a_dependency_is_seen() {
    let flagged = [
        "[dependencies]\ngtk4 = \"0.11\"\n",
        "[dependencies]\ngtk4 = { version = \"0.11\", features = [\"v4_14\"] }\n",
        "[dependencies]\ngtk4.version = \"0.11\"\n",
        "[dev-dependencies]\nwebkit6 = \"0.5\"\n",
        "[build-dependencies]\nglib-build-tools = \"0.20\"\n",
        "[target.'cfg(unix)'.dependencies]\ngdk4 = \"0.11\"\n",
        "[dependencies.gtk4]\nversion = \"0.11\"\n",
        "[target.'cfg(target_os = \"linux\")'.dependencies.gtk4]\nversion = \"0.11\"\n",
        "[dependencies]\nui = { package = \"gtk4\", version = \"0.11\" }\n",
        "[dependencies.ui]\npackage = \"gtk4\"\nversion = \"0.11\"\n",
        "[dependencies.gtk4] # comment\nversion = \"0.11\"\n",
    ];
    for manifest in flagged {
        assert_eq!(forbidden_dependencies(manifest).len(), 1, "not flagged:\n{manifest}");
    }
    let clean = [
        "[package]\nname = \"gtk4\"\n",
        "[features]\ngtk4 = []\n",
        "[dependencies]\nmlua = { version = \"0.10\", features = [\"lua54\"] }\n",
        "[dependencies.serde]\nversion = \"1\"\npackage = \"serde\"\n",
        "[dependencies]\n# gtk4 = \"0.11\"\n",
        "[dependencies]\ngtkish = \"1\"\n",
        "[dependencies] # comment\nmlua = \"0.10\"\n",
    ];
    for manifest in clean {
        assert!(forbidden_dependencies(manifest).is_empty(), "flagged:\n{manifest}");
    }
}
