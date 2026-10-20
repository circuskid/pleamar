# The language — the guide

> **This is the guide**: it reads straight through and tells the why. The exact description —grammar, every element with what it accepts, version— is in [Language reference — version 0.1](11-language-reference.md).

**Status:** implemented (`src/language/`). A `.plm` file goes in and out comes the same `Scene` that used to be written in Rust. [The language — draft 0](03-language-draft-0.md) was the sketch, first written in Spanish; **this is what there really is, with the keywords in English**. Still with no name of its own.

```sh
pleamar --scene examples/marea.plm      # reloads itself when the file is saved
pleamar --check examples/marea.plm   # reads it, says whether it is right, and exits
```

Full examples: **`examples/bar.plm` (a real bar: workspaces, window, clock and volume)**, `examples/marea.plm` (the ball and its card, 150 lines), `examples/face.plm` (layers and gestures, no logic at all) and `examples/tray.plm` (components, `repeat` and layout: a notice list that grows and shrinks).

## The idea in one sentence

**Everything written here is run by the renderer, on its own.** There are no loops and no mutating variables: everything terminates and everything is checked at load time. The logic —outside, in a `.luau` with the same name: [The logic, in Luau](10-luau-logic.md)— only sets facts, texts and events.

## Several files: `import` and `library`

```
// examples/common/palette.plm
library Palette {
    let ink  = #f5f7f5
    let mint = #9ed6bd
    spring snappy = 320, 26
    component Dot(tone) { size: 10, 10; ellipse { at: 5, 5; radius: 5; color: tone } }
}
```
```
// examples/icons.plm
import "common/palette.plm"
import "common/menu.plm"              // which in turn imports the palette: it is read once

scene TrayIcons {
    let mint = #e86a9a               // the scene's one wins: that is how a tone is changed
    …
}
```

The `import`s go before `scene` (or before `library`), and the path is relative **to the file that imports**, not to wherever it is launched from. A library **only declares** —`let`, `spring` and `component`—: what is painted, what moves and the boundary with the logic belong to the scene. What is imported behaves as if it were written at the start of the scene, so a library component sees the facts, the events and the colors of whoever uses it.

Two components with the same name cannot live together (`there is already a component 'Dot', at palette.plm:4`); a circle of imports is reported with its path; and **an error says which file it is in**, including when it is inside a library. Saving a library hot-reloads the scenes that use it.

**A plugin is a library with its logic alongside** (`clock.plm` + `clock.luau`): it declares its boundary and its permissions, which live apart from the scene's.

```
library Clock strict {
    permissions { run: "date" }
    text now = "--:--"
    event tapped ->
    component Clock(tone: color = ink) { row face { …; text now { … } };  on press face { emit tapped } }
}
```

From the scene its boundary is called `Clock.now`, `Clock.tapped`; its logic only sees its own and only has its own permissions. `examples/with-plugin.plm` is a scene with no logic of its own that puts a clock in like this.

A scene can ask for **several windows**: `surface { … }` is its own, and `surface panel { …; …what it draws… }` one of several, with its `open:` to appear and disappear. They all share properties, facts and rules.

## General shape

```
scene Name {
    word header … { block }           // a node
    name: value, value                // a property
}
```

A line break or a `;` ends a statement. `//` comments to the end of the line. Names take dots (`orb.x`, `note.0.title`).

**The order is whatever suits the reader.** The file is read in four passes —declarations; `let` and layers; drawing; rules—, so a rule can come before the shape it names, and a `prop` at the end. Only a `let` has to come before whoever uses it.

Numbers with a unit: `40`, `40px`, `34%` (= 0.34), `138deg` (into radians), `320ms` and `14s` (durations). Colors `#151616` or `#fff`. Texts `"in quotes"`.

## Declarations

`permissions { run: "date"; services: "audio", "apps" }` — what the scene's logic may touch of the system. Undeclared, nothing: see [The logic, in Luau](10-luau-logic.md).

