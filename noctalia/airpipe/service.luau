--!nonstrict
-- Headless engine, and the only entry that runs commands. It polls
-- `airpipe status --json` and the airpipe.service user unit, reads
-- `airpipe config --json` when settings may have changed, runs the actions
-- the panel, widget and shortcut ask for through the "command" state, and
-- publishes everything as the "snapshot" state they render.

local STATE_KEY = "snapshot"
local COMMAND_KEY = "command"
local UNIT = "airpipe.service"
-- The JSON interface this plugin understands (airpipe's src/api.rs).
local API_VERSION = 1
local POLL_TIMEOUT_MS = 5000
local ACTION_TIMEOUT_MS = 20000
-- `airpipe list` waits for the network and probes every speaker.
local SCAN_TIMEOUT_MS = 30000
local VOLUME_STEP = 5

local snap = {
    -- "unknown", "ok", "missing" (no airpipe binary), "too_old" (no JSON
    -- interface), "daemon_old" (the running airpipe predates it) or "error".
    cli = "unknown",
    cliError = nil,
    unit = { active = false, enabled = false },
    status = nil, -- airpipe status --json
    config = nil, -- airpipe config --json
    found = nil, -- airpipe list --json
    scanning = false,
    busy = nil, -- label of the action in progress
    error = nil, -- why the last action failed
    restartNeeded = false, -- config.toml changed while airpipe was running
}
local lastNonce = 0
local polling = false
local wasActive = nil

local function tr(key, args)
    return noctalia.tr(key, args)
end

local function trim(text)
    return noctalia.string.trim(tostring(text or ""))
end

local function binary()
    local configured = trim(noctalia.getConfig("binary"))
    if configured == "" then
        return "airpipe"
    end
    return noctalia.expandPath(configured)
end

local function pollInterval()
    local value = tonumber(noctalia.getConfig("poll_interval")) or 2
    return math.clamp(math.floor(value), 1, 30)
end

local function publish()
    snap.updatedAt = noctalia.nowMs()
    noctalia.state.set(STATE_KEY, snap)
end

local function firstLine(text)
    for line in tostring(text or ""):gmatch("[^\r\n]+") do
        local trimmed = trim(line)
        if trimmed ~= "" then
            return trimmed
        end
    end
    return ""
end

-- A reply of airpipe's JSON interface, or nil.
local function decode(stdout)
    local ok, value = pcall(function()
        return noctalia.json.decode(stdout or "")
    end)
    if ok and type(value) == "table" and tonumber(value.api) ~= nil then
        return value
    end
    return nil
end

local function run(argv, timeout, done)
    local started = noctalia.runAsync(argv, done, timeout)
    if not started then
        done({ exitCode = -1, stdout = "", stderr = tr("error.not_started"), timedOut = false })
    end
end

local function failure(result)
    if result.timedOut then
        return tr("error.timed_out")
    end
    local detail = firstLine(result.stderr)
    if detail == "" then
        detail = firstLine(result.stdout)
    end
    return detail
end

local function pollUnit(after)
    run({ "systemctl", "--user", "is-active", UNIT }, POLL_TIMEOUT_MS, function(active)
        run({ "systemctl", "--user", "is-enabled", UNIT }, POLL_TIMEOUT_MS, function(enabled)
            snap.unit = {
                active = trim(active.stdout) == "active",
                enabled = trim(enabled.stdout) == "enabled",
            }
            -- A restart, from here or elsewhere, applies pending settings.
            if wasActive == false and snap.unit.active then
                snap.restartNeeded = false
            end
            wasActive = snap.unit.active
            after()
        end)
    end)
end

local function pollStatus(after)
    if not noctalia.commandExists(binary()) then
        snap.cli, snap.status = "missing", nil
        after()
        return
    end
    run({ binary(), "status", "--json" }, POLL_TIMEOUT_MS, function(result)
        local doc = decode(result.stdout)
        local stderr = tostring(result.stderr or "")
        if doc ~= nil then
            snap.cli, snap.cliError, snap.status = "ok", nil, doc
        elseif stderr:find("unknown request", 1, true) then
            snap.cli, snap.status = "daemon_old", nil
        elseif stderr:find("--json", 1, true) then
            snap.cli, snap.status = "too_old", nil
        else
            snap.cli, snap.cliError, snap.status = "error", failure(result), nil
        end
        after()
    end)
end

local function poll()
    if polling then
        return
    end
    polling = true
    pollUnit(function()
        pollStatus(function()
            polling = false
            publish()
        end)
    end)
end

local function loadConfig()
    if not noctalia.commandExists(binary()) then
        return
    end
    run({ binary(), "config", "--json" }, POLL_TIMEOUT_MS, function(result)
        local doc = decode(result.stdout)
        if doc ~= nil and tonumber(doc.api) == API_VERSION then
            snap.config = doc
        end
        publish()
    end)
end

-- User actions run one at a time, in order: two airpipe commands editing
-- the same file must not race, and a quick series of clicks keeps its order.
local queue = {}
local acting = false

local function pump()
    if acting or #queue == 0 then
        return
    end
    local job = table.remove(queue, 1)
    acting = true
    snap.busy, snap.error = job.label, nil
    publish()
    run(job.argv, job.opts.timeout or ACTION_TIMEOUT_MS, function(result)
        acting = false
        snap.busy = nil
        if result.timedOut or result.exitCode ~= 0 then
            snap.error = tr("error.action", { action = job.label, detail = failure(result) })
        elseif job.opts.ok ~= nil then
            job.opts.ok()
        end
        if job.opts.reloadConfig then
            loadConfig()
        end
        poll()
        pump()
    end)
end

-- Queues one user action, then refreshes what it may have changed. A job
-- with a `coalesce` kind replaces a waiting one of the same kind (volume
-- steps from a scrolling wheel).
local function act(label, argv, opts)
    opts = opts or {}
    local job = { label = label, argv = argv, opts = opts }
    if opts.coalesce ~= nil then
        for i, waiting in ipairs(queue) do
            if waiting.opts.coalesce == opts.coalesce then
                queue[i] = job
                return
            end
        end
    end
    table.insert(queue, job)
    pump()
end

-- The running output's PipeWire node, else the configured one.
local function nodeName()
    local output = snap.status ~= nil and snap.status.output or nil
    if output ~= nil and output.node_name ~= nil then
        return output.node_name
    end
    local cfg = snap.config ~= nil and snap.config.config or nil
    return (cfg ~= nil and cfg.output ~= nil and cfg.output.node_name) or "airpipe"
end

local function setVolume(percent)
    percent = math.clamp(math.floor((tonumber(percent) or 0) + 0.5), 0, 100)
    act(tr("action.volume"), { "pactl", "set-sink-volume", nodeName(), `{percent}%` }, { coalesce = "volume" })
    return percent
end

-- config.toml settings apply when airpipe restarts.
local function markRestart()
    if snap.status ~= nil and snap.status.running then
        snap.restartNeeded = true
    end
end

local function scan()
    if snap.scanning then
        return
    end
    snap.scanning = true
    publish()
    run({ binary(), "list", "--json" }, SCAN_TIMEOUT_MS, function(result)
        snap.scanning = false
        local doc = decode(result.stdout)
        if doc ~= nil then
            snap.found = doc
        else
            snap.error = tr("error.action", { action = tr("action.scan"), detail = failure(result) })
        end
        publish()
    end)
end

local function systemctl(verb)
    return { "systemctl", "--user", verb, UNIT }
end

local handlers = {
    start = function()
        act(tr("action.start"), systemctl("start"))
    end,
    stop = function()
        act(tr("action.stop"), systemctl("stop"))
    end,
    restart = function()
        act(tr("action.restart"), systemctl("restart"), {
            ok = function()
                snap.restartNeeded = false
            end,
        })
    end,
    enable = function()
        act(tr("action.enable"), systemctl("enable"))
    end,
    disable = function()
        act(tr("action.disable"), systemctl("disable"))
    end,
    add = function(cmd)
        act(tr("action.speakers"), { binary(), "add", "--", tostring(cmd.speaker) }, { reloadConfig = true })
    end,
    remove = function(cmd)
        act(tr("action.speakers"), { binary(), "remove", "--", tostring(cmd.speaker) }, { reloadConfig = true })
    end,
    set = function(cmd)
        local argv = { binary(), "config", "set", tostring(cmd.key), tostring(cmd.value) }
        act(tr("action.setting"), argv, { reloadConfig = true, ok = markRestart })
    end,
    unset = function(cmd)
        act(tr("action.setting"), { binary(), "config", "unset", tostring(cmd.key) }, {
            reloadConfig = true,
            ok = markRestart,
        })
    end,
    set_speaker = function(cmd)
        -- Names come from the network: after `--`, never read as options.
        local argv =
            { binary(), "config", "set-speaker", "--", tostring(cmd.speaker), tostring(cmd.key), tostring(cmd.value) }
        act(tr("action.speaker_setting"), argv, { reloadConfig = true })
    end,
    volume = function(cmd)
        setVolume(cmd.value)
    end,
    mute = function()
        act(tr("action.volume"), { "pactl", "set-sink-mute", nodeName(), "toggle" })
    end,
    scan = scan,
    refresh = function()
        loadConfig()
        poll()
    end,
    dismiss = function()
        snap.error = nil
        publish()
    end,
}

noctalia.state.watch(COMMAND_KEY, function(value)
    if type(value) ~= "table" then
        return
    end
    local nonce = tonumber(value.nonce) or 0
    if nonce == lastNonce then
        return
    end
    lastNonce = nonce
    local handler = handlers[value.action]
    if handler ~= nil then
        handler(value)
    end
end)

-- `noctalia msg plugin kushuh/airpipe:service all <event>`; the bar widget
-- sends volume-up and volume-down on scroll.
function onIpc(event, _payload)
    if event == "volume-up" or event == "volume-down" then
        local output = snap.status ~= nil and snap.status.output or nil
        local current = output ~= nil and tonumber(output.volume) or nil
        if current == nil then
            return
        end
        local step = event == "volume-up" and VOLUME_STEP or -VOLUME_STEP
        local percent = setVolume(current + step)
        noctalia.osd.show(percent == 0 and "volume-off" or "volume", `{percent}%`, { progress = percent / 100 })
    elseif event == "toggle" then
        handlers[snap.unit.active and "stop" or "start"]()
    elseif event == "refresh" then
        handlers.refresh()
    end
end

function update()
    poll()
    noctalia.setUpdateInterval(pollInterval() * 1000)
end

function onConfigChanged()
    noctalia.setUpdateInterval(pollInterval() * 1000)
    handlers.refresh()
end

local pending = noctalia.state.get(COMMAND_KEY)
lastNonce = type(pending) == "table" and (tonumber(pending.nonce) or 0) or 0
noctalia.setUpdateInterval(pollInterval() * 1000)
loadConfig()
poll()
