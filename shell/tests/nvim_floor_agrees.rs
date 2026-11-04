//! The editor pane accepts exactly the nvims the window starts: `neovide_editor::NVIM_FLOOR`, which the
//! pane hands the fork, is the window's own `eitri_core::nvim_bin::MINIMUM_VERSION`.

#[test]
fn the_editor_floor_is_the_windows_floor() {
    assert_eq!(neovide_editor::NVIM_FLOOR, eitri_core::nvim_bin::MINIMUM_VERSION);
}