| Statement | What it is |
| --- | --- |
| `surface { size: 720, 224; anchor: top; margin: 40; level: top; reserve: 0; screens: "HDMI-A-1" }` | The window it asks for. `size: full, 44` is the whole width of the monitor; how much that is can be read in `screen.width`. `anchor`: top, bottom, left, right, top_left…, center. `level`: background, bottom, top, overlay. `screens: all` or a list. `keyboard: none | on_demand | exclusive` |
| `prop orb.x = 360 ~lively` | An animated property: a spring. Without `~`, `lively` |
| `pose eyes = 14` | A pose property: the one a gesture leads by the hand |
| `fact open = false` · `fact mode: low \| normal \| critical = normal` | Something that is true for a while. The logic and the rules set it. A yes or no, a number, or an enum: `mode == critical`, `"{mode}"` |
| `event confirmed` · `event view_event ->` | Something that happens. With `->`, it also goes out to the logic |
| `text notice.title = "Meeting"` | A live text: the logic changes it |
| `image fox = icon "firefox", 48, 48` · `… = file "path.png", 48, 48` | An image, and the largest size it is painted at |
| `measure label` | Creates `label.width` and `label.height`, filled in by whichever text carries `measure: label` |
| `let panel.x = orb.x + 62` | A name for an expression |
| `let mint = #9ed6bd` · `let warm = mix(mint, #f84, 50%)` | A name for a color |
| `spring bouncy = 170, 12` | A spring of your own: stiffness, damping. In-house: lively, calm, quick, slow, gentle, pose. Inline: `~spring(170, 12)` |

## Expressions

`+ - * /`, parentheses, `< > <= >= == !=`, `and or not`, `true false`. True is more than 0.5.
Functions: `min`, `max`, `abs`, `clamp(x, a, b)`, `smooth(a, b, x)`, `mix(a, b, t)`, `if(cond, a, b)` and **`vel(prop)`** —the velocity of a spring, which only the renderer knows—.
Valid as a name: a `let`, a `prop`, a `fact`, a measure (`label.width`) and the presence of a claim (`shape.rec`: 1 while it wins).

## Drawing

Painting happens in the order it is written.

```
body {                                  // shapes blended into one silhouette
    color: #151616                      // or gradient: x0, y0, x1, y1, #c0, #c1
    rim: 5%;  light: 3%, y0, alto;  shadow: dx, dy, difusa, alfa;  border: grosor, #color;  opacity: e
    ellipse orb { at: x, y; radius: r; scale: sx, sy }
    box { from: x, y; size: w, h; corner: r;  blend: neck }     // blend: how much it blends with the previous one
}

ellipse { at: x, y; radius: r; color: #…; opacity: e }          // a loose shape
box     { at: cx, cy | from: x, y;  size: w, h;  corner: r }
arc     { at: x, y; radius: r; span: 138deg; width: w }         // like "∩"
line    { from: x, y; to: x, y; width: w }
// to any of them: rotate: angle · stroke: width (outline only: a circle → a ring)

text notice.title { at: x, y; anchor: left center; width: 354; lines: 1; size: 20; weight: 500;
                    color: #…; opacity: e; align: left; line_height: 1.3; family: "Inter"; measure: label }
text "Dismiss"  { at: x, y; anchor: center }
text number(volume * 100, 0, " %") { … }                     // a number out of an expression: decimals and what goes after

image fox { at: x, y; size: w, h; opacity: e; tint: #9ed6bd }   // tint: for symbolic icons

group {                                 // the tree: transforms, blends and clips its children
    pivot: x, y;  rotate: e;  scale: s | sx, sy;  move: dx, dy
    opacity: e                          // they blend as ONE thing
    clip inset 3 ellipse { … }          // holds until the end of the group
    …hijos…
}
```

**A named shape is a zone if some rule names it** (or if it carries `active`, or it was declared with `zone`): it can be pressed, and the mouse enters through it. A name put there only to read better does not stop a click. It inherits the transforms of the groups it is in. `active: expr` switches it on and off. `zone box whole { … }` is a zone that is not painted. **Whichever is declared later sits on top.**

## Texts with slots

```
text "Hello, {who}"                                   // a live text
text "{volume * 100} %"                              // an expression, no decimals
text "{temperature, 1} °C"                           // with one
text "{upper(n.app)}"                                // upper() and lower(), over a text
text "{n.title}{? · {n.body}}"                       // {? …}: the stretch is only there if its text is not empty
text "some real {{braces}}"                     // two in a row are one
Chip("{notes.total} nuevos", mint)                   // and it goes into a component already resolved
```

The renderer puts it together, every time any of its parts changes: a text the logic sets, a fact, a property with its spring (`"{progress * 100} %"` climbs on its own). Inside a slot everything that is valid in an expression is valid, and the names resolve where the string is written, not where it is used. An error inside a string points at its exact character:

```
6:25: there is nothing called 'volumen'. Did you mean 'volume'?
   6 |     text "Hola, {who}: {volumen * 100} %" { … }
                               ^
```

`text number(expr, decimals, "after")` still works, but it is no longer needed.

## Models and `for` — lists that come from data

