//! Local plugins: a page served from the plugin's own directory and given the closed set of host
//! capabilities its manifest declares, plus the processes, files and editor calls those capabilities
//! reach. Nothing here touches a toolkit; `shell` builds the page and wires the window to the host.

pub mod editor;
pub mod host;
pub mod keys;
pub mod kit_settings;
pub mod load;
pub mod manifest;
pub mod page_policy;
pub mod protocol;
pub mod roots;
pub mod state;
pub mod tasks;
pub mod theme;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
