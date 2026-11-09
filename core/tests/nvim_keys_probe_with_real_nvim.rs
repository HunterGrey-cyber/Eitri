//! The probe the nvim-keys feed adds to its report, driven by a real `nvim` with stand-in plugins.
//!
//! `#[ignore]`d: it needs `nvim` on PATH. It spends no tokens, touches no network, opens no window
//! and needs no display -- `--headless` is the whole harness. Each test builds its own config, data,
//! state and cache directories, so nothing of the user's nvim is read, and puts its stand-in plugins
//! where a package manager would: a `pack/*/start/<name>` directory, a `lazy/<name>` directory, a
//! module in `package.loaded`.
//!
//! The stand-ins have the shape of the real plugins as far as the probe can see them: which module
//! lands in `package.loaded`, which global is set, when the mappings appear, and what script a mapping
//! was defined by. `-c` commands run before `VimEnter`, so each test drives `FocusLost` (one of the
//! feed's own re-read events) by hand and lets the debounce fire with `:sleep`.
//!
//! Run: `cargo test -p eitri-core --test nvim_keys_probe_with_real_nvim -- --ignored`

use std::io::{ErrorKind, Read};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};

use eitri_core::nvim_keys::feed::NvimKeysFeed;
use eitri_core::nvim_keys::{parse_report, NvimProbe, NvimReport, PluginState, ProbeKey, Scope};

/// Accepts until `WouldBlock` and reads each connection to EOF; nvim has exited by then, so every
/// sender has already written and closed.
fn drain_all_lines(listener: UnixListener) -> Vec<String> {
    let mut lines = Vec::new();
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                let mut text = String::new();
                stream.read_to_string(&mut text).unwrap();
                lines.extend(text.lines().map(str::to_string));
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => break,
            Err(e) => panic!("accept failed: {e}"),
        }
    }
    lines
}

/// A scratch tree with an nvim config, data, state and cache directory of its own, removed on drop.
struct Rig {
    dir: PathBuf,
}

