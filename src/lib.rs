//! pleamar — prototype of a shell where animating does not depend on the logic.
//!
//! A library as well as a program: `pleamar::run()` is the whole program, and
//! what builds on it —a compositor that fills the scene's `windows`— hands
//! itself over before calling it (`provide_windows`).
//!
//! Three threads: this one, which the platform keeps (the windows of each
//! monitor, their scale and the mouse); the logic one, which decides; and the render one, which is the
//! only one that animates.

pub mod scene;
mod scenes;
mod skill;
mod shaders;
mod blur;
mod shapes;
mod lsp;
mod luau_types;
pub mod gpu;
mod agent;
#[cfg(target_os = "linux")]
pub mod dmabuf;
mod lens;
mod markdown;
mod language;
mod logic;
#[cfg(feature = "luau")]
mod logic_luau;
mod permissions;
mod platform;
mod probe;
mod report;
mod render;

pub use platform::{host_keymap, provide_layer_hooks, provide_platform, provide_windows, set_host_keymap, LayerHooks, NestSender, Platform, PlatformWindow};
pub use gpu::{Frames, NewSheet, Sent, Target, View};
/// The same wgpu the render paints with, for a platform that lends it textures.
pub use wgpu;
pub use scenes::from_file::read as read_scene;
mod text;

use scene::*;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const HELP: &str = "pleamar [options]
  --scene FILE        the scene to open (.plm); it reloads itself when you save it. The test
                      benches written in Rust also work: marea, island, face, showcase, swarm
  --check FILE        reads a scene, says whether it is fine, and exits
  --approve SCENE     shows what the plugins of a scene ask for, and asks whether to approve them
                      (with --yes after it, it does not ask). Unapproved, a plugin runs touching nothing
  --grammar           the words the language accepts, exactly as the compiler consults them
  --luau-types [SCENE] a definitions file for luau-lsp: the globals a logic is given, typed, and with
                      a scene its own facts, texts, models and events, so an editor completes them
  --docs [TOPIC]      the documentation of this version: reference, guide, recipes, logic, measuring
  --install-skill     teaches the AI agents installed (Claude Code, Codex, OpenCode) to build with
                      pleamar: writes its skill for each. It refreshes itself when pleamar updates
  --autostart         starts what ~/.config/pleamar/autostart says (your shells), one command a
                      line, and exits: `exec-once = pleamar --autostart` on Hyprland. Lines
                      that start with `wm:` are for pleamar-wm's own session and are skipped
  --lsp               a language server on stdio: mistakes as you type, what fits here, and what
                      each word means. For any editor that speaks LSP
  --highlight EDITOR  writes the syntax file for 'vim' or 'vscode', made from the vocabulary
  --version           the version of the program and of the language it understands
  --report [SCENES]   measures the scenes running (all of them, or those named) for a while
                      —use the desktop as usual meanwhile— and writes what it saw, with the
                      machine's details, to a file to send us when something stutters.
                      «--seconds N» (30), «--out FILE»
  --say [SCENE] CMD…  says something to a running scene and exits; several commands, in order. Commands:
                      «emit event [n]», «fact name value», «text name whatever it says», «submit input what»
                      (as if typed there and Enter pressed), «focus input», «get name» (answers), «quit»
  --screen NAMES      «all», or monitors separated by commas (by default, whatever the scene asks for).
                      A repeated name gives two surfaces on the same monitor.
  --stall MS          how long the logic blocks after every decision (600)
  --naive             the logic blocks the painting thread, as in QtQuick
  --demo              opens and closes by itself, with no mouse
  --mouse SCRIPT      fake mouse: «360,90@1000 click@2500 down@… up@… wheel+@… out@4000» (ms);
                      and keys: «key:Escape@1200 keydown:Left@1500 keyup:Left@2500 type:hello@3000»
  --seconds N         exits by itself after N seconds
  --margin PX         top margin, instead of the scene\u{2019}s
  --no-hud            without the frame graph
  --record NAMES      prints what those properties, facts or texts are worth on every
                      frame, as «ms<tab>value…», to check an animation against its
                      contract: «--record lid,body.y --seconds 20 > log.tsv»
  --no-vsync          paint without waiting for the screen, to measure what a frame costs
  --reduced-motion    springs settle at once and gestures show their still face
Right-click closes it, if the scene does not use the right button for anything.";