```
model rows max 14 {                       // a list of records, all with these fields
    label: text
    enabled: bool = true                  // what it is worth if the record does not bring it
    depth: number
}

column list { for r in rows { Row(r) } }  // one copy per record

component Row(r) {                        // a record is passed like any parameter
    size: 220, 30
    box hit { from: 0, 0; size: 220, 30; corner: 7; active: r.enabled }
    text r.label { at: 12 + r.depth * 12, 15; anchor: left center; size: 13.5; color: ink }
    on press hit { emit choose(r.index) } // `index`: its position, from 0
}
```

A field can also be an enum (`urgency: low | normal | critical = normal`), an image (`icon: image 24, 24`, and then `image r.icon { … }` with nothing else to declare) or **another list** (`list items max 6 { label: text }`, walked with `for it in r.items`).

The logic hands it over whole, in one go: `model.rows = list` (see [The logic, in Luau](10-luau-logic.md)). A `text` field is used where a live text goes (`text r.label { … }`, `image pic = from r.icon, 24, 24`); a `number` or `bool` one, in any expression. Each turn of the `for` **only exists if the list reaches that far**: it is not seen, it takes no room in its layout and its zones do not stop a click. `rows.count` is how many are seen and `rows.total` how many there really are (`show: rows.total > rows.count` for a "there are more"). `max` is how many fit (16 if not said); a single record is named `rows.0.label`.

A `for` works inside a `row` or a `column` and also loose, and inside a `popup`.

## Components, repeats and layouts

**A component says what it needs.** The parameters can carry a type and a default value, and the arguments, a name:

```
component MenuRow(r: record, chosen: event, tone: color = mint, width: number = 220) {
    …
    on press hit { emit chosen(r.index) }     // the event it was handed, whatever it is called outside
}

for r in rows { MenuRow(r, chosen: choose) }  // by position and, from wherever, by name
```

Types: `number`, `bool`, `color`, `text`, `record` (a model record), `event`, `image`, `gesture` and `spring`. Anything missing, left over or of the wrong type is an error **where it is used**, with the whole signature: `'MenuRow' is missing 'chosen' (an event): it is MenuRow(r: record, chosen: event, tone: color = …, width: number = …)`. Without a type, a parameter is whatever the argument looks like, as until now.

```
component Note(i) {                     // parameters: numbers, colors, "texts", or the name of a live text
    size: 360, 58                       // how much room it takes, for whoever lays it out
    prop lit = 0 ~quick                 // each copy has its own
    body { color: mix(#1b1c1c, #2b2d2d, lit); box hit { from: 0, 0; size: 360, 58; corner: 14 } }
    text note.$i.title { at: 16, 38; anchor: left center }
    on enter hit { lit: 1 ~quick }      // …and its own zone and its own rules
    on press hit { emit opened.$i }
}

Note(3) { move: 20, 40 }                // a copy is a group: move, rotate, scale, opacity

repeat i in 0..5 { event opened.$i -> } // unrolled at load time; `$i` goes into the names

column list ~calm {                     // or `row`. With a spring, each child TRAVELS to its slot
    at: 180, 66;  gap: 8;  padding: 0;  align: start | center | end
    anchor: right                       // which part falls on `at`: left, center, right · top, middle, bottom
    fill: #222;  corner: 12             // a background the size of whatever it holds
    repeat i in 0..5 { Note(i) { show: count > i } }
    space 6
}
box { from: 180, 66 + list.height + 10; size: head.width, 2 }   // with a name, it can be measured
```

- What a copy declares inside (`prop`, named shapes, measures) **is its own**: two copies do not tread on each other, and the rules inside talk about their own.
- **There is no layout engine.** Each child's place is an expression —what the previous ones take up—: if one grows, the rest shift; with `~spring`, they shift animated. `show:` decides whether a child is there: it takes room and is seen, or neither one nor the other, and with a spring that too is a journey.
- Inside a layout a child does not say where it goes. The ones that know how much room they take are `box`, `ellipse`, `image`, `text` (it measures itself), another `row`/`column`, and a `group` or a component with `size:`.
- A list of variable length is today one of fixed capacity with `show:`. See `examples/tray.plm`.

**A slot for children.** Whatever a copy brings inside its block goes where its component says `children`, and it is read with the names of whoever wrote it:

```
component Card(title: text) {
    size: 300, 40 + inside.height
    body { color: #1b1c1c; box { from: 0, 0; size: 300, 40 + inside.height; corner: 12 } }
    text "{upper(title)}" { at: 12, 18; anchor: left center; size: 11; color: ink }
    column inside { at: 12, 32; gap: 4;  children }
}
Card("Alerts") { text title { size: 14; color: ink };  repeat i in 0..2 { text "row {i}" { size: 13; color: ink } } }
```

