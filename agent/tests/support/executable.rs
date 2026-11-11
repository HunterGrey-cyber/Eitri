/// Writes in a child so a sibling test's fork cannot inherit a writable script descriptor.
pub fn write_executable_script(
    path: impl AsRef<std::path::Path>,
    script: impl AsRef<[u8]>,
    mode: u32,
) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let status = std::process::Command::new("sh")
        .args(["-c", r#"umask 077 && printf '%s' "$1" > "$2" && chmod "$3" "$2""#, "sh"])
        .arg(std::ffi::OsStr::from_bytes(script.as_ref()))
        .arg(path.as_ref())
        .arg(format!("{mode:o}"))
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!("script writer exited with {status}")))
    }
}