struct Args {
    scene: String,
    screen: Option<String>,
    stall: u64,
    naive: bool,
    demo: bool,
    mouse: Option<String>,
    seconds: Option<f64>,
    margin: Option<i32>,
    hud: bool,
    reduced: bool,
    no_vsync: bool,
    /// Which properties, facts or texts to record on every frame.
    record: Vec<String>,
}

fn args(given: Vec<String>) -> Args {
    let mut a = Args { scene: String::new(), screen: None, stall: 600, naive: false, demo: false, mouse: None, seconds: None, margin: None, hud: true, reduced: false, no_vsync: false, record: Vec::new() };
    let mut it = given.into_iter();
    while let Some(op) = it.next() {
        let mut value = || it.next().unwrap_or_else(|| { eprintln!("{HELP}"); std::process::exit(2) });
        match op.as_str() {
            "--scene" => a.scene = value(),
            "--say" => {
                // Several commands after the scene go down one connection, one line each, and
                // the scene runs them in that order: a popup's texts are in place before its emit.
                let (a1, rest) = (value(), it.by_ref().collect::<Vec<String>>());
                let r = match rest.is_empty() {
                    false => platform::send(Some(&a1), &rest),
                    true => platform::send(None, std::slice::from_ref(&a1)),
                };
                std::process::exit(match r {
                    Ok(()) => 0,
                    Err(e) => {
                        eprintln!("{e}");
                        1
                    }
                });
            }
            "--report" => {
                let rest: Vec<String> = it.by_ref().collect();
                std::process::exit(report(rest, None));
            }
            "--version" => {
                println!("pleamar {} · language {}.{}", env!("CARGO_PKG_VERSION"), language::VERSION.0, language::VERSION.1);
                std::process::exit(0);
            }
            "--approve" => {
                let scene = value();
                let yes_to_all = std::env::args().any(|a| a == "--yes");
                std::process::exit(permissions::ask(&scene, yes_to_all));
            }
            "--grammar" => {
                print!("{}", language::vocabulary::to_text());
                std::process::exit(0);
            }
            "--autostart" => std::process::exit(autostart()),
            "--docs" => {
                let topic = it.next().unwrap_or_default();
                std::process::exit(skill::docs(&topic));
            }
            "--install-skill" => std::process::exit(skill::install(true)),
            // The editor: mistakes while you type, and the highlighting.
            "--lsp" => {
                lsp::serve();
                std::process::exit(0);
            }
            "--highlight" => std::process::exit(lsp::highlighting(&value())),
            // The logic's editor: what its globals are (`pleamar --luau-types marea.plm > pleamar.d.luau`).
            "--luau-types" => {
                let path = it.next().filter(|p| !p.starts_with("--"));
                let scene = match path.as_deref().map(scenes::from_file::read) {
                    Some(Ok(e)) => Some(e),
                    Some(Err(m)) => {
                        eprintln!("{m}");
                        std::process::exit(1);
                    }
                    None => None,
                };
                print!("{}", luau_types::definitions(scene.as_ref()));
                std::process::exit(0);
            }
            "--check" => {
                let path = value();
                std::process::exit(match scenes::from_file::read(&path) {
                    Ok(e) => {
                        println!("{path}: ok · {} properties, {} instructions, {} layers, {} rules, {} zones, {} gestures", e.props.len(), e.instrs.len(), e.layers.len(), e.rules.len(), e.zones.len(), e.gestures.len());
                        0
                    }
                    Err(m) => {
                        eprintln!("{m}");
                        1
                    }
                });
            }
            "--screen" => a.screen = Some(value()),
            "--stall" => a.stall = value().parse().expect("--stall wants milliseconds"),
            "--mouse" => a.mouse = Some(value()),
            "--seconds" => a.seconds = Some(value().parse::<f64>().ok().filter(|s| *s > 0.0).expect("--seconds wants a number of seconds, like 8 or 2.5")),
            "--margin" => a.margin = Some(value().parse().expect("--margin wants pixels")),
            "--naive" => a.naive = true,
            "--demo" => a.demo = true,
            "--no-hud" => a.hud = false,
            "--reduced-motion" => a.reduced = true,
            "--no-vsync" => a.no_vsync = true,
            "--record" => a.record = value().split(',').map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).collect(),
            _ => { eprintln!("{HELP}"); std::process::exit(2) }
        }
    }
    if a.scene.is_empty() {
        eprintln!("{HELP}");
        std::process::exit(2);
    }
    a
}

