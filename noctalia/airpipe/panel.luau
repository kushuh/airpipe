--!nonstrict
-- The AirPlay panel: turn airpipe on and off, set the volume, choose the
-- speakers, and change every setting of config.toml and devices.toml.
-- Renders the service's "snapshot"; every change is sent to the service as
-- a "command" (entries share no Lua memory).

local STATE_KEY = "snapshot"
local COMMAND_KEY = "command"
local LATENCIES = { "low-latency", "balanced", "robust" }
local START_VOLUMES = { "device-default", "last" }
-- What "keep connected" off goes back to (airpipe's default).
local DEFAULT_IDLE_SECS = 30

local snapshot = noctalia.state.get(STATE_KEY)
local expanded = { speakers = {}, add = false, settings = false }
-- Slider positions while dragging, by slider; sent on release.
local drafts = {}
-- Text typed in a field and not saved yet, by field.
local typed = {}

local function tr(key, args)
    return noctalia.tr(key, args)
end

local function trim(text)
    return noctalia.string.trim(tostring(text or ""))
end

local function send(action, args)
    local prev = noctalia.state.get(COMMAND_KEY)
    local cmd = args or {}
    cmd.action = action
    cmd.nonce = (type(prev) == "table" and tonumber(prev.nonce) or 0) + 1
    noctalia.state.set(COMMAND_KEY, cmd)
end

local function round(value)
    return math.floor((tonumber(value) or 0) + 0.5)
end

local function indexOf(list, value)
    for i, item in ipairs(list) do
        if item == value then
            return i - 1 -- ui.select is 0-based
        end
    end
    return 0
end

local function sameName(a, b)
    return string.lower(tostring(a)) == string.lower(tostring(b))
end

local function muted(text, key)
    return ui.label({ key = key, text = text, color = "on_surface_variant", maxLines = 4 })
end

local function card(key, children)
    return ui.column({ key = key, gap = 10, padding = 12, radius = 12, fill = "surface_container/0.4" }, children)
end

local function sectionHeader(key, title, open)
    return ui.row({ key = key, align = "center", gap = 8, onClick = function()
        expanded[key] = not open
        render()
    end }, {
        ui.label({ text = title, fontWeight = "bold", flexGrow = 1 }),
        ui.glyph({ name = open and "chevron-up" or "chevron-down", size = 16, color = "on_surface_variant" }),
    })
end

local function settingRow(key, label, control)
    return ui.row({ key = key, align = "center", gap = 12 }, {
        ui.label({ text = label, flexGrow = 1, maxLines = 2 }),
        control,
    })
end

-- A text field with a save button next to it; Enter saves too. Typing
-- alone changes nothing: the save button lights up until the text is saved.
local function textField(key, saved, width, placeholder, save)
    saved = tostring(saved or "")
    local function unsaved(text)
        return text ~= nil and trim(text) ~= trim(saved)
    end
    local function commit(text)
        typed[key] = nil
        if unsaved(text) then
            save(trim(text))
        end
        render()
    end
    local dirty = unsaved(typed[key])
    return ui.row({ gap = 4, align = "center" }, {
        ui.input({
            key = key,
            value = saved,
            width = width,
            placeholder = placeholder,
            onChange = function(text)
                local was = unsaved(typed[key])
                typed[key] = text
                -- Re-render only when the save button changes, not per key.
                if unsaved(text) ~= was then
                    render()
                end
            end,
            onSubmit = commit,
        }),
        ui.button({
            glyph = "device-floppy",
            variant = dirty and "primary" or "ghost",
            enabled = dirty,
            tooltip = tr("panel.save"),
            onClick = function()
                commit(typed[key])
            end,
        }),
    })
end

-- A 0-100 slider that sends its value when released.
local function percentSlider(key, label, value, commit)
    local shown = drafts[key] or round(value)
    return ui.column({ key = key, gap = 4 }, {
        ui.row({ justify = "space_between" }, {
            ui.label({ text = label }),
            ui.label({ text = tr("unit.percent", { value = shown }), color = "on_surface_variant" }),
        }),
        ui.slider({
            min = 0,
            max = 100,
            step = 1,
            value = shown,
            onChange = function(text)
                -- The host also reports the value it was just given: only a
                -- real move re-renders, or each render would trigger the next.
                local moved = round(text)
                if moved ~= shown then
                    drafts[key] = moved
                    render()
                end
            end,
            onDragEnd = function()
                local final = drafts[key]
                drafts[key] = nil
                if final ~= nil and final ~= round(value) then
                    commit(final)
                end
            end,
        }),
    })
end

-- Live state of a configured speaker, by name.
local function liveSpeaker(name)
    local status = snapshot.status
    for _, speaker in ipairs(status ~= nil and status.speakers or {}) do
        if sameName(speaker.name, name) then
            return speaker
        end
    end
    return nil
end

-- Its devices.toml entry, once airpipe has used it.
local function knownSpeaker(name)
    local config = snapshot.config
    for _, speaker in ipairs(config ~= nil and config.devices or {}) do
        if sameName(speaker.name, name) then
            return speaker
        end
    end
    return nil
end

local function settings()
    local config = snapshot.config
    return config ~= nil and config.config or nil
end

local function running()
    return snapshot.status ~= nil and snapshot.status.running == true
end

local function statusLine()
    if snapshot.busy ~= nil then
        return tr("state.busy", { action = snapshot.busy })
    end
    if snapshot.cli ~= "ok" then
        return tr(`state.{snapshot.cli}`)
    end
    if not running() then
        return tr("state.off")
    end
    local playing = {}
    local problem = false
    for _, speaker in ipairs(snapshot.status.speakers or {}) do
        if speaker.state == "streaming" then
            table.insert(playing, speaker.name)
        elseif speaker.state == "retrying" then
            problem = true
        end
    end
    if #playing > 0 then
        return tr("state.playing", { speakers = table.concat(playing, ", ") })
    end
    return problem and tr("state.problem") or tr("state.ready")
end

local function header()
    local active = snapshot.unit ~= nil and snapshot.unit.active == true
    return ui.row({ key = "header", align = "center", gap = 10 }, {
        ui.glyph({ name = "cast", size = 22, color = running() and "primary" or "on_surface_variant" }),
        ui.column({ gap = 2, flexGrow = 1 }, {
            ui.label({ text = tr("panel.title"), fontSize = 16, fontWeight = "bold" }),
            muted(statusLine()),
        }),
        ui.button({ glyph = "refresh", variant = "ghost", tooltip = tr("panel.refresh"), onClick = function()
            send("refresh")
        end }),
        ui.toggle({ checked = active, onChange = function(value)
            send(value == "true" and "start" or "stop")
        end }),
    })
end

local function banners()
    local items = {}
    if snapshot.error ~= nil then
        table.insert(items, ui.row({ key = "error", align = "center", gap = 8 }, {
            ui.glyph({ name = "alert-circle", size = 18, color = "error" }),
            ui.label({ text = snapshot.error, color = "error", flexGrow = 1, maxLines = 4 }),
            ui.button({ glyph = "x", variant = "ghost", onClick = function()
                send("dismiss")
            end }),
        }))
    end
    if snapshot.restartNeeded then
        table.insert(items, card("restart", {
            ui.label({ text = tr("banner.restart"), maxLines = 3 }),
            ui.button({ text = tr("banner.restart_now"), variant = "primary", onClick = function()
                send("restart")
            end }),
        }))
    end
    if snapshot.cli ~= "ok" and snapshot.cli ~= "unknown" then
        local children = { ui.label({ text = tr(`cli.{snapshot.cli}`), maxLines = 6 }) }
        if snapshot.cli == "error" and snapshot.cliError ~= nil then
            table.insert(children, muted(snapshot.cliError))
        elseif snapshot.cli == "daemon_old" then
            table.insert(children, ui.button({ text = tr("banner.restart_now"), variant = "primary", onClick = function()
                send("restart")
            end }))
        end
        table.insert(items, card("cli", children))
    end
    return items
end

local function volumeSection()
    if not running() then
        return nil
    end
    local output = snapshot.status.output or {}
    if not output.follow_system then
        return muted(tr("volume.fixed"), "volume")
    end
    if output.volume == nil then
        return muted(tr("volume.unknown"), "volume")
    end
    local shown = drafts.volume or round(output.volume)
    return ui.row({ key = "volume", align = "center", gap = 8 }, {
        ui.button({
            glyph = output.muted and "volume-off" or "volume",
            variant = "ghost",
            tooltip = tr("volume.mute"),
            onClick = function()
                send("mute")
            end,
        }),
        ui.slider({
            min = 0,
            max = 100,
            step = 1,
            value = shown,
            flexGrow = 1,
            onChange = function(text)
                local moved = round(text) -- see percentSlider
                if moved ~= shown then
                    drafts.volume = moved
                    render()
                end
            end,
            onDragEnd = function()
                local final = drafts.volume
                drafts.volume = nil
                if final ~= nil then
                    send("volume", { value = final })
                end
            end,
        }),
        ui.label({ text = tr("unit.percent", { value = shown }), color = "on_surface_variant", width = 40 }),
    })
end

local function speakerState(name)
    if not running() then
        return tr("speaker.off"), "on_surface_variant"
    end
    local live = liveSpeaker(name)
    if live == nil then
        return tr("speaker.pending"), "on_surface_variant"
    end
    if live.state == "streaming" then
        local volume = live.volume ~= nil and tr("unit.percent", { value = round(live.volume) }) or tr("speaker.muted")
        return tr("speaker.streaming", { volume = volume }), "primary"
    elseif live.state == "retrying" then
        return tr("speaker.retrying", { seconds = live.retry_in_secs or 0 }), "error"
    end
    return tr(`speaker.{live.state}`), "on_surface"
end

local function speakerSettings(name)
    local known = knownSpeaker(name)
    if known == nil then
        return { muted(tr("speaker.not_used_yet")) }
    end
    local function commit(key)
        return function(value)
            send("set_speaker", { speaker = name, key = key, value = value })
        end
    end
    return {
        percentSlider(`default-{name}`, tr("speaker.default_volume"), known.default_volume, commit("default_volume")),
        percentSlider(`max-{name}`, tr("speaker.max_volume"), known.max_volume, commit("max_volume")),
        settingRow(`offset-row-{name}`, tr("speaker.latency_offset"), textField(`offset-{name}`, known.latency_offset_ms or 0, 90, nil, function(value)
            send("set_speaker", { speaker = name, key = "latency_offset_ms", value = value })
        end)),
        muted(tr("speaker.latency_offset_hint")),
    }
end

local function speakerCard(name)
    local stateText, stateColor = speakerState(name)
    local open = expanded.speakers[name] == true
    local live = liveSpeaker(name)
    local children = {
        ui.row({ align = "center", gap = 10 }, {
            ui.glyph({ name = "device-speaker", size = 20, color = stateColor }),
            ui.column({ gap = 2, flexGrow = 1 }, {
                ui.label({ text = name, fontWeight = "bold", maxLines = 1 }),
                ui.label({ text = stateText, color = stateColor == "error" and "error" or "on_surface_variant" }),
            }),
            ui.button({ glyph = "adjustments", variant = open and "secondary" or "ghost", tooltip = tr("speaker.settings"), onClick = function()
                expanded.speakers[name] = not open
                render()
            end }),
            ui.button({ glyph = "x", variant = "ghost", tooltip = tr("speaker.remove"), onClick = function()
                send("remove", { speaker = name })
            end }),
        }),
    }
    if live ~= nil and live.last_error ~= nil then
        table.insert(children, ui.label({ text = live.last_error, color = "error", maxLines = 4 }))
    end
    if open then
        for _, item in ipairs(speakerSettings(name)) do
            table.insert(children, item)
        end
    end
    return card(`speaker-{name}`, children)
end

local function speakersSection()
    local cfg = settings()
    local names = cfg ~= nil and cfg.output ~= nil and cfg.output.speakers or {}
    local items = { ui.label({ key = "speakers-title", text = tr("speakers.title"), fontWeight = "bold" }) }
    if #names == 0 then
        table.insert(items, muted(tr("speakers.none"), "speakers-none"))
    end
    for _, name in ipairs(names) do
        table.insert(items, speakerCard(name))
    end
    return items
end

local function foundRow(speaker)
    local usable = speaker.airplay2 and speaker.problem == nil and (not speaker.needs_pairing or speaker.paired)
    local notes = { speaker.model }
    if not speaker.airplay2 then
        table.insert(notes, tr("found.airplay1"))
    elseif speaker.needs_pairing and not speaker.paired then
        table.insert(notes, tr("found.needs_pairing", { name = speaker.name }))
    elseif speaker.needs_password then
        table.insert(notes, tr("found.needs_password"))
    end
    local column = {
        ui.label({ text = speaker.name, fontWeight = "bold", maxLines = 1 }),
        muted(table.concat(notes, " · ")),
    }
    if speaker.problem ~= nil then
        table.insert(column, ui.label({ text = speaker.problem, color = "error", maxLines = 3 }))
    end
    return ui.row({ key = `found-{speaker.name}`, align = "center", gap = 10 }, {
        ui.column({ gap = 2, flexGrow = 1 }, column),
        ui.button({ text = tr("found.add"), glyph = "plus", variant = "outline", enabled = usable, onClick = function()
            send("add", { speaker = speaker.name })
        end }),
    })
end

local function addSection()
    local items = { sectionHeader("add", tr("found.title"), expanded.add) }
    if not expanded.add then
        return items
    end
    table.insert(items, ui.button({
        key = "scan",
        text = snapshot.scanning and tr("found.scanning") or tr("found.scan"),
        glyph = "radar",
        variant = "outline",
        enabled = not snapshot.scanning,
        onClick = function()
            send("scan")
        end,
    }))
    local found = snapshot.found
    if found == nil then
        return items
    end
    local cfg = settings()
    local inUse = cfg ~= nil and cfg.output ~= nil and cfg.output.speakers or {}
    local shown = 0
    for _, speaker in ipairs(found.speakers or {}) do
        local used = false
        for _, name in ipairs(inUse) do
            used = used or sameName(name, speaker.name)
        end
        if not used then
            table.insert(items, foundRow(speaker))
            shown += 1
        end
    end
    for _, name in ipairs(found.unresponsive or {}) do
        table.insert(items, muted(tr("found.unresponsive", { name = name }), `unresponsive-{name}`))
        shown += 1
    end
    if shown == 0 then
        table.insert(items, muted(tr("found.none"), "found-none"))
    end
    return items
end

local function setTo(key)
    return function(value)
        send("set", { key = key, value = value })
    end
end

local function settingsSection()
    local items = { sectionHeader("settings", tr("settings.title"), expanded.settings) }
    local cfg = settings()
    if not expanded.settings or cfg == nil then
        return items
    end
    local unit = snapshot.unit or {}
    local idle = tonumber(cfg.audio.idle_timeout_secs) or DEFAULT_IDLE_SECS
    local latencyLabels = {}
    for _, value in ipairs(LATENCIES) do
        table.insert(latencyLabels, tr(`latency.{value}`))
    end
    local startLabels = {}
    for _, value in ipairs(START_VOLUMES) do
        table.insert(startLabels, tr(`start.{value}`))
    end

    table.insert(items, settingRow("autostart", tr("settings.autostart"), ui.toggle({
        checked = unit.enabled == true,
        onChange = function(value)
            send(value == "true" and "enable" or "disable")
        end,
    })))
    table.insert(items, settingRow("latency", tr("settings.latency"), ui.select({
        options = latencyLabels,
        selectedIndex = indexOf(LATENCIES, cfg.audio.latency),
        onChange = function(index)
            local value = LATENCIES[(tonumber(index) or 0) + 1]
            if value ~= nil and value ~= cfg.audio.latency then
                setTo("audio.latency")(value)
            end
        end,
    })))
    table.insert(items, settingRow("latency-ms", tr("settings.latency_ms"), textField("latency-ms-input", cfg.audio.latency_ms, 90, tr("settings.latency_ms_placeholder"), function(value)
        if value == "" then
            send("unset", { key = "audio.latency_ms" })
        else
            setTo("audio.latency_ms")(value)
        end
    end)))
    table.insert(items, muted(tr("settings.latency_hint", { ms = snapshot.config.effective_latency_ms or "?" }), "latency-hint"))
    table.insert(items, settingRow("keep", tr("settings.keep_connected"), ui.toggle({
        checked = idle == 0,
        onChange = function(value)
            setTo("audio.idle_timeout_secs")(value == "true" and 0 or DEFAULT_IDLE_SECS)
        end,
    })))
    if idle ~= 0 then
        table.insert(items, settingRow("idle", tr("settings.idle_timeout"), textField("idle-input", idle, 90, nil, setTo("audio.idle_timeout_secs"))))
    end
    table.insert(items, settingRow("follow", tr("settings.follow_system"), ui.toggle({
        checked = cfg.volume.follow_system == true,
        onChange = setTo("volume.follow_system"),
    })))
    table.insert(items, settingRow("start", tr("settings.start_volume"), ui.select({
        options = startLabels,
        selectedIndex = indexOf(START_VOLUMES, cfg.volume.start),
        onChange = function(index)
            local value = START_VOLUMES[(tonumber(index) or 0) + 1]
            if value ~= nil and value ~= cfg.volume.start then
                setTo("volume.start")(value)
            end
        end,
    })))
    table.insert(items, settingRow("metadata", tr("settings.metadata"), ui.toggle({
        checked = cfg.metadata.enabled == true,
        onChange = setTo("metadata.enabled"),
    })))
    table.insert(items, settingRow("client-name", tr("settings.client_name"), textField("client-name-input", cfg.network.client_name, 140, nil, function(value)
        if value ~= "" then
            setTo("network.client_name")(value)
        end
    end)))
    table.insert(items, muted(tr("settings.files", {
        config = snapshot.config.config_file or "",
        devices = snapshot.config.devices_file or "",
    }), "files"))
    return items
end

function render()
    if snapshot == nil then
        panel.render(ui.column({ gap = 12 }, { muted(tr("state.loading")) }))
        return
    end
    local body = banners()
    if snapshot.cli == "ok" then
        local volume = volumeSection()
        if volume ~= nil then
            table.insert(body, volume)
        end
        for _, section in ipairs({ speakersSection(), addSection(), settingsSection() }) do
            table.insert(body, ui.separator({}))
            for _, item in ipairs(section) do
                table.insert(body, item)
            end
        end
    end
    local children = { header(), ui.scroll({ flexGrow = 1, gap = 12 }, body) }
    if snapshot.version ~= nil then
        -- Outside the scroll area, so it is always visible.
        table.insert(children, ui.label({
            key = "version",
            text = tr("panel.version", { version = snapshot.version }),
            color = "on_surface_variant",
            fontSize = 11,
        }))
    end
    panel.render(ui.column({ flexGrow = 1, gap = 12 }, children))
end

noctalia.state.watch(STATE_KEY, function(value)
    if type(value) == "table" then
        snapshot = value
        render()
    end
end)

function onOpen(_context)
    -- Settings may have been changed elsewhere (terminal, another panel).
    send("refresh")
    render()
end

render()
