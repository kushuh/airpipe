--!nonstrict
-- Bar widget: the AirPlay glyph, coloured by state, with an optional
-- "speaker · volume" label. Renders the service's "snapshot"; a click opens
-- the panel and scrolling changes the volume (plugin.toml [widget.actions]).

local STATE_KEY = "snapshot"

local snapshot = noctalia.state.get(STATE_KEY)

local function tr(key, args)
    return noctalia.tr(key, args)
end

local function streamingNames(status)
    local names = {}
    for _, speaker in ipairs(status.speakers or {}) do
        if speaker.state == "streaming" then
            table.insert(names, speaker.name)
        end
    end
    return names
end

local function hasProblem(status)
    for _, speaker in ipairs(status.speakers or {}) do
        if speaker.state == "retrying" or speaker.last_error ~= nil then
            return true
        end
    end
    return false
end

-- glyph colour, label, tooltip title for the current state
local function describe()
    if snapshot == nil or snapshot.cli == "unknown" then
        return "on_surface_variant", "", tr("state.loading")
    end
    if snapshot.cli ~= "ok" then
        return "error", "", tr(`state.{snapshot.cli}`)
    end
    local status = snapshot.status
    if status == nil or not status.running then
        return "on_surface_variant", "", tr("state.off")
    end
    local output = status.output or {}
    local volume = output.volume ~= nil and tr("widget.volume", { volume = math.floor(output.volume + 0.5) }) or ""
    local playing = streamingNames(status)
    if #playing > 0 then
        local label = table.concat(playing, " + ")
        if volume ~= "" then
            label = `{label} · {volume}`
        end
        return "primary", label, tr("state.playing", { speakers = table.concat(playing, ", ") })
    end
    if hasProblem(status) then
        return "error", volume, tr("state.problem")
    end
    return "on_surface", volume, tr("state.ready")
end

local function render()
    local color, label, title = describe()
    barWidget.setGlyph("cast")
    barWidget.setGlyphColor(color)
    barWidget.setText(noctalia.getConfig("show_label") == true and label or "")
    barWidget.setTooltip({
        { key = tr("widget.tooltip.title"), value = title },
        { key = "", value = tr("widget.tooltip.hints") },
    })
end

noctalia.state.watch(STATE_KEY, function(value)
    snapshot = value
    render()
end)

render()