A component can have **several slots** (`children header`, `children footer`; in the copy, `header { … }`). And a layout can put something **between** its children and know **how many** there are:

```
column inside { at: 14, 40; gap: 5
    children
    between i { box { size: 272, 1; color: ink; opacity: if(i == 1, 50%, 12%) } }   // only between the ones there are; `i`: between which ones
}
text "· {inside.count}" { … }
```

**Libraries that do not snoop.** `library Menu strict { … }`: its components only read what they ask for by parameter, what they declare and what belongs to their library. It is what is needed to trust someone else's library.

## Layers — who wins

```
layer shape ~quick {                    // from highest to lowest priority
    rec    while recording
    alert  for 700ms after urgent
    happy  for 620ms after confirmed, done
    broken from failed until forgotten
    eyes                                // the default
}
layer card ~calm {
    shown while open { orb.x: 140 ~lively;  panel.w: 406 ~calm after 70ms;  neck: 0 ~calm after 560ms }
    rest             { orb.x: 360 ~lively after 150ms;  panel.w: 0 ~calm after 90ms }
}
```

The first claim that holds wins; when it stops holding, the next one is seen, on its own. A claim's block is its choreography: where each property goes, with which spring and with which delay. **The target is an expression**, and it is evaluated when its time comes: `r: base * 3 ~lively`. `shape.rec` can be used in any expression.

## Rules — what makes things change

```
on press view            { open = false; impulse orb.y -620; emit view_event }
on press right dot       { emit menu }               // or `middle`
on release dot           { r: 30 ~lively }           // what was pressed there is released, wherever the mouse is by now
on hold dot for 500ms    { emit held }               // it has been held down that long
on enter view            { glow: 1 ~quick }          // the :hover of CSS
on leave view            { glow: 0 ~quick }
on hover orb for 320ms   { open = true }
on away whole for 420ms  { open = false }            // it was over it and has been out that long
on scroll sound          { emit volume_step(wheel) } // the wheel, in any zone underneath it
on drag track            { volume = clamp(local.x / 64, 0, 1) }   // it moves with the button held down
on key Escape            { open = false }            // the surface has to ask for the keyboard: `keyboard: on_demand`
on key Ctrl+k            { toggle open }             // with modifiers: Ctrl+, Alt+, Super+ (Shift goes in the key itself)
on submit query          { emit launch(sel) }        // Enter inside the `query` field
on focus                 { glow: 1 ~quick }          // the surface gains the keyboard…
on blur                  { open = false }            // …or loses it: something else was clicked
on drop tray             { emit dropped }            // something from another application is dropped on the zone
on toggle                { toggle open; focus query } // an event, which can come from outside (see below)
on idle for 14s while not open { asleep = true }
on confirmed             { play joy }
every 2.5s..7s while awake { play yawn }
```

Any rule can carry `while expr` at the end of its header (`on press dot while armed { … }`): it is checked at the moment it fires.

Effects: `fact = expression` (evaluated when it fires), `toggle fact`, `emit event` or with a payload `emit opened(i)`, `impulse prop velocity`, `play gesture`, `focus field` (gives it the writing cursor), `blur`, and `prop: value ~spring after 70ms`.

