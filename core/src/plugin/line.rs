/// The Lua function an init.lua line calls; the line is assembled from it, so no literal here spells a call.
const LOAD_CALL: &str = "eitri.plugin.load";

/// The init.lua line for a manifest id. The id grammar excludes quotes and Lua syntax.
pub fn plugin_line(id: &str) -> Option<String> {
    super::manifest::id_ok(id).then(|| format!("{LOAD_CALL}(\"{id}\")"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_line_only_quotes_manifest_ids() {
        for id in ["markdown", "agents-top", "0", &"a".repeat(40)] {
            assert_eq!(plugin_line(id), Some(format!("{LOAD_CALL}(\"{id}\")")));
        }
        for id in [
            "",
            "-a",
            "a-",
            "Markdown",
            "a_b",
            "a/b",
            "../x",
            "a\"",
            "a\\b",
            "a\n",
            "a\0",
            "页",
            &"a".repeat(41),
        ] {
            assert_eq!(plugin_line(id), None, "{id:?}");
        }
    }
}
