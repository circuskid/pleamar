# Parity with Quickshell — what is actually missing

**What this note is for.** pleamar is **an alternative to Quickshell**, not Marea's engine. Marea was the stress test at the start, and some day it will be rewritten in this language; but what decides whether this is worth anything is whether someone who writes their desktop in Quickshell today can write it here. This note is that yardstick, and it overrules [Test: does the real Marea fit in the language?](04-marea-test.md).

**Where the numbers come from.** Not from Quickshell's documentation: from **two real configurations** on this machine —`~/.config/quickshell` (k4, 278 QML files) and `proyecto-marea` (370)—, counting which types they instantiate. A type nobody uses is not a gap; one that shows up 800 times is.

## 1. What they actually use

Instantiated types, with the number of times they appear across the two configurations:

| From QtQuick | | pleamar |
| --- | --- | --- |
| `Rectangle` | 819 | ✅ `box`, and on top of that blended shapes, shadow, rim and light |
| `Text` | 655 | ✅ `text`, with slots (`"{a} · {b}"`), measuring and clipping |
| `NumberAnimation` · `SequentialAnimation` · `ParallelAnimation` · `PauseAnimation` · `ScriptAction` | 469 · 78 · 38 · 42 · 37 | ✅ springs (`~calm`), layers with delay, and `gesture` (which is a timeline: the same as `SequentialAnimation`) |
| `RowLayout` · `ColumnLayout` · `Row` · `Column` | 460 · 279 · 106 · 74 | ✅ `row` / `column` with gap, padding, align, anchor, `between` and spring |
| `Repeater` | 364 | ✅ `repeat` (fixed) and `for` (over a model) |
| `MouseArea` · `HoverHandler` | 351 · 33 | ✅ zones: `on press/enter/leave/hover/drag/scroll/hold` |
| `Item` | 351 | ✅ `group` |
| `Timer` | 195 | ✅ `every` in the scene, `after`/`every` in the logic |
| `Connections` | 145 | ✅ `on event` rules, `on("fact:x")` |
| `Shape` · `ShapePath` · `PathLine` | 40 · 78 · 56 | ✅ `path` with `move`, `line`, `curve` and `close`: filled or stroked, and it blends with everything else |
| `Component` · `Loader` | 67 · 64 | 🟡 `component` yes; deferred loading no · §2 |
| `GradientStop` | 66 | ✅ up to eight stops, each one where it says, and radial as well as linear |
| `Image` | 63 | ✅ `image`, from a file, from an icon or from a piece of data |
| `TextInput` | 34 | ✅ `input` (single line; no IME) |
| `ListView` · `Flickable` | 31 · 22 | 🟡 `view:` in a layout, with `content:` and `for … from` for lists of thousands in a handful of copies, and dragged by grabbing any row. No inertia when let go |
| `Flow` | 28 | ✅ `wrap: n` in a `row` or `column`: a grid, and what is not seen leaves no gap |
| `QtObject` | 38 | 🟡 standalone properties, no grouping |

| From Quickshell | | pleamar |
| --- | --- | --- |
| `Process` | 83 | ✅ `run`, `spawn`, `kill`, with permissions |
| `Singleton` | 47 | ✅ a plugin (a library with logic) is this, and with a boundary and permissions on top |
| `Region` | 27 | ✅ the input region is computed on its own, from the zones |
| `FileView` | 16 | ✅ `files` service, with a folder of its own per scene and per plugin |
| `PanelWindow` | 10 | ✅ several per scene (`surface panel { … }`), each with its `open:` |
| `Variants` | 7 | ✅ `screens: each`: one surface per monitor, each with its own state |
| `ShellRoot` · `Scope` | 7 · 4 | ✅ `scene` |
| `IpcHandler` | 4 | ✅ `pleamar --say`, with `emit`, `fact`, `text`, `get` |
| `IconImage` | 3 | ✅ `image x = icon "…"` |
| `SystemClock` | 2 | ✅ `clock` service, and `service clock as now { … }` with no logic at all |
| `ScreencopyView` | 2 | 🟡 a window: the `thumbnails` service, a small picture of each window of the desktop that follows it as it changes (ext-image-copy-capture); a whole screen as it is at a moment, `capture.freeze`. A whole screen, live, not yet |
| `NotificationServer` | 2 | ✅ `notifications` service |
| `FloatingWindow` | 2 | ✅ `kind: window`, with its title; and what it draws is measured against the window's own size |
| `WlSessionLock` | 1 | ✅ `kind: lock` (ext-session-lock), on every monitor or those `screens:` names |
| `LazyLoader` | 1 | ⬜ · §2 |
| `GlobalShortcut` | 1 | 🟡 a compositor bind that calls `--say` |
| `PwObjectTracker` | 1 | ✅ `audio` service, speaking PulseAudio's protocol directly (PipeWire too, through pipewire-pulse); `wpctl` only when there is no server |
| `ClippingRectangle` | 1 | ✅ `clip` |

