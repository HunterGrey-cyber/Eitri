//! `eitri remote add-device`, `devices`, `revoke`, `grant` and `ssh-key`: the device store from a terminal.

/// `eitri remote add-device <name>`: enrolls a phone through the focused window. Not carried out by this build.
pub fn add_device(_args: &[String]) -> i32 {
    crate::not_yet("add-device")
}

/// `eitri remote devices`: lists the enrolled devices. Not carried out by this build.
pub fn devices(_args: &[String]) -> i32 {
    crate::not_yet("devices")
}

/// `eitri remote revoke <device>`: takes a device out. Not carried out by this build.
pub fn revoke(_args: &[String]) -> i32 {
    crate::not_yet("revoke")
}

/// `eitri remote grant <device> watch|steer`: changes what a device may do. Not carried out by this build.
pub fn grant(_args: &[String]) -> i32 {
    crate::not_yet("grant")
}

/// `eitri remote ssh-key add <file>`: lets a phone's SSH key forward to the server. Not carried out by this build.
pub fn ssh_key(_args: &[String]) -> i32 {
    crate::not_yet("ssh-key")
}
