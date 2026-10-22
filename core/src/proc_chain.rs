//! Reading process ancestry from operating-system process records.

/// `start` and its ancestors, nearest first, read from `/proc/<pid>/stat` text. The parent pid is
/// the second field after the last `)`, because the command name may itself hold spaces and
/// parentheses. Stops before pid 1 (init owns no window), on a read or parse failure, on a cycle,
/// and at `max` entries.
pub fn ppid_chain(start: u32, read_stat: &dyn Fn(u32) -> Option<String>, max: usize) -> Vec<u32> {
    let mut chain = Vec::new();
    let mut pid = start;
    while chain.len() < max && !chain.contains(&pid) {
        chain.push(pid);
        let Some(stat) = read_stat(pid) else { break };
        let Some(ppid) = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().nth(1))
            .and_then(|f| f.parse::<u32>().ok())
        else {
            break;
        };
        if ppid <= 1 {
            break;
        }
        pid = ppid;
    }
    chain
}

/// The linux reader for [`ppid_chain`]; other platforms have no `/proc`.
#[cfg(target_os = "linux")]
pub fn proc_stat(pid: u32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()
}

#[cfg(not(target_os = "linux"))]
pub fn proc_stat(_pid: u32) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stat_table(rows: &'static [(u32, &'static str)]) -> impl Fn(u32) -> Option<String> {
        move |pid| rows.iter().find(|(p, _)| *p == pid).map(|(_, s)| s.to_string())
    }

    #[test]
    fn ppid_chain_parses_a_comm_with_spaces_and_parens_and_stops_at_init() {
        static ROWS: &[(u32, &str)] = &[
            (300, "300 (nvim) S 200 300 300 0 -1 4194560 1 0 0 0"),
            (200, "200 (tmux: server) (x) S 100 200 200 0 -1 4194560"),
            (100, "100 (a b) c) S 1 100 100 0 -1 4194560"),
        ];
        assert_eq!(ppid_chain(300, &stat_table(ROWS), 32), [300, 200, 100]);
        // A read failure ends the chain; the start is always there.
        assert_eq!(ppid_chain(999, &stat_table(ROWS), 32), [999]);
        // The cap bounds it.
        assert_eq!(ppid_chain(300, &stat_table(ROWS), 2), [300, 200]);
        // A malformed stat ends it.
        static BAD: &[(u32, &str)] = &[(5, "5 nvim S 1")];
        assert_eq!(ppid_chain(5, &stat_table(BAD), 32), [5]);
        // A loop does not spin.
        static LOOP: &[(u32, &str)] = &[(7, "7 (a) S 8 0"), (8, "8 (b) S 7 0")];
        assert_eq!(ppid_chain(7, &stat_table(LOOP), 32), [7, 8]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn proc_stat_reads_this_process_and_its_chain_is_bounded() {
        let me = std::process::id();
        let chain = ppid_chain(me, &proc_stat, 32);
        assert_eq!(chain[0], me);
        assert!(chain.len() <= 32);
        assert!(!chain.contains(&1));
    }
}
