//! `eitri remote serve`, `stop` and `status`: the one server per user that phones connect to.

/// `eitri remote serve`: runs the server. Not carried out by this build.
pub fn serve(_args: &[String]) -> i32 {
    crate::not_yet("serve")
}

/// `eitri remote stop`: stops the server and closes every phone. Not carried out by this build.
pub fn stop(_args: &[String]) -> i32 {
    crate::not_yet("stop")
}

/// `eitri remote status`: whether the server runs, and how. Not carried out by this build.
pub fn status(_args: &[String]) -> i32 {
    crate::not_yet("status")
}
