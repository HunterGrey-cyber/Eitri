//! The probe script selects probes and their operation counts. It accepts JSON from the runner
//! or a hand-written file.
//!
//! ```json
//! {"run": ["cal", "p1", "p2"], "counts": {"p1_moves": 1000, "p2_cycles": 500}}
//! ```
//!
//! The `p6` probe uses `p6_cycles` for pooled terminals and the page, and `p6_view_cycles` for
//! creating and ending the window's chat views. The latter defaults to `p6_cycles` when absent.

use std::collections::BTreeMap;

/// Every probe name a script may list, in the order a full run takes them. `cal` is the calibration
/// loop that every cost comparison divides by.
pub(super) const PROBES: &[&str] = &["cal", "p1", "p2", "p3", "p4", "p5", "p6", "p7", "p8"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Script {
    pub(super) run: Vec<String>,
    counts: BTreeMap<String, u64>,
}

impl Script {
    pub(super) fn parse(text: &str) -> Result<Script, String> {
        let value: serde_json::Value = serde_json::from_str(text).map_err(|e| format!("not JSON: {e}"))?;
        let object = value.as_object().ok_or("the script must be a JSON object")?;
        let mut run = Vec::new();
        match object.get("run") {
            None => run.extend(PROBES.iter().map(|p| (*p).to_owned())),
            Some(list) => {
                for item in list.as_array().ok_or("\"run\" must be a list of probe names")? {
                    let name = item.as_str().ok_or("\"run\" must be a list of probe names")?;
                    if !PROBES.contains(&name) {
                        return Err(format!("unknown probe {name:?}; the probes are {PROBES:?}"));
                    }
                    run.push(name.to_owned());
                }
            }
        }
        let mut counts = BTreeMap::new();
        if let Some(map) = object.get("counts") {
            for (key, number) in map.as_object().ok_or("\"counts\" must be an object")? {
                let n = number
                    .as_u64()
                    .ok_or_else(|| format!("count {key:?} must be a non-negative integer"))?;
                counts.insert(key.clone(), n);
            }
        }
        Ok(Script { run, counts })
    }

    /// The number the script gives `key`, else `default`.
    pub(super) fn count(&self, key: &str, default: u64) -> u64 {
        self.counts.get(key).copied().unwrap_or(default)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_script_without_a_list_runs_every_probe_in_order() {
        let script = Script::parse("{}").expect("an empty object is a script");
        assert_eq!(script.run, PROBES);
    }

    #[test]
    fn counts_fall_back_to_the_given_default() {
        let script = Script::parse(r#"{"run": ["p1"], "counts": {"p1_moves": 7}}"#).expect("parses");
        assert_eq!(script.run, vec!["p1".to_owned()]);
        assert_eq!(script.count("p1_moves", 1000), 7);
        assert_eq!(script.count("p2_cycles", 500), 500);
    }

    #[test]
    fn an_unknown_probe_is_refused_naming_it() {
        let err = Script::parse(r#"{"run": ["p9"]}"#).expect_err("p9 is not a probe");
        assert!(err.contains("p9"), "{err}");
    }

    #[test]
    fn a_count_that_is_not_a_whole_number_is_refused_naming_it() {
        for bad in [
            r#"{"counts": {"p1_moves": -1}}"#,
            r#"{"counts": {"p1_moves": 1.5}}"#,
            r#"{"counts": {"p1_moves": "x"}}"#,
        ] {
            let err = Script::parse(bad).expect_err(bad);
            assert!(err.contains("p1_moves"), "{err}");
        }
    }

    #[test]
    fn text_that_is_not_an_object_is_refused() {
        assert!(Script::parse("[1]").is_err());
        assert!(Script::parse("nope").is_err());
    }
}