And what they use from the `Quickshell` object: `env` (82) ✅ `sys.ask("env", …)`, with permission; `shellPath` (32) ✅ `sys.ask("files.folder")` and `require`, which are already relative to whoever writes them; `screens` (31) ✅ `screens: each` and `screen.name`; `execDetached` (24) ✅ `spawn`; `iconPath` (18) ✅ `image x = icon "…"`; `clipboardText` (6) ✅ `sys.ask("clipboard")`.

## 2. What is missing, ordered by how much it hurts

### 🟡 Lists: what is left

A list of five thousand already fits in sixteen copies (`content:` + `for … from`, §3.1), so the cost no longer depends on how much there is, and it is dragged by grabbing any of its rows. What is missing is **inertia** when it is let go, and copies that **are born and die on their own** instead of the scene declaring the window and the logic slicing the chunk (G12).

### 🟡 Deferred loading: `Loader`, `LazyLoader`, `Component`

128 uses. In pleamar everything is expanded at load time. For a menu that almost never opens, or a list of 200, that is work and memory for nothing.

### 🟡 What has not been tested with real hands

Exclusive keyboard, clicking outside a popup, real dragging, the wheel, and notifications and the tray in the real session (with k4 stopped). They are in note 08 as E1, E8, S11, B12 and B14.

### ⚪ The rest

A whole screen live in the scene (`ScreencopyView`; windows are done, as thumbnails, and a still of a monitor, as `capture.freeze`); IME (E7).

## 3. What pleamar has and Quickshell does not

Kept in sight here, because it is the reason this exists:

- **The renderer animates on its own.** With the logic blocked for 600 ms, 38 frames at ~17 ms; QtQuick, in the same test, a 600 ms gap. The logic cannot make the screen stutter, however badly written it is.
- **Everything is checked at load time.** A misspelled name is an error with file, line, caret and "did you mean…?", not an `undefined` at runtime.
- **A language that cannot hang:** no free loops, no recursion. What is declared always terminates.
- **Plugins with a contract:** a boundary of their own under their own name, a thread of their own, and permissions **approved by whoever uses them** (`pleamar --approve`). In Quickshell, somebody else's piece of configuration is JavaScript with every permission the person running it has.
- **Shapes that blend**, with shadow, rim and light, no layers and no tricks: it is SDF.
- **Cross-platform by design:** everything system-related behind `src/platform/`, and `./portable.sh` checks that it compiles for Windows and macOS. Workspaces and the active window go through standard protocols, not through one compositor.

## 3.1. What else came out of this

**The editor knows the language**: `pleamar --lsp` reports errors while typing, with the same compiler that reads the scene, and `--highlight` writes the syntax file from the vocabulary. Quickshell has QML's tooling, which is far older and far more complete; this is small, but it cannot fall out of date.

**A list of five thousand rows costs what sixteen cost**, and it is written in the scene: `content:` states its real size, `for … from` numbers the copies from the right place, and the scroll offset is a property a rule can take wherever it wants. In Quickshell that is `ListView`, which virtualizes on its own but brings its whole instantiation thread along with it.

**A `path` is just another shape**, not an island: `Shape` in QtQuick is a separate engine (it triangulates and paints through another render path), so it does not blend with what is around it and gets no shadow for free. Here it is the same signed distance as a circle, and `blend` merges it with whatever is next to it.

**A service is requested from the scene**, not from the logic: `service clock as now { time: text }` and the fields reach facts and texts on their own, with the compiler checking that the service brings them. In Quickshell the equivalent is a `Singleton` with its `property` and its JavaScript.

## 4. In what order

1. The language server knowing about names: completing the scene's facts and components, and going to where they are declared (G9).
2. Copies that are born and die on their own (G12), and inertia when a list is let go.
3. Plain windows, session lock, IME.
