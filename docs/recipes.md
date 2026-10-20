# Recipes

Patterns that already work, taken from scenes that run. Every block here is a
whole scene and compiles: `./run-tests.sh` checks them.

- [A list of thousands](#a-list-of-thousands)
- [Dragging a list](#dragging-a-list)
- [A progress ring](#a-progress-ring)
- [A grid](#a-grid)
- [Something that melts into something else](#something-that-melts-into-something-else)
- [Gradients and paths](#gradients-and-paths)
- [Saving what the user picks](#saving-what-the-user-picks)
- [A normal window](#a-normal-window)
- [One bar per monitor](#one-bar-per-monitor)
- [A popup](#a-popup)
- [Reacting to a number changing](#reacting-to-a-number-changing)
- [How far into the song](#how-far-into-the-song)
- [Every window, with its picture](#every-window-with-its-picture)
- [Only for whoever sits there](#only-for-whoever-sits-there)

## A list of thousands

A model unfolds all its records on load, and the ceiling is 256. For a list of
thousands the scene declares only the **window** — what is on screen and a bit
more — and says how long the whole list is. The logic sends the slice.

```plm
language 0.1
scene Huge {
    surface { size: 420, 420; anchor: center }
    let ink = #f5f7f5
    let step = 34

    model rows max 16 { label: text }
    fact total = 0                  // how many there really are
    fact first = 0                  // where the window starts
    event slid ->

    box { from: 0, 0; size: 420, 420; corner: 16; color: #151616 }
    column list {
        at: 10, 10
        view: 400, 400
        step: step
        content: total * step       // the length of the whole list, not of the window

        for r in rows from first {
            group {
                size: 400, step
                move: 0, first * step   // put the copies where they belong in the whole list
                text r.label { at: 14, 17; anchor: left center; size: 12.5; color: ink }
                text number(r.index, 0) { at: 386, 17; anchor: right center; size: 10; color: ink; opacity: 35% }
            }
        }
    }
    //  Whenever it crosses a row — wheel, drag or jump — the logic gets asked for
    //  the slice that now fits.
    on change floor(list.scroll / step) { emit slid(list.scroll) }
}
```

```lua
local STEP, WINDOW, HOWMANY = 34, 16, 5000
local all = {}
for i = 1, HOWMANY do all[i] = { label = "row number " .. i } end
fact.total = HOWMANY

local at = -1
local function show(from)
    from = math.max(0, math.min(from, HOWMANY - WINDOW))
    if from == at then return end
    at = from
    local slice = {}
    for k = 1, WINDOW do slice[k] = all[from + k] end
    fact.first = from
    model.rows = slice
end

show(0)
on("slid", function(scroll) show(math.floor(scroll / STEP)) end)
```

Five thousand rows cost the same as sixteen: sixteen groups and sixteen zones,
0.49 ms per frame.

## Dragging a list

Nothing to declare. A layout with `view:` is dragged by default, and it is
grabbed **from the inside**: dragging is not the business of the topmost zone,
but of any zone that was under the pointer when it was pressed. So a list can be
dragged by one of its rows.

The scroll position is also a property like any other, which a rule can send
wherever it wants:

```plm
language 0.1
scene Jumps {
    surface { size: 300, 240 }
    let ink = #f5f7f5
    model rows max 10 { label: text }

    column list { at: 10, 10; view: 280, 180; step: 30; gap: 4
        for r in rows {
            group { size: 276, 26; text r.label { at: 8, 13; anchor: left center; size: 12; color: ink } }
        }
    }
    zone box top { at: 150, 216; size: 80, 26; corner: 8; cursor: pointer }
    box { at: 150, 216; size: 80, 26; corner: 8; color: #232727 }
    text "top" { at: 150, 216; anchor: center; size: 11; color: ink }
    on press top { list.scroll: 0 ~calm }
}
```

## A progress ring

`arc` draws an arc shaped like `∩`: it opens **upwards and symmetrically**, and
`span` is the whole angle it covers, not where it starts. A ring that fills from
the top clockwise comes from turning it by half of what it covers:

```plm
language 0.1
scene Ring {
    surface { size: 420, 140; anchor: top }
    box { from: 0, 0; size: 420, 140; color: #151616 }
    repeat k in 0..5 {
        let p = k / 4                                    // 0, 0.25, 0.5, 0.75, 1
        ellipse { at: 50 + k * 80, 70; radius: 26; stroke: 6; color: #f5f7f5; opacity: 14% }
        arc {
            at: 50 + k * 80, 70
            radius: 26
            width: 6
            span: p * 360deg                             // how much of the circle
            rotate: p * 180deg                           // half of it: that starts it at the top
            color: #9ed6bd
        }
        text number(p * 100, 0, "%") { at: 50 + k * 80, 70; anchor: center; size: 11; color: #f5f7f5 }
    }
}
```

With a battery: `span: batt.percent / 100 * 360deg` and
`rotate: batt.percent / 100 * 180deg`, because `battery.percent` runs from 0 to
100. `audio.volume`, on the other hand, runs from 0 to 1. The scales of every
service field are in the reference.

To have it fill counter-clockwise, negate the rotation. To start somewhere other
than the top, add a fixed angle to it.

## A grid

`wrap: n` turns a layout into a grid: n per line, then the next one. The cell is
as big as the largest child, and **what is not shown leaves no gap**, so the
rest move up with the layout's spring.

```plm
language 0.1
scene Grid {
    surface { size: 340, 220 }
    fact all = true
    box { from: 0, 0; size: 340, 220; color: #151616 }

    row tiles ~calm {
        at: 20, 20
        gap: 10
        wrap: 5
        repeat i in 1..15 {
            group {
                size: 50, 50
                show: all or i <= 6
                box { from: 0, 0; size: 50, 50; corner: 12; color: mix(#9ed6bd, #e8c07a, i / 14) }
                text "{i}" { at: 25, 25; anchor: center; size: 13; weight: 600; color: #151616 }
            }
        }
    }
    text "{tiles.count} tiles" { at: 20, 190; size: 11; color: #f5f7f5; opacity: 45% }
}
```

## Something that melts into something else

Shapes inside the same `body` melt by distance when they carry `blend`. This is
what QtQuick cannot do without tricks: opening is not two things meeting, it is
one thing stretching.

```plm
language 0.1
scene Melt {
    surface { size: 420, 300; anchor: top }
    fact open = false
    prop card = 0 ~calm

    let cx = 210
    let cy = -6
    let card.y = cy + 44 + card * 110

    follow card = if(open, 1, 0)

    body {
        color: #151616
        rim: 7%
        shadow: 0, 12, 30, 42%
        ellipse ball { at: cx, cy; radius: 32 }
        ellipse neck { at: cx, cy + 30 + card * 26; radius: 22 * card; blend: 26 }
        box sheet { at: cx, card.y; size: 360 * card, 180 * card; corner: 26; blend: 26 }
    }

    zone box ball_zone { at: cx, cy + 20; size: 140, 90; corner: 45; cursor: pointer }
    on press ball_zone { toggle open }
}
```

## Gradients and paths

A gradient takes from two to eight colours, each one saying where it falls, and
can be radial. A path is a broken or curved line; closed, it is filled.

```plm
language 0.1
scene Draw {
    surface { size: 400, 260 }
    box { from: 0, 0; size: 400, 260; color: #101111 }

    //  Linear, with a stop at 30 %.
    body {
        gradient: 20, 20 to 180, 20, #9ed6bd, #e8c07a 30%, #e86a9a, #151616
        box { from: 20, 20; size: 160, 50; corner: 12 }
    }
    //  Radial, from a centre outwards.
    body {
        gradient: radial 100, 160 radius 60, #f5f7f5, #9ed6bd 40%, #151616
        ellipse { at: 100, 160; radius: 60 }
    }
    //  A line chart, with round ends.
    path {
        at: 220, 40
        stroke: 2.5
        color: #9ed6bd
        move 0, 56
        line 30, 36
        line 60, 44
        curve 120, 12 via 90, 40
        line 150, 20
    }
    //  A filled arrow: closed paths get filled, concave ones too.
    path {
        at: 230, 150
        color: #e8c07a
        move 0, 22
        line 60, 22
        line 60, 8
        line 92, 32
        line 60, 56
        line 60, 42
        line 0, 42
        close
    }
}
```

## Saving what the user picks

Every scene — and every plugin — has a folder of its own and cannot get out of
it: no paths, no `..`, the same rule as `require`.

```plm
language 0.1
scene Settings {
    surface { size: 300, 140; anchor: center }
    permissions { services: "files", "files.write" }

    fact tone = 0
    fact saves = 0
    text folder = ""

    box { from: 0, 0; size: 300, 140; corner: 16; color: #151616 }
    row swatches { at: 20, 30; gap: 10
        repeat k in 0..3 {
            group {
                size: 34, 34
                prop on = 0 ~calm
                follow on = if(tone == k, 1, 0)
                zone box hit { from: 0, 0; size: 34, 34; corner: 17; cursor: pointer }
                ellipse {
                    at: 17, 17
                    radius: 11 + 4 * on
                    color: mix(mix(#9ed6bd, #e8c07a, k >= 1), #e86a9a, k >= 2)
                    opacity: 35% + on * 65%
                }
                on press hit { emit pick(k) }
            }
        }
    }
    event pick ->
    text "saved {saves} times" { at: 20, 92; size: 11; color: #f5f7f5; opacity: 45% }
    text folder { at: 20, 116; size: 10; color: #f5f7f5; opacity: 30%; width: 260; lines: 1 }
}
```

```lua
local FILE = "settings.json"

local saved = sys.ask("files.read", FILE, "json") or {}
fact.tone = tonumber(saved.tone) or 0
fact.saves = tonumber(saved.saves) or 0
text.folder = sys.ask("files.folder")

on("pick", function(which)
    fact.tone = which
    fact.saves = fact.saves + 1
    sys.call("files.write", FILE, { tone = fact.tone, saves = fact.saves })
end)

-- If the file changes underneath — another copy, an editor — reload it.
sys.watch("files:" .. FILE, function()
    local now = sys.ask("files.read", FILE, "json")
    if now then fact.tone = tonumber(now.tone) or fact.tone end
end)
```

## A normal window

`kind: window` asks for a window with its frame and its close button, the kind
the compositor places. Since anyone can resize it, `screen.width` and
`screen.height` are **its own** size and change while it runs.

```plm
language 0.1
scene Window {
    surface { size: 460, 320; kind: window; title: "pleamar · settings" }
    let ink = #f5f7f5
    let w = screen.width
    let h = screen.height

    body {
        gradient: radial w / 2, 0 radius max(w, h), #1d2320, #121313
        box { from: 0, 0; size: w, h }
    }
    text "A normal window" { at: 30, 40; size: 18; weight: 600; color: ink }
    path { at: 30, 70; stroke: 1.5; color: #9ed6bd; move 0, 0; line w - 60, 0 }
    box { at: w - 100, h - 40; size: 160, 38; corner: 12; color: #232727 }
    text "all right" { at: w - 100, h - 40; anchor: center; size: 13; color: ink }
}
```

## One bar per monitor

`screens: each` repeats the surface on every monitor, and **each copy has its
own**: its properties, its zones and its rules. Inside, `$screen` is its number.

```plm
language 0.1
scene Bars {
    surface { size: full, 40; anchor: top; screens: each }
    let ink = #f5f7f5
    model mon max 4 { title: text }

    box { from: 0, 0; size: 1920, 40; color: #151616 }
    text mon.$screen.title { at: 20, 20; anchor: left center; size: 12.5; color: ink }
    text screen.name { at: 200, 20; anchor: left center; size: 11; color: ink; opacity: 40% }
}
```

`--screen A,B` hands out the copies between those monitors, which is how two
screens get rehearsed without having two.

## A popup

A popup is a little window of its own that can go outside the surface: a menu
hanging off a bar does not need the bar to be as tall as the menu.

```plm
language 0.1
scene Menu {
    surface { size: 300, 60; anchor: top }
    fact open = false

    box { from: 0, 0; size: 300, 60; color: #151616 }
    zone box button { at: 150, 30; size: 100, 30; corner: 15; cursor: pointer }
    box { at: 150, 30; size: 100, 30; corner: 15; color: #333535 }
    text "menu" { at: 150, 30; anchor: center; size: 12; color: #f5f7f5 }
    on press button { toggle open }

    popup sheet { at: 100, 56; size: 200, 16 + items.height; open: open
        column items { at: 8, 8; gap: 4
            repeat i in 0..4 {
                box { size: 184, 30; corner: 6; color: #2b2d2d }
            }
        }
    }
}
```

## Reacting to a number changing

`on change expr` fires when that number stops being what it was. It does not
fire when it is born. It is what tells the logic something moved, whatever moved
it.

```plm
language 0.1
scene Watch {
    surface { size: 240, 80 }
    permissions { services: "clock", "audio" }
    service clock as now { time: text = "--:--"; minute: number }
    service audio { volume: number }

    prop bump = 0 ~lively
    event minute_changed ->

    box { from: 0, 0; size: 240, 80; color: #151616 }
    text now.time { at: 120, 40 + bump; anchor: center; size: 26; color: #f5f7f5 }

    //  The minute changed: let it show.
    on change now.minute { impulse bump -520; emit minute_changed }
    //  Someone moved the volume, from anywhere.
    on change audio.volume { impulse bump -160 }
}
```

`impulse` pushes a spring instead of setting it: it leaves on its own and comes
back with its own bounce.

## How far into the song

`media.position` is where the player said it was, the last time it said
anything: on a new song, a pause, a seek, a change of `rate`. In between,
nobody asks it again. The scene carries it on with its own clock while the song
plays, and keeps the moment it heard it, in `time`, to know how far to carry it:

```plm
language 0.1
scene Progress {
    surface { size: 240, 60 }
    permissions { services: "media" }
    service media as song { playing: bool; position: number; rate: number; length: number; can_seek: bool }

    fact heard: number = 0                      // `time` when the position last came in
    on change song.position { heard = time }
    let elapsed = min(song.position + if(song.playing, (time - heard) * song.rate, 0), song.length)

    prop done = 0 ~120ms                        // the jumps of a seek, eased
    follow done = elapsed / max(song.length, 1)

    box { from: 0, 0; size: 240, 60; color: #151616 }
    box { from: 20, 38; size: 200, 4; corner: 2; color: #f5f7f5; opacity: 14% }
    box { from: 20, 38; size: 200 * done, 4; corner: 2; color: #9ed6bd }
    text "{elapsed, time}" { at: 20, 22; anchor: left center; size: 12; color: #f5f7f5 }
    text "{song.length, time}" { at: 220, 22; anchor: right center; size: 12; color: #f5f7f5; opacity: 50% }
}
```

Paused, `elapsed` is just the position the pause reported. A scene that names
`time` never rests, so this one paints every frame while it is open, paused or
not. With reduced motion `time` stops, and the bar moves only when the player
reports.

To drag it, the logic sends the player there: `sys.call("media.seek", seconds)`
once, when the pointer lets go, not on every move —each call is several
round trips to the player—, and `sys.call("media.step", seconds)` for a few
seconds either way from a key. Each is a permission of its own beside
`"media"`: `services: "media", "media.seek", "media.step"`, or `"media.*"`.
Where `can_seek` is false the player will not move (a live stream), so the bar
is better left still. The new `position`
comes back as a report, and `on change song.position` catches it like any other.

## Every window, with its picture

An overview, a switcher: each window of the desktop —the scene's own or any
program's— with a small picture of it, live. The `thumbnails` service lists
them; nothing is copied until the logic says which ones (`thumbnails.want`),
and then only when they change. A picture the compositor stopped copying
stays as it was, dimmed here by `stale`.

```plm
language 0.2
scene Overview {
    surface { size: 1000, 220; anchor: bottom }
    permissions { services: "thumbnails", "thumbnails.*" }
    model shots max 6 { id: text; title: text; app: text; picture: image 300, 180; stale: bool }
    service thumbnails { list: shots }

    box { from: 0, 0; size: 1000, 220; color: #10151a }
    row { at: 20, 10; gap: 20
        for s in shots {
            column { gap: 4
                image s.picture { size: 300, 180; opacity: if(s.stale, 50%, 100%) }
                text "{s.title}" { size: 11; width: 300; lines: 1; color: #e6e6e6 }
            }
        }
    }
}
```

And in its logic, which ones (all of them, here; an overview with pages
would say only the page shown):

```luau
sys.call("thumbnails.want", "all")
```

An overview that shows the windows moving asks for them live instead: the
same `picture`, but each frame goes from the compositor to the image without
a file, at the size the image is drawn (a very large one a little softer)
and up to 30 times a second. Asked for no longer, each keeps its last frame
as a file.

```luau
sys.call("thumbnails.live", "all")
```


## Only for whoever sits there

A mark that must be seen on the monitor but not in what is captured of it: that
the desktop is being used from elsewhere, a reminder you do not want in a
screen share. `captures: hidden` leaves the surface out of screenshots,
recordings, shared screens and remote desktops (pleamar-wm does it; other
compositors show it in captures too). Put it on a surface of its own, and let
it take no clicks: whoever sees the screen from elsewhere does not see it, and
would press it by mistake.

```plm
language 0.2
scene Mark {
    fact remote = true
    prop shown = 0 ~900ms
    follow shown = remote
    surface { size: full, 40; anchor: top }
    text "the bar" { at: 20, 20; anchor: left center; size: 13; color: #e6e6e6 }

    surface mark {
        size: full, full
        anchor: top_left
        level: overlay
        screens: each
        captures: hidden
        open: shown > 0.001
        group {
            opacity: shown
            box { from: 2, 2; size: mark.width - 4, mark.height - 4; corner: 16; stroke: 2; color: #e8c26a }
            text "In use from elsewhere" { at: 24, mark.height - 24; anchor: left center; size: 12; color: #e8c26a }
        }
    }
}
```