**What a rule can read from the mouse**, as if they were facts: `pointer.x`, `pointer.y` (in the surface), `local.x`, `local.y` (**inside the zone**: in a layout, (0, 0) is the corner of the slot, wherever it is on screen; the zone's own `from:` is not taken away), `drag.dx`, `drag.dy` (since the press) and `wheel` (notches; positive, upwards). A drag carries on even if the mouse leaves the zone, until release.

A shape can carry `cursor: pointer | text | grab | grabbing`. **A named `row` or `column` is a zone too** —its whole box, underneath those of its children—: that way the wheel works across a whole pill.

Its size (`list.width`, `list.height`) can be read **anywhere in the file, including before** where it is declared: the panel that wraps a list can follow its height (`follow tall = 84 + list.height`).

> Careful with anything dragged inside an anchored layout: if something inside changes width meanwhile (a "54 %" that becomes "100 %"), the layout reflows and the zone moves under the mouse. Give a fixed width to whatever changes (`width: 42; align: right`).

## Typing: `input`

```
text query = ""
input query { at: 48, 44; width: 504; size: 20; color: ink
              placeholder: "Search for an app…"; selection: #2f5f52 }
```

A one-line field edited by **the renderer**: every key is seen on the next frame, whatever state the logic is in. The field is named after the `text` it edits, and that same name is its zone (pressing gives it the focus and places the cursor; dragging selects). It knows the usual: arrows, Home and End, Ctrl+arrow by words, Shift to select, Ctrl+A, Ctrl+C / X / V against the system clipboard, Backspace and Delete, and it repeats a key held down. If the text does not fit, it slides so the cursor stays visible.

The logic hears about every change (`text:query`) and about Enter (`submit:query`); what is not typing —Escape, the up and down arrows— still reaches `on key`.

**The keyboard, only when needed.** `keyboard: exclusive while open` in the `surface`: while `open` is false the surface does not ask for the keyboard, and the desktop stays with whoever had it.

## Popup surfaces: `popup`

```
fact menu_open = false
on press right hit { menu_open = true }

popup menu {
    at: 285, 56                       // where it comes out, inside the scene's surface
    size: 200, 16 + items.height      // expressions: it measures whatever its list measures
    open: menu_open                   // a fact: open while it is true

    body { color: #1b1c1c;  box { from: 0, 0; size: 200, 16 + items.height; corner: 12 } }
    column items { at: 8, 8;  repeat i in 0..4 { Item(i) } }
}
```

A real surface, child of the main one: **it can go outside it** (a menu under a 44 px bar). What is inside is drawn with (0, 0) at its corner and it is scene like the rest: same springs, components, zones and rules, and `menu_open` is set and cleared from anywhere. If the system closes it —something outside was clicked—, the fact goes false and the logic hears about it (`fact:menu_open`). It goes at scene level, not inside a group. If it does not fit on the screen, the compositor slides it until it does.

## Images the logic picks

```
text  pic.$i  = ""                        // the logic puts "firefox" here, or a path starting with /
image icon.$i = from pic.$i, 24, 24       // and the image is whatever that text says
```

When the text changes, the workshop looks for the new image on its own thread and the renderer keeps showing the previous one until it arrives. An empty text is no image.

## Orders from outside

```
pleamar --say launcher "emit toggle"
```

Every running scene listens on a socket with its name (the file's). `emit event` or `emit event 3` fires its rules as if the logic had emitted it. Also `fact name value`, `text name whatever it should say` (the logic hears about it, as if someone had typed it), `focus field`, `quit`, and **`get name`, which answers** with whatever that fact, text or property is worth: `pleamar --say launcher "get open"` → `1`. And **`describe`**, which answers with what there is on screen to read and touch, by name: what an agent needs to use the scene without looking at its pixels (`describe json`, as data; see the reference, *Told to an agent*).

**Several at once, in order**: each command after the scene's name is a line, and they all go down one connection, so the scene runs them in that order. A popup's texts are in place before the emit that shows it:

```
pleamar --say osd "text label Volume" "fact value 0.4" "emit popup"
```

A `get` right after an `emit` answers before the emit's rules have run. Put a `wait` between them (`"emit flip" "wait flag == true 1s" "get flag"`).

**From another scene** it is the same order, from its logic: `sys.call("say", "osd", "emit popup")`, with `permissions { services: "say:osd" }` (see the reference, *Talking to another scene*).

**A global shortcut is this**: a compositor bind that runs that order. In Hyprland:

```lua
hl.bind("SUPER + space", hl.dsp.exec_cmd('pleamar --say launcher "emit toggle"'))
```

## What it runs on its own

```
blink eyelid every 2.4s..6s for 170ms
wave  breath = sleep * 1.3 at 1.7                    // amplitude · sin(1.7 t)
spin  angle by 0.9                                   // += 0.9 per second
follow chip.w = label.width + 32                     // follows the expression with its spring
look  gaze.x, gaze.y at orb.x, orb.y reach 5, 3.2 within 140 rest 3, 0.6
```

## Gestures

```
gesture nod reflex {                    // classes: ambient < posture < reflex < asked < state
    130ms out_quad         { look.y: 4; eyes: 10 }
    170ms out_back hold 60ms emit shutter { eyes: 15 }
    160ms                               // no block: back to base
}
posture working while searching { … }   // repeats on its own while it is true
```

A gesture only cuts another of its own class or lower. Whatever a keyframe does not name goes back to its base. Easings: linear, in_quad, out_quad, in_cubic, out_cubic, in_out_sine, out_back.

## Errors

With line, column, the chunk of the file and, if it looks like something, a suggestion. **Every one found is reported** (up to eight), not just the first:

```
marea.plm:91:28: there is nothing called 'pannel.h'. Did you mean 'panel.h'?
  91 |             size: panel.w, pannel.h
                                  ^
```

## Hot reload

Saving the file reads it again (a scene like Marea, in under a millisecond). If it is right, it replaces the old one **without losing anything**: the properties keep value and velocity, the facts and the texts stay as they were, and if the file changed a target, it heads there with its spring. If it is wrong, it says why and the old one carries on.
