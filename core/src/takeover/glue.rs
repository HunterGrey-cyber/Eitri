use std::path::PathBuf;
use std::time::Duration;

use crate::nvim_rpc::{NvimLink, Pending};

use super::room::OriginLink;
use crate::companion::InstallError;

pub struct TakeoverParts {
    pub sockets: crate::companion::Sockets,
    pub commands: CommandsPart,
    pub tmux_swap: Option<TmuxSwap>,
}

pub struct CommandsPart {
    pub socket: PathBuf,
    pub words: crate::nvim_verbs::CommandWords,
    pub group: Option<(String, Vec<crate::nvim_verbs::leader::LeaderKey>)>,
}

pub struct TmuxSwap {
    pub tmux: String,
    pub tmux_pane: String,
    pub pane_switch_socket: PathBuf,
    pub shim_dir: PathBuf,
}

pub struct GlueHandle {
    pub link_chan: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GlueNotice {
    UiEntered,
    Replaced,
}

/// Installs the takeover glue over the origin connection, within 2 s; the install report's process id is dropped.
pub fn takeover_install(link: &OriginLink, parts: &TakeoverParts) -> Result<GlueHandle, InstallError> {
    takeover_install_link(&link.link, parts)
}

/// The install over a borrowed RPC connection, for a test that holds only the connection.
#[doc(hidden)]
pub fn takeover_install_link(link: &NvimLink, parts: &TakeoverParts) -> Result<GlueHandle, InstallError> {
    let channel = link.channel_id();
    let pending = link.exec_lua(
        crate::companion::INSTALL_LUA,
        crate::companion::takeover_install_args(channel, parts),
    );
    let result = install_answer(channel, pending);
    if result.is_err() && !matches!(result, Err(InstallError::Refused(_))) {
        // Closing our own channel also takes out an install that nvim answers after the deadline.
        link.close();
    }
    result
}

fn install_answer(channel: u64, pending: Pending) -> Result<GlueHandle, InstallError> {
    let answer = pending
        .wait(Duration::from_secs(2))
        .ok_or_else(|| InstallError::Malformed("nvim did not answer the takeover install within 2 s".into()))?
        .map_err(|error| InstallError::Malformed(error.to_string()))?;
    let report = crate::companion::parse_install_report(&answer)?;
    if !report.failed.is_empty() {
        return Err(InstallError::Malformed(format!(
            "the takeover glue could not install: {}",
            report
                .failed
                .iter()
                .map(|(name, why)| format!("{name}: {why}"))
                .collect::<Vec<_>>()
                .join("; ")
        )));
    }
    Ok(GlueHandle { link_chan: channel })
}

/// The outline is hidden only while this part still owns the value.
pub(crate) const CURSOR_PART_LUA: &str = r#"
local opts = ...
local torn = false
local previous = vim.g.neovide_cursor_unfocused_outline_width
vim.g.neovide_cursor_unfocused_outline_width = 0
return function()
  if torn then return end
  torn = true
  if vim.g.neovide_cursor_unfocused_outline_width == 0 then
    vim.g.neovide_cursor_unfocused_outline_width = previous
  end
end
"#;

/// Preserve nvim's own environment, including PATH entries added by plugins while attached.
pub(crate) const ENV_PART_LUA: &str = r#"
local opts = ...
local torn = false
if not vim.env.TMUX or vim.env.TMUX == '' then
  return function() torn = true end
end
local previous, swapped = {}, { TMUX = opts.tmux, TMUX_PANE = opts.tmux_pane }
swapped['EITRI_PANE_SWITCH_SOCKET'] = opts.socket
for name, value in pairs(swapped) do
  previous[name] = vim.env[name]
  vim.env[name] = value
end
local old_path = vim.env.PATH
local prefix = opts.shim_dir .. ':'
vim.env.PATH = prefix .. (old_path or '')
return function()
  if torn then return end
  torn = true
  for name, value in pairs(swapped) do
    if vim.env[name] == value then vim.env[name] = previous[name] end
  end
  local path = vim.env.PATH
  if path and path:sub(1, #prefix) == prefix then
    local rest = path:sub(#prefix + 1)
    if rest == '' and old_path == nil then vim.env.PATH = nil else vim.env.PATH = rest end
  elseif path then
    -- A new entry can precede the prefix we inserted. Remove it only if the entire
    -- suffix still matches that original PATH; a shim moved elsewhere is the user's.
    local suffix = ':' .. prefix .. (old_path or '')
    if #path > #suffix and path:sub(-#suffix) == suffix then
      vim.env.PATH = path:sub(1, #path - #suffix) .. (old_path and ':' .. old_path or '')
    end
  end
end
"#;

/// The fork publishes its channel before ui_attach, after this part has installed its hook.
pub(crate) const UI_PART_LUA: &str = r#"
local opts = ...
local torn = false
local group = vim.api.nvim_create_augroup('EitriTakeover', {clear=true})
vim.api.nvim_create_autocmd('UIEnter', {group=group, callback=function()
  if torn then return end
  local fork = rawget(_G, '__neovide_attach')
  if not fork or vim.v.event.chan ~= fork.chan then
    pcall(vim.rpcnotify, opts.chan, 'eitri_ui_entered')
  end
end})
return function()
  if torn then return end
  torn = true
  pcall(vim.api.nvim_del_augroup_by_id, group)
end
"#;

/// The installer runs teardowns backwards, so this last part cleans up the fork first.
pub(crate) const FORK_PART_LUA: &str = r#"
local opts = ...
local torn = false
return function()
  if torn then return end
  torn = true
  local fork = rawget(_G, '__neovide_attach')
  if type(fork) == 'table' and type(fork.cleanup) == 'function' then
    pcall(fork.cleanup, fork.chan)
  end
end
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::companion::InstallRefusal;
    use rmpv::Value;

    fn pending(value: Value) -> Pending {
        let (answer, pending) = Pending::pair();
        answer.send(Ok(value));
        pending
    }

    #[test]
    fn an_install_keeps_only_the_channel_and_refusals_remain_refusals() {
        let report = Value::Map(vec![
            ("version".into(), 2.into()),
            ("slot".into(), "agent".into()),
            ("installed".into(), Value::Array(vec![])),
            ("failed".into(), Value::Array(vec![])),
            ("pid".into(), 987654.into()),
            ("tmux".into(), false.into()),
        ]);
        assert_eq!(install_answer(42, pending(report)).unwrap().link_chan, 42);
        let refused = Value::Map(vec![("refused".into(), "takeover".into())]);
        assert!(matches!(
            install_answer(42, pending(refused)),
            Err(InstallError::Refused(InstallRefusal::Takeover))
        ));
    }

    #[test]
    fn a_partial_install_is_a_failure() {
        let report = Value::Map(vec![
            ("version".into(), 2.into()),
            ("slot".into(), "agent".into()),
            ("installed".into(), Value::Array(vec![])),
            (
                "failed".into(),
                Value::Array(vec![Value::Array(vec!["takeover_ui".into(), "no hook".into()])]),
            ),
            ("pid".into(), 1.into()),
            ("tmux".into(), false.into()),
        ]);
        assert!(matches!(
            install_answer(42, pending(report)),
            Err(InstallError::Malformed(_))
        ));
    }

    #[test]
    fn an_unanswered_install_ends_after_its_request_budget() {
        let (_answer, pending) = Pending::pair();
        assert!(matches!(install_answer(42, pending), Err(InstallError::Malformed(_))));
    }
}