/// `pleamar --report`: see `report.rs`. `extra` is what a program built on
/// pleamar adds about itself (pleamar-wm: its monitors and its configuration).
pub fn report(args: Vec<String>, extra: Option<String>) -> i32 {
    report::run(args, extra)
}

/// Not starting again when the program changes on disk (see `watch_binary`):
/// for a compositor, starting again closes every window it holds. Only this
/// process; the programs it starts still reload as ever.
/// What the programs a scene's logic starts must know about the desktop the
/// scene is (pleamar-wm says where its windows connect, `WAYLAND_DISPLAY`, and
/// its X11 display): the process itself may have been started without them,
/// and a program run from the logic —a screenshot, an app from a launcher—
/// would look for another desktop, or none. `None` removes the variable.
pub fn set_child_env(name: &str, value: Option<&str>) {
    let mut vars = CHILD_ENV.lock().unwrap();
    vars.retain(|(n, _)| n != name);
    vars.push((name.to_owned(), value.map(str::to_owned)));
}

static CHILD_ENV: std::sync::Mutex<Vec<(String, Option<String>)>> = std::sync::Mutex::new(Vec::new());

/// Gives a program what `set_child_env` said (before its own variables, which win).
pub(crate) fn child_env(command: &mut std::process::Command) {
    for (name, value) in CHILD_ENV.lock().unwrap().iter() {
        match value {
            Some(v) => {
                command.env(name, v);
            }
            None => {
                command.env_remove(name);
            }
        }
    }
}