impl Rig {
    fn new(tag: &str) -> Rig {
        let dir = std::env::temp_dir().join(format!("kp-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Rig { dir }
    }

    fn write(&self, path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// A file under nvim's config directory (`init.lua` is the entry point).
    fn config(&self, rel: &str, text: &str) -> PathBuf {
        let path = self.dir.join("config/nvim").join(rel);
        self.write(&path, text);
        path
    }

    /// A file of a plugin in a `pack/*/start/<name>` directory, which nvim loads after `init.lua`.
    fn pack_file(&self, name: &str, rel: &str, text: &str) {
        self.write(&self.dir.join("data/nvim/site/pack/t/start").join(name).join(rel), text);
    }

    /// A file of a plugin in a `lazy/<name>` directory, as a plugin manager lays them out.
    fn lazy_file(&self, name: &str, rel: &str, text: &str) -> PathBuf {
        let path = self.dir.join("data/nvim/lazy").join(name).join(rel);
        self.write(&path, text);
        path
    }

    /// Runs nvim headless with the feed loaded and `commands` as `-c` commands, then quits; returns
    /// every report it sent, in order.
    fn run(&self, commands: &[&str]) -> Vec<NvimReport> {
        agent::state_dirs::redirect_state_to_a_test_root();
        let mut feed = NvimKeysFeed::new().expect("the feed must bind");
        let listener = feed.take_listener().unwrap();
        let mut command = std::process::Command::new("timeout");
        command.args(["60", "nvim", "--headless", "-i", "NONE"]);
        for arg in feed.nvim_args() {
            command.arg(arg);
        }
        for (k, v) in feed.child_env() {
            command.env(k, v);
        }
        for (var, sub) in [
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_DATA_HOME", "data"),
            ("XDG_STATE_HOME", "state"),
            ("XDG_CACHE_HOME", "cache"),
            ("XDG_CONFIG_DIRS", "no-system-config"),
            ("XDG_DATA_DIRS", "no-system-data"),
        ] {
            command.env(var, self.dir.join(sub));
        }
        for c in commands {
            command.args(["-c", c]);
        }
        let status = command
            .args(["-c", "qa!"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("nvim must be on PATH");
        assert!(status.success(), "nvim ended with {status}");
        let reports = drain_all_lines(listener)
            .iter()
            .map(|line| parse_report(line.as_bytes()).unwrap_or_else(|| panic!("a report that does not parse: {line}")))
            .collect();
        feed.cleanup();
        reports
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn probe(report: &NvimReport) -> &NvimProbe {
    report.probe.as_ref().expect("the report carries a probe")
}

/// Keys of one probed mode and chord, whatever case nvim spells the chord in.
fn keys<'a>(probe: &'a NvimProbe, mode: &str, chord: &str) -> Vec<&'a ProbeKey> {
    probe
        .keys
        .iter()
        .filter(|k| k.mode == mode && k.lhs.eq_ignore_ascii_case(chord))
        .collect()
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn blink_cmp_maps_on_insert_enter_are_seen_only_at_the_next_reread() {
    let rig = Rig::new("blink");
    rig.pack_file("blink.cmp", "plugin/blink_cmp.lua", "require('blink.cmp')\n");
    rig.pack_file(
        "blink.cmp",
        "lua/blink/cmp.lua",
        "vim.api.nvim_create_autocmd('InsertEnter', {\n\
         \x20 callback = function()\n\
         \x20   vim.keymap.set('i', '<C-b>', function() end, { buffer = true, desc = 'Scroll documentation up' })\n\
         \x20 end,\n\
         })\n\
         return {}\n",
    );
    let before = rig.run(&[
        "doautocmd FocusLost",
        "sleep 400m",
        "doautocmd InsertEnter",
        "sleep 400m",
    ]);
    assert_eq!(before.len(), 1, "InsertEnter alone must send nothing");
    let first = probe(&before[0]);
    assert_eq!(first.plugins.get("blink.cmp"), Some(&PluginState::Loaded));
    assert!(
        keys(first, "i", "<C-b>").is_empty(),
        "the map does not exist before InsertEnter: {first:?}"
    );

    let after = rig.run(&[
        "doautocmd FocusLost",
        "sleep 400m",
        "doautocmd InsertEnter",
        "sleep 400m",
        "doautocmd FocusLost",
        "sleep 400m",
    ]);
    assert_eq!(after.len(), 2, "one report before InsertEnter, one at the next re-read");
    let seen = keys(probe(&after[1]), "i", "<C-b>");
    assert_eq!(seen.len(), 1, "{:?}", after[1].probe);
    assert_eq!(seen[0].scope, Scope::Buffer);
    assert!(seen[0].callback && seen[0].rhs.is_none());
    assert_eq!(seen[0].desc.as_deref(), Some("Scroll documentation up"));
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn vim_tmux_navigator_is_loaded_and_its_four_maps_come_from_its_directory() {
    let rig = Rig::new("vtn");
    rig.pack_file(
        "vim-tmux-navigator",
        "plugin/tmux_navigator.vim",
        "if exists('g:loaded_tmux_navigator') | finish | endif\n\
         let g:loaded_tmux_navigator = 1\n\
         command! TmuxNavigateLeft echo 'left'\n\
         nnoremap <silent> <c-h> :<C-U>TmuxNavigateLeft<cr>\n\
         nnoremap <silent> <c-j> :<C-U>TmuxNavigateLeft<cr>\n\
         nnoremap <silent> <c-k> :<C-U>TmuxNavigateLeft<cr>\n\
         nnoremap <silent> <c-l> :<C-U>TmuxNavigateLeft<cr>\n",
    );
    let reports = rig.run(&["doautocmd FocusLost", "sleep 400m"]);
    let seen = probe(reports.last().unwrap());
    assert_eq!(seen.plugins.get("vim-tmux-navigator"), Some(&PluginState::Loaded));
    assert_eq!(seen.plugins.len(), 1, "{:?}", seen.plugins);
    for chord in ["<C-h>", "<C-j>", "<C-k>", "<C-l>"] {
        let found = keys(seen, "n", chord);
        assert_eq!(found.len(), 1, "{chord}: {:?}", seen.keys);
        assert_eq!(found[0].scope, Scope::Global);
        assert!(found[0].rhs.as_deref().unwrap().contains("TmuxNavigateLeft"));
        assert_eq!(found[0].origin.as_deref(), Some("vim-tmux-navigator"));
    }
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn smart_splits_is_loaded_when_its_module_is() {
    let rig = Rig::new("smart");
    rig.pack_file(
        "smart-splits.nvim",
        "plugin/smart_splits.lua",
        "require('smart-splits')\n",
    );
    rig.pack_file(
        "smart-splits.nvim",
        "lua/smart-splits/init.lua",
        "return { move_cursor_left = function() end }\n",
    );
    let reports = rig.run(&["doautocmd FocusLost", "sleep 400m"]);
    let plugins = &probe(reports.last().unwrap()).plugins;
    assert_eq!(plugins.get("smart-splits.nvim"), Some(&PluginState::Loaded));
    assert_eq!(plugins.len(), 1, "{plugins:?}");
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_loaded_lazy_lists_the_plugins_it_will_load_later_as_declared() {
    let rig = Rig::new("lazy");
    rig.config(
        "init.lua",
        "vim.g.loaded_tmux_navigator = 1\n\
         package.loaded.lazy = { plugins = function()\n\
         \x20 return { { name = 'smart-splits.nvim' }, { name = 'blink.cmp' }, { name = 'vim-tmux-navigator' },\n\
         \x20   { name = 'some-other-plugin.nvim' }, 'not a table' }\n\
         end }\n",
    );
    let reports = rig.run(&["doautocmd FocusLost", "sleep 400m"]);
    let plugins = &probe(reports.last().unwrap()).plugins;
    assert_eq!(plugins.get("lazy.nvim"), Some(&PluginState::Loaded));
    assert_eq!(plugins.get("smart-splits.nvim"), Some(&PluginState::Declared));
    assert_eq!(plugins.get("blink.cmp"), Some(&PluginState::Declared));
    assert_eq!(
        plugins.get("vim-tmux-navigator"),
        Some(&PluginState::Loaded),
        "a plugin that is both loaded and listed is loaded"
    );
    assert_eq!(
        plugins.len(),
        4,
        "a plugin outside the closed list is not reported: {plugins:?}"
    );
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_lazy_whose_plugins_call_fails_still_reports_everything_else() {
    let rig = Rig::new("lazyerr");
    rig.config(
        "init.lua",
        "package.loaded.lazy = { plugins = function() error('lazy is not ready') end }\n\
         vim.keymap.set('n', '<C-a>', '<Nop>', { desc = 'mine' })\n",
    );
    let reports = rig.run(&["doautocmd FocusLost", "sleep 400m"]);
    let seen = probe(reports.last().unwrap());
    assert_eq!(seen.plugins.len(), 1, "{:?}", seen.plugins);
    assert_eq!(seen.plugins.get("lazy.nvim"), Some(&PluginState::Loaded));
    assert_eq!(keys(seen, "n", "<C-a>").len(), 1);
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn lazy_is_never_required_when_it_is_not_loaded() {
    let rig = Rig::new("nolazy");
    let marker = rig.dir.join("lazy-required");
    rig.config(
        "init.lua",
        &format!(
            "package.preload.lazy = function()\n\
             \x20 vim.fn.writefile({{ 'required' }}, '{}')\n\
             \x20 return {{ plugins = function() return {{ {{ name = 'blink.cmp' }} }} end }}\n\
             end\n",
            marker.display()
        ),
    );
    let reports = rig.run(&["doautocmd FocusLost", "sleep 400m"]);
    assert!(!reports.is_empty());
    for report in &reports {
        assert!(probe(report).plugins.is_empty(), "{:?}", probe(report).plugins);
    }
    assert!(!marker.exists(), "the probe loaded a module the user did not");
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_chord_set_from_the_config_with_a_desc_is_reported_with_it() {
    let rig = Rig::new("desc");
    rig.config(
        "init.lua",
        "vim.keymap.set('n', '<C-b>', function() end, { desc = 'My own prefix' })\n\
         vim.keymap.set('i', '<C-Space>', '<Nop>', { desc = 'complete' })\n",
    );
    let reports = rig.run(&["doautocmd FocusLost", "sleep 400m"]);
    let seen = probe(reports.last().unwrap());
    let prefix = keys(seen, "n", "<C-b>");
    assert_eq!(prefix.len(), 1, "{:?}", seen.keys);
    assert_eq!(prefix[0].desc.as_deref(), Some("My own prefix"));
    assert!(prefix[0].callback && prefix[0].rhs.is_none());
    assert_eq!(prefix[0].scope, Scope::Global);
    let space = seen
        .keys
        .iter()
        .filter(|k| k.mode == "i" && k.desc.as_deref() == Some("complete"))
        .collect::<Vec<_>>();
    assert_eq!(space.len(), 1, "{:?}", seen.keys);
    assert_eq!(space[0].rhs.as_deref(), Some(""), "nvim stores <Nop> as an empty rhs");
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn origin_is_a_directory_name_or_config_and_never_a_path() {
    let rig = Rig::new("origin");
    rig.pack_file("packed.nvim", "plugin/packed.vim", "inoremap <C-k> packed\n");
    let lazy_script = rig.lazy_file("lazied.nvim", "plugin/lazied.vim", "nnoremap <C-a> lazied\n");
    let config_script = rig.config("maps.vim", "nnoremap <C-j> config\n");
    rig.config(
        "init.lua",
        &format!(
            "vim.cmd.source('{}')\nvim.cmd.source('{}')\n",
            lazy_script.display(),
            config_script.display()
        ),
    );
    let reports = rig.run(&["doautocmd FocusLost", "sleep 400m"]);
    let seen = probe(reports.last().unwrap());
    let origin = |mode: &str, chord: &str| {
        let found = keys(seen, mode, chord);
        assert_eq!(found.len(), 1, "{chord}: {:?}", seen.keys);
        found[0].origin.clone()
    };
    assert_eq!(origin("i", "<C-k>").as_deref(), Some("packed.nvim"));
    assert_eq!(origin("n", "<C-a>").as_deref(), Some("lazied.nvim"));
    assert_eq!(origin("n", "<C-j>").as_deref(), Some("config"));
    for key in &seen.keys {
        assert!(
            key.origin.as_deref().is_none_or(|o| !o.contains('/')),
            "an origin is never a path: {key:?}"
        );
    }
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn over_two_thousand_global_maps_the_probe_still_holds_its_chords() {
    let rig = Rig::new("many");
    rig.config(
        "init.lua",
        "for i = 1, 2100 do vim.keymap.set('n', '<Leader>m' .. i, '<Nop>') end\n\
         vim.keymap.set('n', '<C-b>', '<Nop>', { desc = 'after the flood' })\n\
         vim.keymap.set('t', '<C-h>', '<Nop>')\n",
    );
    let reports = rig.run(&["doautocmd FocusLost", "sleep 400m"]);
    let report = reports.last().unwrap();
    assert_eq!(report.maps.len(), 2000, "the Normal-mode list stays capped");
    let seen = probe(report);
    let prefix = keys(seen, "n", "<C-b>");
    assert_eq!(prefix.len(), 1, "{:?}", seen.keys);
    assert_eq!(prefix[0].desc.as_deref(), Some("after the flood"));
    assert_eq!(keys(seen, "t", "<C-h>").len(), 1);
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn text_is_cut_to_two_hundred_characters_and_a_plain_config_still_reports_a_probe() {
    let rig = Rig::new("cut");
    rig.config(
        "init.lua",
        "vim.keymap.set('n', '<C-l>', 'x', { desc = string.rep('d', 300) })\n",
    );
    let reports = rig.run(&["doautocmd FocusLost", "sleep 400m"]);
    let seen = probe(reports.last().unwrap());
    let found = keys(seen, "n", "<C-l>");
    assert_eq!(found.len(), 1, "{:?}", seen.keys);
    assert_eq!(found[0].desc.as_deref().unwrap().chars().count(), 200);

    let bare = Rig::new("bare");
    let reports = bare.run(&["doautocmd FocusLost", "sleep 400m"]);
    let seen = probe(reports.last().unwrap());
    assert!(seen.plugins.is_empty(), "{:?}", seen.plugins);
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_desc_that_is_not_utf8_does_not_lose_the_report() {
    let rig = Rig::new("utf8");
    rig.config(
        "init.lua",
        "vim.keymap.set('n', '<C-k>', 'x', { desc = 'bad \\255 byte' })\n\
         vim.keymap.set('n', '<Leader>q', 'x', { desc = 'other \\255 byte' })\n",
    );
    let reports = rig.run(&["doautocmd FocusLost", "sleep 400m"]);
    let report = reports.last().expect("a report is sent");
    let found = keys(probe(report), "n", "<C-k>");
    assert_eq!(found.len(), 1, "{:?}", probe(report).keys);
    assert_eq!(found[0].desc.as_deref(), Some("bad \u{fffd} byte"));
    let leader = report.maps.iter().find(|m| m.lhs == "\\q").unwrap();
    assert_eq!(leader.desc.as_deref(), Some("other \u{fffd} byte"));
}

/// The reports as the host reads them back, for comparing two of them.
fn as_json(report: &NvimReport) -> String {
    serde_json::to_string(report).unwrap()
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_rescan_sends_a_report_that_has_not_changed_and_a_source_post_does_not() {
    let rig = Rig::new("rescan-same");
    rig.config("init.lua", "vim.keymap.set('n', '<C-l>', 'x', { desc = 'mine' })\n");
    let reports = rig.run(&[
        "doautocmd FocusLost",
        "sleep 400m",
        "doautocmd SourcePost",
        "sleep 400m",
        "lua __eitri_keys_rescan({})",
        "sleep 400m",
        "doautocmd SourcePost",
        "sleep 400m",
    ]);
    assert_eq!(
        reports.len(),
        2,
        "the first report and the rescan's; a SourcePost with nothing new sends none"
    );
    assert_eq!(as_json(&reports[0]), as_json(&reports[1]), "nothing changed in between");
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_rescan_made_while_a_debounce_waits_still_sends() {
    let rig = Rig::new("rescan-pending");
    let reports = rig.run(&[
        "doautocmd FocusLost",
        "sleep 400m",
        "doautocmd FocusLost",
        "lua __eitri_keys_rescan({})",
        "sleep 400m",
    ]);
    assert_eq!(
        reports.len(),
        2,
        "the second FocusLost alone would send nothing; the rescan inside its debounce makes it send"
    );
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn the_extra_chords_are_probed_in_that_report_only() {
    let rig = Rig::new("rescan-extra");
    rig.config(
        "init.lua",
        "vim.keymap.set('n', '<C-o>', 'x', { desc = 'jump' })\n\
         vim.keymap.set('n', '<M-t>', 'x', { desc = 'terminal' })\n",
    );
    let reports = rig.run(&[
        "doautocmd FocusLost",
        "sleep 400m",
        "lua __eitri_keys_rescan({'<C-o>', 'C-o', '<M-T>'})",
        "sleep 400m",
        "doautocmd SourcePost",
        "sleep 400m",
        "lua __eitri_keys_rescan({})",
        "sleep 400m",
    ]);
    assert_eq!(
        reports.len(),
        3,
        "ordinary, rescan with extras, rescan without; the SourcePost between sends none: {reports:?}"
    );
    assert!(keys(probe(&reports[0]), "n", "<C-o>").is_empty());
    let widened = probe(&reports[1]);
    let jump = keys(widened, "n", "<C-o>");
    assert_eq!(jump.len(), 1, "{:?}", widened.keys);
    assert_eq!(jump[0].desc.as_deref(), Some("jump"));
    assert_eq!(
        keys(widened, "n", "<M-t>").len(),
        1,
        "chords compare case-insensitively: {:?}",
        widened.keys
    );
    assert!(
        keys(probe(&reports[2]), "n", "<C-o>").is_empty() && keys(probe(&reports[2]), "n", "<M-t>").is_empty(),
        "the extras last one report: {:?}",
        probe(&reports[2]).keys
    );
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn an_extra_chord_not_in_angle_notation_probes_nothing() {
    let rig = Rig::new("rescan-bad");
    rig.config("init.lua", "vim.keymap.set('n', '<C-o>', 'x', { desc = 'jump' })\n");
    let reports = rig.run(&[
        "doautocmd FocusLost",
        "sleep 400m",
        "lua __eitri_keys_rescan({'C-o', '<>', 7, {}, '<C-o'})",
        "sleep 400m",
    ]);
    assert_eq!(reports.len(), 2);
    assert!(
        keys(probe(&reports[1]), "n", "<C-o>").is_empty(),
        "{:?}",
        probe(&reports[1]).keys
    );
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_rescan_reports_a_buffer_map_that_no_event_announced() {
    let rig = Rig::new("rescan-stale");
    let reports = rig.run(&[
        "doautocmd FocusLost",
        "sleep 400m",
        "lua vim.keymap.set('i', '<C-b>', function() end, { buffer = true, desc = 'Scroll documentation up' })",
        "sleep 400m",
        "lua __eitri_keys_rescan({})",
        "sleep 400m",
    ]);
    assert_eq!(reports.len(), 2, "making the map sends nothing by itself");
    assert!(keys(probe(&reports[0]), "i", "<C-b>").is_empty());
    let seen = keys(probe(&reports[1]), "i", "<C-b>");
    assert_eq!(seen.len(), 1, "{:?}", probe(&reports[1]).keys);
    assert_eq!(seen[0].scope, Scope::Buffer);
    assert_eq!(seen[0].desc.as_deref(), Some("Scroll documentation up"));
}
