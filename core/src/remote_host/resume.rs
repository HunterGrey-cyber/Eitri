use agent::ssh::{ConnectError, ControlMaster};

#[derive(Debug)]
pub enum ResumeAction {
    Connected { host: String },
    Restarted { host: String },
    Failed { host: String, error: ConnectError },
}

/// Called from a worker after an injected resume or network-change notification.
pub fn on_resume(masters: &[ControlMaster]) -> Vec<ResumeAction> {
    masters
        .iter()
        .map(|master| {
            let host = master.host.clone();
            if master.round_trip(std::time::Duration::from_secs(5)) {
                ResumeAction::Connected { host }
            } else {
                match master.restart() {
                    Ok(()) => ResumeAction::Restarted { host },
                    Err(error) => ResumeAction::Failed { host, error },
                }
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn resume_restarts_a_live_master_with_a_hung_round_trip() {
        super::super::tests::isolated(
            "remote_host::resume::tests::resume_restarts_a_live_master_with_a_hung_round_trip",
            |dir| {
                let master = ControlMaster::ensure("example-host", dir, Path::new("/eitri")).unwrap();
                std::fs::write(dir.join("hung"), "").unwrap();
                assert!(master.check(std::time::Duration::from_secs(2)));
                assert!(matches!(
                    on_resume(&[master]).as_slice(),
                    [ResumeAction::Restarted { .. }]
                ));
                assert_eq!(std::fs::read_to_string(dir.join("probes")).unwrap(), "probe\n");
                assert_eq!(std::fs::read_to_string(dir.join("starts")).unwrap(), "start\nstart\n");
                assert_eq!(std::fs::read_to_string(dir.join("exits")).unwrap(), "exit\n");
                let argv = std::fs::read_to_string(dir.join("argv")).unwrap();
                assert!(argv.contains("sh -c 'true'\n"));
            },
        );
    }

    #[test]
    fn resume_restarts_a_master_that_fails_check() {
        super::super::tests::isolated(
            "remote_host::resume::tests::resume_restarts_a_master_that_fails_check",
            |dir| {
                let master = ControlMaster::ensure("example-host", dir, Path::new("/eitri")).unwrap();
                assert!(matches!(
                    on_resume(std::slice::from_ref(&master)).as_slice(),
                    [ResumeAction::Connected { .. }]
                ));
                std::fs::remove_file(dir.join("live")).unwrap();
                assert!(matches!(
                    on_resume(&[master]).as_slice(),
                    [ResumeAction::Restarted { .. }]
                ));
                assert_eq!(std::fs::read_to_string(dir.join("starts")).unwrap(), "start\nstart\n");
                assert_eq!(std::fs::read_to_string(dir.join("exits")).unwrap(), "exit\n");
            },
        );
    }
}
