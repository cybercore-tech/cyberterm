-- The `cyberterm` table scripts use (src/app/lua.rs loads this before
-- init.lua). Rust provides __call (any control-socket method, only while
-- a script runs), __now, __log and __notify; everything else is here.

local ct = {
  _handlers = {},
  _bindings = {},
  _commands = {},
  _timers = {},
}

local EVENTS = {
  startup = true,
  pane_created = true,
  focus_changed = true,
  command_started = true,
  command_finished = true,
}

--- Calls a control-socket method (see `cyberterm +ctl help`) and returns
--- its result as a table. Errors are raised as Lua errors.
function ct.call(method, params)
  return __call(method, params or {})
end

--- cyberterm.on("command_finished", function(ev) ... end)
function ct.on(event, fn)
  assert(EVENTS[event], "unknown event '" .. tostring(event) .. "'")
  assert(type(fn) == "function", "on() needs a function")
  ct._handlers[event] = ct._handlers[event] or {}
  table.insert(ct._handlers[event], fn)
end

--- cyberterm.bind("ctrl+alt+g", function() ... end) -- runs before the
--- built-in keybindings.
function ct.bind(combo, fn)
  assert(type(combo) == "string" and type(fn) == "function", "bind(combo, fn)")
  table.insert(ct._bindings, { combo = combo, fn = fn })
end

--- cyberterm.command("deploy", function(args) ... end) -- run with
--- `cyberterm +ctl lua name=deploy args='["prod"]'`.
function ct.command(name, fn)
  assert(type(name) == "string" and type(fn) == "function", "command(name, fn)")
  ct._commands[name] = fn
end

--- Runs fn once after ms milliseconds.
function ct.after(ms, fn)
  assert(type(fn) == "function", "after(ms, fn)")
  table.insert(ct._timers, { at = __now() + (tonumber(ms) or 0), fn = fn })
end

ct.log = function(...)
  local parts = {}
  for i = 1, select("#", ...) do
    parts[#parts + 1] = tostring(select(i, ...))
  end
  __log(table.concat(parts, " "))
end
ct.notify = function(title, body) __notify(tostring(title), tostring(body or "")) end

-- Friendly wrappers over call().
function ct.panes() return ct.call("list_panes") end
function ct.tabs() return ct.call("list_tabs") end
function ct.focused()
  for _, p in ipairs(ct.panes()) do
    if p.focused then return p end
  end
end
function ct.send_text(pane, text) return ct.call("send_text", { pane = pane, text = text }) end
function ct.paste(pane, text) return ct.call("send_text", { pane = pane, text = text, paste = true }) end
function ct.get_text(pane, lines) return ct.call("get_text", { pane = pane, lines = lines }).text end
--- split{ direction = "right"|"down"|"left"|"up", command = "...", cwd = "..." }
function ct.split(opts) return ct.call("split", opts).pane end
--- new_tab{ title = "...", command = "...", cwd = "..." }
function ct.new_tab(opts) return ct.call("new_tab", opts) end
function ct.focus(pane) return ct.call("focus", { pane = pane }) end
function ct.close(pane) return ct.call("close", { pane = pane }) end
function ct.set_title(title, tab) return ct.call("set_title", { title = title, tab = tab }) end
function ct.history(query, opts)
  opts = opts or {}
  opts.query = query
  return ct.call("history", opts)
end
function ct.blocks(pane, limit, output)
  return ct.call("blocks", { pane = pane, limit = limit, output = output })
end
function ct.copy(text) return ct.call("lua_copy", { text = text }) end
--- A short message along the bottom of the focused pane.
function ct.status(text) return ct.call("lua_status", { text = tostring(text) }) end

-- Called from Rust ------------------------------------------------------

function ct._emit(event, ev)
  local list = ct._handlers[event]
  if not list then return end
  if event == "command_finished" then
    -- The output on demand (it can be long).
    ev.output = function()
      local b = ct.blocks(ev.pane, 1, true)[1]
      return b and b.output or ""
    end
  end
  for _, fn in ipairs(list) do
    fn(ev)
  end
end

function ct._run_binding(i) ct._bindings[i].fn() end

function ct._run_command(name, args)
  local fn = ct._commands[name]
  if not fn then error("no Lua command '" .. tostring(name) .. "'") end
  return fn(args)
end

function ct._next_timer()
  local next
  for _, t in ipairs(ct._timers) do
    if not next or t.at < next then next = t.at end
  end
  return next
end

function ct._fire_timers(now)
  local due, keep = {}, {}
  for _, t in ipairs(ct._timers) do
    if t.at <= now then due[#due + 1] = t else keep[#keep + 1] = t end
  end
  ct._timers = keep
  for _, t in ipairs(due) do t.fn() end
end

cyberterm = ct
