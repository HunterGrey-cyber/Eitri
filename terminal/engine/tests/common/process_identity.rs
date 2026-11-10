use std::fs;

pub struct ProcessIdentity {
    pid: u32,
    start: u64,
}

impl ProcessIdentity {
    pub fn capture(pid: u32) -> Self {
        Self {
            pid,
            start: start_time(pid).expect("the child is still alive"),
        }
    }

    /// A zombie still has this start time; a reused pid belongs to a different child.
    pub fn is_reaped(&self) -> bool {
        start_time(self.pid) != Some(self.start)
    }
}

fn start_time(pid: u32) -> Option<u64> {
    match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => Some(
            stat.rsplit_once(')')
                .expect("proc stat command")
                .1
                .split_whitespace()
                .nth(19)
                .expect("proc stat start time")
                .parse()
                .expect("numeric start time"),
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => panic!("read process identity: {error}"),
    }
}

#[test]
fn another_start_time_at_the_same_pid_is_not_the_original_child() {
    let mut identity = ProcessIdentity::capture(std::process::id());
    assert!(!identity.is_reaped());
    identity.start = identity.start.checked_add(1).unwrap();
    assert!(identity.is_reaped());
}
