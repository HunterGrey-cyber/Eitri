local buf, config = ...
_G.__fake_lsp = _G.__fake_lsp or { seen = {}, answers = {}, hang = {}, delay = 0 }
local capabilities = config.capabilities or {
  definitionProvider = true, referencesProvider = true, implementationProvider = true,
  hoverProvider = true, documentSymbolProvider = true, workspaceSymbolProvider = true,
}
capabilities.positionEncoding = config.encoding or 'utf-16'
return vim.lsp.start({
  name = config.name or 'fake', root_dir = config.root,
  cmd = function(dispatchers)
    local closed, serial = false, 0
    return {
      request = function(method, params, cb)
        serial = serial + 1
        local fake = _G.__fake_lsp
        fake.seen[#fake.seen + 1] = { method = method, params = params, client = config.name or 'fake' }
        if method == 'initialize' then
          vim.defer_fn(function() cb(nil, { capabilities = capabilities }) end, config.initialize_delay or 0)
        elseif method == 'shutdown' then
          cb(nil, nil)
        else
          if not fake.hang[method] then
            local answer = vim.deepcopy(fake.answers[method])
            vim.defer_fn(function() cb(nil, answer, { method = method }) end, fake.delay or 0)
          end
        end
        return true, serial
      end,
      notify = function(method, params)
        if method == '$/cancelRequest' then
          local fake = _G.__fake_lsp
          fake.cancelled = fake.cancelled or {}
          fake.cancelled[#fake.cancelled + 1] = params.id
        end
        return true
      end,
      is_closing = function() return closed end,
      terminate = function() closed = true dispatchers.on_exit(0, 0) end,
    }
  end,
}, { bufnr = buf, reuse_client = function() return false end })