pub fn stay_on_update() {
    scenes::STAY_ON_UPDATE.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// The program: reads the command line and runs what it asks for.
pub fn run() {
    run_with(std::env::args().skip(1).collect());
}

/// The same, with the options given instead of the command line's.
pub fn run_with(options: Vec<String>) {
    let start_time = std::time::Instant::now();
    let a = args(options);
    // No terminal to be stopped on, for the programs it starts.
    platform::leave_terminal();
    // The agents' skill, written again if this pleamar is not the one it speaks of.
    skill::refresh_quietly();
    let blocked = Arc::new(AtomicBool::new(false));
    let (to_render, from_render) = channel();
    // The thumbnails service's frames on the card, summed by the render.
    #[cfg(target_os = "linux")]
    {
        let tx = std::sync::Mutex::new(to_render.clone());
        platform::thumbnail_on_check(Box::new(move || {
            let _ = tx.lock().unwrap().send(scene::ToRender::Thumbnails);
        }));
    }
    let (to_logic, from_logic) = channel();
    let mut script: Box<dyn logic::Script> = match a.scene.as_str() {
        "marea" => Box::<scenes::marea::Marea>::default(),
        "island" => Box::<scenes::island::Island>::default(),
        "face" => Box::<scenes::face::Face>::default(),
        "showcase" => Box::<scenes::showcase::Showcase>::default(),
        "swarm" => Box::<scenes::swarm::Swarm>::default(),
        path if std::path::Path::new(path).is_file() => scenes::from_file::script_for(path, to_render.clone(), to_logic.clone(), blocked.clone()),
        other => {
            eprintln!("I don't know the scene '{other}', and it is not a file\n{HELP}");
            std::process::exit(2)
        }
    };
    // The scene says which surface it wants; the command line can overrule
    // it.
    let mut scene = script.scene();
    if let Some(p) = &a.screen {
        let which: Vec<String> = p.split(',').map(str::to_owned).collect();
        let per_monitor = matches!(scene.surface().screens, Screens::Number(_));
        // A surface per monitor (`screens: each`) gets the list handed out: the
        // first copy to the first name, and so on. It is how two monitors are rehearsed.
        for s in &mut scene.surfaces {
            if let Screens::Number(k) = s.screens {
                s.screens = match which.get(k) {
                    _ if p == "all" => Screens::Number(k),
                    Some(name) => Screens::Named(vec![name.clone()]),
                    // More copies than monitors asked for: that one does not appear.
                    None => Screens::Named(Vec::new()),
                };
            }
        }
        // The main one, unless it is already one of the per-monitor copies: that one has already been handed out.
        if !per_monitor || p == "all" {
            scene.surface_mut().screens = if p == "all" { Screens::All } else { Screens::Named(which) };
        }
    }
    if let Some(m) = a.margin {
        scene.surface_mut().margin[0] = m;
    }

    // The text and image workshop. The first thing it does is read the system's
    // fonts, which is the slowest part of the startup: let it get going.
    let letters = text::Texts::open(to_render.clone());
    // Without wgpu's checks: in a bar that repaints 60 times a second, what is
    // saved per frame shows in the power draw. To debug, `PLEAMAR_VALIDATE=1`.
    let mut flags = wgpu::InstanceFlags::empty();
    if std::env::var_os("PLEAMAR_VALIDATE").is_some() {
        flags = wgpu::InstanceFlags::VALIDATION | wgpu::InstanceFlags::DEBUG;
    }
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor { backends: wgpu::Backends::PRIMARY, flags, ..wgpu::InstanceDescriptor::new_without_display_handle() });
    let wanted = scene.surfaces.clone();

    println!(
        "pleamar · {} mode · the logic blocks {} ms after every decision",
        if a.naive { "NAIVE (single thread)" } else { "decoupled (independent render)" },
        a.stall
    );
    let _ = to_render.send(ToRender::Scene(scene));
    crate::platform::watch_cursor(to_render.clone());
    if std::path::Path::new(&a.scene).is_file() {
        scenes::from_file::watch(a.scene.clone(), to_render.clone(), to_logic.clone());
    }
    let to_logic_for_commands = to_logic.clone();
    let render = {
        let blocked = blocked.clone();
        let op = render::Options { hud: a.hud, naive: a.naive, reduced_motion: a.reduced, no_vsync: a.no_vsync, trace: a.record.clone(), start_time, to_self: to_render.clone() };
        let instance = instance.clone();
        std::thread::Builder::new()
            .name("render".into())
            .spawn(move || {
                // A render that panics leaves nothing on screen that moves, while the
                // rest goes on: a lock screen stayed locked over a frozen picture
                // (wgpu panicked when the card stalled on a monitor's wake). Better
                // to end the whole process, so whatever keeps it running starts it
                // again; the panic has already said why.
                let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| render::run(instance, from_render, letters, to_logic, blocked, op)));
                if run.is_err() {
                    eprintln!("render · it panicked: leaving, so that it can be started again");
                    std::process::exit(70);
                }
                RENDER_DONE.store(true, std::sync::atomic::Ordering::SeqCst);
            })
            .unwrap()
    };
    {
        let op = logic::Options { block_for: Duration::from_millis(a.stall), demo: a.demo, naive: a.naive, echo: a.mouse.is_some() };
        let tx = to_render.clone();
        std::thread::Builder::new()
            .name("logic".into())
            .spawn(move || logic::run(script, from_logic, tx, blocked, op))
            .unwrap();
    }

    // What is said from outside —`pleamar --say "emit toggle"`, which is what
    // a global compositor shortcut runs— comes in as if the logic had said it.
    {
        let name = std::path::Path::new(&a.scene).file_stem().map_or(a.scene.clone(), |n| n.to_string_lossy().into_owned());
        let tx = Mutex::new((to_render.clone(), to_logic_for_commands));
        let me = name.clone();
        platform::listen_for_commands(&name, std::sync::Arc::new(move |line: String, out: &mut dyn FnMut(&str) -> bool| {
            // Copies, and the lock let go at once: a `wait` that lasts does not hold up the others.
            let (tx, to_logic) = {
                let guard = tx.lock().unwrap();
                (guard.0.clone(), guard.1.clone())
            };
            let (tx, to_logic) = (&tx, &to_logic);
            let mut p = line.trim().splitn(3, ' ');
            let (what, who, rest) = (p.next().unwrap_or(""), p.next().unwrap_or(""), p.next().unwrap_or(""));
            let after = line.trim().split_once(' ').map_or("", |x| x.1);
            // Who it is: what a window manager asks to know which window is which scene.
            if what == "hello" {
                return Some(format!("pleamar {} · scene {me} · pid {} · language {}.{}", env!("CARGO_PKG_VERSION"), std::process::id(), language::VERSION.0, language::VERSION.1));
            }
            // `wait saving == false 3s`: answered as soon as it holds, or when it is late.
            if what == "wait" {
                let (question, answer) = std::sync::mpsc::channel();
                let _ = tx.send(ToRender::Wait(after.to_owned(), question));
                return Some(answer.recv_timeout(std::time::Duration::from_secs(62)).unwrap_or_else(|_| "? the render does not answer\n".into()));
            }
            // `watch [10s]`: a line for each thing that happens, for that long (ten seconds if unsaid).
            if what == "watch" {
                let secs = after.trim().trim_end_matches('s').parse::<f32>().ok().filter(|s| *s > 0.0 && *s <= 3600.0).unwrap_or(10.0);
                let (lines, heard) = std::sync::mpsc::channel();
                let until = std::time::Instant::now() + std::time::Duration::from_secs_f32(secs);
                let _ = tx.send(ToRender::Watch(lines, until));
                while let Ok(l) = heard.recv_timeout(until.saturating_duration_since(std::time::Instant::now()) + std::time::Duration::from_millis(200)) {
                    if !out(&l) {
                        break;
                    }
                }
                return None;
            }
            // `pleamar --report`: measuring starts, and later its report is asked for.
            if what == "probe" {
                if who == "report" {
                    let (question, answer) = std::sync::mpsc::channel();
                    let _ = tx.send(ToRender::Probe(Some(question)));
                    return Some(answer.recv_timeout(std::time::Duration::from_secs(3)).unwrap_or_else(|_| "? the render does not answer".into()));
                }
                let _ = tx.send(ToRender::Probe(None));
                return Some("measuring\n".into());
            }
            // `describe`: what there is to read and touch, by name. `describe json`, as data.
            if what == "describe" {
                let (question, answer) = std::sync::mpsc::channel();
                let _ = tx.send(ToRender::Describe(who == "json", question));
                return Some(answer.recv_timeout(std::time::Duration::from_secs(2)).unwrap_or_else(|_| "? the render does not answer".into()));
            }
            // `press save`, `type query words`…: by name, as a hand would. It answers with what happened.
            if let Some(act) = crate::agent::Act::parse(what, after) {
                return Some(match act {
                    Err(m) => format!("? {m}\n"),
                    Ok(act) => {
                        let (question, answer) = std::sync::mpsc::channel();
                        let _ = tx.send(ToRender::Act(act, question));
                        answer.recv_timeout(std::time::Duration::from_secs(8)).unwrap_or_else(|_| "? the render does not answer\n".into())
                    }
                });
            }
            if what == "get" {
                let (question, answer) = std::sync::mpsc::channel();
                let _ = tx.send(ToRender::Query(scene::intern(who), question));
                return Some(answer.recv_timeout(std::time::Duration::from_secs(1)).unwrap_or_else(|_| "? the render does not answer".into()));
            }
            let _ = match what {
                "emit" => tx.send(ToRender::ExternalSignal(scene::intern(who), rest.parse().ok())),
                // What is set from outside, the logic has to know: it did not set it itself.
                // What is written is understood according to what that fact is: `true`, `critical`, `3`.
                "fact" => tx.send(ToRender::ExternalFact(scene::intern(who), rest.to_owned())),
                "text" => {
                    let _ = to_logic.send(Event::Text(scene::intern(who), rest.to_owned()));
                    tx.send(ToRender::Text(scene::intern(who), rest.to_owned()))
                }
                "focus" => tx.send(ToRender::FocusField(Some(scene::intern(who)))),
                // As if it had been typed into that field and Enter pressed: the
                // logic hears the text and then the submit, in that order.
                "submit" => {
                    let _ = to_logic.send(Event::Text(scene::intern(who), rest.to_owned()));
                    let _ = to_logic.send(Event::Submit(scene::intern(who), rest.to_owned()));
                    tx.send(ToRender::Text(scene::intern(who), rest.to_owned()))
                }
                // Through the render, as `--seconds` does: leaving while it still
                // works with the card crashed the process on its way out.
                "quit" => quit_after_render(tx),
                _ => {
                    eprintln!("orders · I don't understand '{line}'");
                    return Some(format!("? I don't understand '{}': emit, fact, text, submit, focus, get, describe, press, hold, drag, wheel, type, key, wait, watch, hello, probe, quit", line.trim()));
                }
            };
            None
        }));
    }
    if let Some(steps) = a.mouse.clone() {
        // To rehearse the zones without taking the mouse away from anyone.
        let tx = to_render.clone();
        std::thread::spawn(move || {
            let start = std::time::Instant::now();
            for step in steps.split_whitespace() {
                let (what, when) = step.split_once('@').expect("--mouse: @ms is missing");
                let when = Duration::from_millis(when.parse().expect("--mouse: ms"));
                std::thread::sleep(when.saturating_sub(start.elapsed()));
                // Each step is announced, to know what whatever comes after is responding to.
                println!("mouse  · {what}");
                let _ = match what {
                    // A whole click: down and up.
                    "click" => tx.send(ToRender::Button(0, true)).and_then(|_| tx.send(ToRender::Button(0, false))),
                    "down" => tx.send(ToRender::Button(0, true)),
                    "up" => tx.send(ToRender::Button(0, false)),
                    "right" => tx.send(ToRender::Button(1, true)).and_then(|_| tx.send(ToRender::Button(1, false))),
                    // `key:Escape`, `key:Ctrl+a`: down and up.
                    t if t.starts_with("key:") => {
                        let mut m = Mods::default();
                        let mut name = &t[4..];
                        for (prefix, sets) in [("Ctrl+", 0), ("Alt+", 1), ("Shift+", 2), ("Super+", 3)] {
                            if let Some(rest) = name.strip_prefix(prefix) {
                                name = rest;
                                match sets {
                                    0 => m.ctrl = true,
                                    1 => m.alt = true,
                                    2 => m.shift = true,
                                    _ => m.logo = true,
                                }
                            }
                        }
                        tx.send(ToRender::Key(name.to_owned(), None, m, 0)).and_then(|_| tx.send(ToRender::KeyReleased(name.to_owned(), 0)))
                    }
                    // `keydown:Left`, `keyup:Left`: a key held, and let go later.
                    t if t.starts_with("keydown:") => tx.send(ToRender::Key(t[8..].to_owned(), None, Mods::default(), 0)),
                    t if t.starts_with("keyup:") => tx.send(ToRender::KeyReleased(t[6..].to_owned(), 0)),
                    // `type:hello`: letter by letter, like a keyboard. A `_` is a space.
                    t if t.starts_with("type:") => {
                        for ch in t[5..].chars() {
                            let ch = if ch == '_' { ' ' } else { ch };
                            let _ = tx.send(ToRender::Key(ch.to_string(), Some(ch.to_string()), Mods::default(), 0));
                            let _ = tx.send(ToRender::KeyReleased(ch.to_string(), 0));
                        }
                        Ok(())
                    }
                    // `code:30`: a key by its evdev code, pressed and released, as
                    // a real keyboard would —what reaches a window of `windows`—.
                    t if t.starts_with("code:") => match t[5..].parse::<u32>() {
                        Ok(code) => tx.send(ToRender::Key(format!("{code:#x}"), None, Mods::default(), code)).and_then(|_| tx.send(ToRender::KeyReleased(format!("{code:#x}"), code))),
                        Err(_) => Ok(()),
                    },
                    "focus+" => tx.send(ToRender::KeyboardFocus(true)),
                    "focus-" => tx.send(ToRender::KeyboardFocus(false)),
                    t if t.starts_with("drop:") => tx.send(ToRender::Dropped("text/plain".into(), t[5..].to_owned())),
                    "wheel+" => tx.send(ToRender::Wheel(1.0)),
                    "wheel-" => tx.send(ToRender::Wheel(-1.0)),
                    "out" => tx.send(ToRender::Pointer(None)),
                    xy => {
                        let (x, y) = xy.split_once(',').expect("--mouse: x,y");
                        tx.send(ToRender::Pointer(Some((x.parse().unwrap(), y.parse().unwrap()))))
                    }
                };
            }
        });
    }
    if let Some(s) = a.seconds {
        let tx = to_render.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs_f64(s));
            // Quitting goes through the render so that it closes its last measurement cycle.
            quit_after_render(&tx);
        });
    }
    #[cfg(unix)]
    quit_on_signals(to_render.clone());

    // From here on this thread belongs to the platform: it puts up the windows and attends
    // to the system until someone closes.
    let extra_height = if a.hud { gpu::HUD_HEIGHT as u32 } else { 0 };
    match platform::provided_platform() {
        Some(p) => p.run(wanted, extra_height, instance, to_render.clone()),
        None => platform::run_event_loop(wanted, extra_height, instance, to_render.clone()),
    }
    let _ = to_render.send(ToRender::Quit);
    let _ = render.join();
    quit();
}

