--!nonstrict
-- Control Center tile: turns the airpipe service on and off; right-click
-- opens the panel. Renders the service's "snapshot".

local STATE_KEY = "snapshot"
local COMMAND_KEY = "command"
local PANEL_ID = "kushuh/airpipe:panel"

local snapshot = noctalia.state.get(STATE_KEY)

local function send(action)
    local prev = noctalia.state.get(COMMAND_KEY)
    local nonce = (type(prev) == "table" and tonumber(prev.nonce) or 0) + 1
    noctalia.state.set(COMMAND_KEY, { action = action, nonce = nonce })
end

local function active()
    return snapshot ~= nil and snapshot.unit ~= nil and snapshot.unit.active == true
end

local function render()
    shortcut.setLabel(noctalia.tr("shortcut.label"))
    shortcut.setIcon("cast", "cast-off")
    shortcut.setActive(active())
    shortcut.setEnabled(snapshot ~= nil and snapshot.busy == nil)
end

function onClick()
    send(active() and "stop" or "start")
end

function onRightClick()
    noctalia.togglePanel(PANEL_ID)
end

noctalia.state.watch(STATE_KEY, function(value)
    snapshot = value
    render()
end)

render()
