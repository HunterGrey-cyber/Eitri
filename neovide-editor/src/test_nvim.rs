//! The `nvim` on `PATH` the real-nvim tests start, and what it shows for a prompt.

/// `(major, minor)` of the `nvim` on `PATH`, from `nvim --version`'s first line.
pub(crate) fn path_nvim_version() -> (u64, u64) {
    let out = std::process::Command::new("nvim")
        .arg("--version")
        .output()
        .expect("nvim must be on PATH");
    let first = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned();
    let numbers = first
        .strip_prefix("NVIM v")
        .unwrap_or_else(|| panic!("unexpected nvim --version line: {first:?}"));
    let mut parts = numbers
        .split(|c: char| !c.is_ascii_digit())
        .map(|p| p.parse::<u64>().expect("a version number"));
    (parts.next().unwrap(), parts.next().unwrap())
}

/// The `nvim_get_mode` mode of a `:confirm` dialog (and of the swap file's question) on the `nvim` on `PATH`: `r?` from
/// 0.12, `c` on 0.11.
pub(crate) fn prompt_mode() -> &'static str {
    if path_nvim_version() >= (0, 12) {
        "r?"
    } else {
        "c"
    }
}