/// The process goes away whole —the destruction order does not deserve code in a
/// prototype—, but not without first stopping what the logic left running, nor
/// what a platform handed over still has working with the card (`provide_before_quit`).
/// Quitting goes through the render, and waits until it has let the card go (a few
/// seconds at most): leaving while it still works with it brought the driver down with it.
fn quit_after_render(tx: &std::sync::mpsc::Sender<ToRender>) -> ! {
    let _ = tx.send(ToRender::Quit);
    let asked = std::time::Instant::now();
    while !RENDER_DONE.load(std::sync::atomic::Ordering::SeqCst) && asked.elapsed() < Duration::from_secs(3) {
        std::thread::sleep(Duration::from_millis(10));
    }
    quit()
}

fn quit() -> ! {
    if let Some(f) = BEFORE_QUIT.lock().unwrap().take() {
        f();
    }
    #[cfg(feature = "luau")]
    logic_luau::stop_children();
    std::process::exit(0)
}

/// `kill` (SIGTERM), Ctrl+C at its terminal (SIGINT) or that terminal going
/// away (SIGHUP): out the same way as `--say … quit`, through the render and
/// stopping what the logic started, which a plain kill left running. A second
/// one while it is on its way out ends it at once.
#[cfg(unix)]
fn quit_on_signals(tx: std::sync::mpsc::Sender<ToRender>) {
    use std::sync::atomic::{AtomicI32, Ordering};
    static PIPE: AtomicI32 = AtomicI32::new(-1);
    static HEARD: AtomicBool = AtomicBool::new(false);
    // Only what a signal handler may do: a byte into a pipe, or leave.
    extern "C" fn heard(_: libc::c_int) {
        if HEARD.swap(true, Ordering::SeqCst) {
            // SAFETY: _exit is async-signal-safe.
            unsafe { libc::_exit(1) }
        }
        let fd = PIPE.load(Ordering::SeqCst);
        let byte = 1u8;
        // SAFETY: write is async-signal-safe, and the pipe lives as long as the process.
        unsafe { libc::write(fd, (&byte as *const u8).cast(), 1) };
    }
    let mut fds = [0; 2];
    // SAFETY: two descriptors for the call to fill.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return;
    }
    PIPE.store(fds[1], Ordering::SeqCst);
    let _ = std::thread::Builder::new().name("signals".into()).spawn(move || {
        let mut byte = 0u8;
        // SAFETY: reading one byte from our own pipe.
        while unsafe { libc::read(fds[0], (&mut byte as *mut u8).cast(), 1) } < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {}
        quit_after_render(&tx);
    });
    for s in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        // SAFETY: a handler that only does what a handler may.
        unsafe { libc::signal(s, heard as *const () as libc::sighandler_t) };
    }
}

