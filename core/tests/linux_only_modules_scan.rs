//! Modules that call Linux-only interfaces are declared for Linux only, so `eitri-core` and `agent` still build for
//! the macOS host, which has no second window. Read as text through `source_scan`; whitespace and line breaks
//! between the attribute and the `mod` line do not matter.

use eitri_core::source_scan::{code_only, read_required, rust_sources};

/// Each Linux-only module: the file that declares it, its name, its own source. A module whose own source is a
/// `mod.rs` is a whole directory: every file under that directory may name Linux-only calls.
const LINUX_ONLY: &[(&str, &str, &str)] = &[
    (
        "core/src/window_link/mod.rs",
        "fdpass",
        "core/src/window_link/fdpass.rs",
    ),
    ("agent/src/lib.rs", "child", "agent/src/child.rs"),
    ("core/src/window_link/mod.rs", "link", "core/src/window_link/link.rs"),
    ("core/src/window_link/mod.rs", "start", "core/src/window_link/start.rs"),
    ("core/src/lib.rs", "run_guard", "core/src/run_guard.rs"),
    ("core/src/lib.rs", "attachments", "core/src/attachments/isolate.rs"),
    ("core/src/lib.rs", "ide_server", "core/src/ide_server/channels.rs"),
    ("desk/src/lib.rs", "linked", "desk/src/linked/mod.rs"),
];

/// Names that exist only on Linux among the calls these modules make.
const LINUX_CALLS: &[&str] = &[
    "memfd_create",
    "MSG_CMSG_CLOEXEC",
    "SO_PEERCRED",
    "eventfd",
    "SYS_pidfd_",
    "PR_SET_PDEATHSIG",
];

/// The directories whose modules must be portable unless listed above.
const PORTABLE_DIRS: &[&str] = &["core/src/window_link", "desk/src"];

/// Whether `path` is exempt from the second test: it is a Linux-only module's own file, or lies under the directory of
/// a Linux-only module whose own source is a `mod.rs`.
fn exempt(path: &str, entries: &[(&str, &str, &str)]) -> bool {
    entries.iter().any(|(_, _, file)| {
        *file == path
            || file
                .strip_suffix("mod.rs")
                .is_some_and(|dir| dir.ends_with('/') && path.starts_with(dir))
    })
}

fn squeezed(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn each_linux_only_module_is_declared_for_linux_only() {
    for (decl, name, file) in LINUX_ONLY {
        let own = code_only(&read_required(file).text);
        let uses_another = LINUX_ONLY
            .iter()
            .any(|(_, other, other_file)| other_file != file && own.contains(&format!("{other}::")));
        assert!(
            LINUX_CALLS.iter().any(|call| own.contains(call)) || uses_another,
            "{file} names no Linux-only call and uses no Linux-only module: take it off the list"
        );
        let declaring = read_required(decl).text;
        let wanted = format!("#[cfg(target_os = \"linux\")] pub mod {name};");
        assert!(
            squeezed(&declaring).contains(&wanted),
            "{decl} must declare `{name}` as `{wanted}`"
        );
    }
}

#[test]
fn no_other_module_there_names_a_linux_only_call() {
    for source in rust_sources(PORTABLE_DIRS) {
        if exempt(&source.path, LINUX_ONLY) {
            continue;
        }
        let code = code_only(&source.text);
        for call in LINUX_CALLS {
            assert!(
                !code.contains(call),
                "{} names {call}: declare it for Linux only and add it to this scan's list",
                source.path
            );
        }
    }
}

#[test]
fn a_linux_only_module_with_a_directory_exempts_the_whole_directory() {
    let entries = [
        ("core/src/lib.rs", "run_guard", "core/src/run_guard.rs"),
        ("desk/src/lib.rs", "linked", "desk/src/linked/mod.rs"),
    ];
    assert!(exempt("core/src/run_guard.rs", &entries));
    assert!(exempt("desk/src/linked/mod.rs", &entries));
    assert!(exempt("desk/src/linked/spawn.rs", &entries));
    assert!(exempt("desk/src/linked/registry/tests/tabs.rs", &entries));
    // A single-file module does not exempt its neighbours, and a sibling directory with a shared prefix is not inside.
    assert!(!exempt("core/src/other.rs", &entries));
    assert!(!exempt("desk/src/linked_other/mod.rs", &entries));
    assert!(!exempt("desk/src/lib.rs", &entries));
}
