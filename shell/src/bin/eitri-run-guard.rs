//! `eitri-run-guard`: the guard one plugin task runs under (`eitri_core::run_guard`), as a binary of its own.
//! `eitri run-guard` does the same, but every guard of the full `eitri` maps and relocates GTK and WebKitGTK for
//! the life of its task; this one links neither. The window starts it from beside its own executable.

fn main() {
    // Asked by a window that wants to know this helper is its own build's guard, before it starts tasks under it.
    if std::env::args_os()
        .nth(1)
        .is_some_and(|a| a == eitri_core::run_guard::FINGERPRINT_FLAG)
    {
        println!("{}", eitri_core::run_guard::fingerprint());
        return;
    }
    eitri_core::run_guard::serve_inherited()
}