/// Whether the render has finished: nothing of it touches the card any more.
static RENDER_DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static BEFORE_QUIT: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>> = std::sync::Mutex::new(None);

/// What to do right before the process goes away: a platform with threads of
/// its own working with the card stops them there.
pub fn provide_before_quit(f: Box<dyn FnOnce() + Send>) {
    *BEFORE_QUIT.lock().unwrap() = Some(f);
}

/// The user's pleamar folder, the one for their dotfiles: `PLEAMAR_CONFIG`, or
/// `$XDG_CONFIG_HOME/pleamar` (`~/.config/pleamar`).
pub fn config_dir() -> Option<std::path::PathBuf> {
    if let Some(d) = std::env::var_os("PLEAMAR_CONFIG").filter(|v| !v.is_empty()) {
        return Some(d.into());
    }
    let base = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()).map(std::path::PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))?;
    Some(base.join("pleamar"))
}

/// `pleamar --autostart`: what `autostart` says, each on its own and let go
/// —they outlive this—, except what is only for pleamar-wm's session (`wm:`).
fn autostart() -> i32 {
    let Some(file) = config_dir().map(|d| d.join("autostart")) else { return 1 };
    let Ok(text) = std::fs::read_to_string(&file) else {
        eprintln!("autostart · there is no {}", file.display());
        return 1;
    };
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with("wm:")) {
        let mut c = std::process::Command::new("sh");
        c.arg("-c").arg(line).stdin(std::process::Stdio::null());
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut c, 0);
        match c.spawn() {
            Ok(_) => println!("autostart · {line}"),
            Err(e) => eprintln!("autostart · «{line}» could not start: {e}"),
        }
    }
    0
}
