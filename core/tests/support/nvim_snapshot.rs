use rmpv::Value;

#[derive(Debug, PartialEq)]
pub struct Snapshot(Value);

const SNAPSHOT: &str = r#"
local function keys(t)
  local out = {}
  for k in pairs(t) do out[#out + 1] = k end
  table.sort(out)
  return out
end
local function plain(x)
  if type(x) == 'function' then return tostring(x) end
  if type(x) ~= 'table' then return x end
  local out = {}
  for k, v in pairs(x) do out[k] = plain(v) end
  return out
end
local options = { global = {}, window = {}, buffer = {} }
for name in pairs(vim.api.nvim_get_all_options_info()) do
  for scope, opts in pairs({ global = { scope = 'global' }, window = { win = 0 }, buffer = { buf = 0 } }) do
    local ok, value = pcall(vim.api.nvim_get_option_value, name, opts)
    if ok then options[scope][name] = value end
  end
end
local function keymaps(items)
  -- Replacing a mapping can change hash-bucket order without changing any mapping.
  table.sort(items, function(a, b) return a.lhs < b.lhs end)
  return plain(items)
end
local maps = {}
for _, mode in ipairs({'n', 'v', 'x', 's', 'o', 'i', 'l', 'c', 't'}) do
  maps[mode] = { global = keymaps(vim.api.nvim_get_keymap(mode)), buffer = keymaps(vim.api.nvim_buf_get_keymap(0, mode)) }
end
local autocmds = {}
for _, a in ipairs(vim.api.nvim_get_autocmds({})) do
  autocmds[a.event] = autocmds[a.event] or {}
  table.insert(autocmds[a.event], a)
end
local buffers = {}
for _, b in ipairs(vim.api.nvim_list_bufs()) do
  buffers[#buffers + 1] = { id = b, name = vim.api.nvim_buf_get_name(b), loaded = vim.api.nvim_buf_is_loaded(b) }
end
-- Completion also lists END and deleted-group placeholders, which name no live group.
local groups = {}
for _, group in ipairs(vim.fn.getcompletion('', 'augroup')) do
  if pcall(vim.api.nvim_get_autocmds, {group = group}) then groups[#groups + 1] = group end
end
-- vim.uv.walk crashes an embedded nvim 0.10 (SIGSEGV on 0.10.4), so handles are counted from 0.11 on.
local handles = vim.NIL
if vim.fn.has('nvim-0.11') == 1 then
  handles = 0
  vim.uv.walk(function(h) if not h:is_closing() then handles = handles + 1 end end)
end
return plain({
  globals = vim.fn.eval('g:'), commands = plain(vim.api.nvim_get_commands({ builtin = false })),
  augroups = groups, autocmds = plain(autocmds), maps = maps, options = options,
  highlights = vim.api.nvim_get_hl(0, {}), functions = vim.fn.execute('function'),
  env = { TMUX = vim.env.TMUX or vim.NIL, TMUX_PANE = vim.env.TMUX_PANE or vim.NIL, PATH = vim.env.PATH or vim.NIL },
  global_keys = keys(_G), handles = handles, buffers = buffers, namespaces = vim.api.nvim_get_namespaces(),
  modules = keys(package.loaded),
})
"#;

pub fn snapshot(exec_lua: &dyn Fn(&str) -> Value) -> Snapshot {
    // Drain queued callbacks and handle closes before counting them. This also loads the
    // scheduler's own API helpers before either module-list observation.
    exec_lua("local done = false; vim.schedule_wrap(function() done = true end)(); assert(vim.wait(1000, function() return done end, 10))");
    exec_lua(SNAPSHOT);
    let value = exec_lua(SNAPSHOT);
    Snapshot(canonical(value))
}

fn canonical(value: Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.into_iter().map(canonical).collect()),
        Value::Map(items) => {
            let mut items: Vec<_> = items.into_iter().map(|(k, v)| (k, canonical(v))).collect();
            items.sort_by_key(|(k, _)| format!("{k:?}"));
            Value::Map(items)
        }
        other => other,
    }
}

pub fn assert_same(before: &Snapshot, after: &Snapshot) {
    assert_value_same(&before.0, &after.0, "nvim");
}

fn assert_value_same(before: &Value, after: &Value, path: &str) {
    match (before, after) {
        (Value::Map(a), Value::Map(b)) => {
            assert_eq!(
                a.iter().map(|(k, _)| k).collect::<Vec<_>>(),
                b.iter().map(|(k, _)| k).collect::<Vec<_>>(),
                "{path} keys"
            );
            for ((key, a), (_, b)) in a.iter().zip(b) {
                assert_value_same(a, b, &format!("{path}.{}", key.as_str().unwrap_or("?")));
            }
        }
        (Value::Array(a), Value::Array(b)) => {
            assert_eq!(a.len(), b.len(), "{path} length");
            for (index, (a, b)) in a.iter().zip(b).enumerate() {
                assert_value_same(a, b, &format!("{path}[{index}]"));
            }
        }
        _ => assert_eq!(before, after, "{path}"),
    }
}
