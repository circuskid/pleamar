//! Hyprland, spoken through its two sockets: one to ask and command, the other
//! through which it tells what happens. Without launching `hyprctl` even once.

use super::SysValue;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;

fn socket(which: &str) -> Option<String> {
    let signature = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok()?;
    let base = std::env::var("XDG_RUNTIME_DIR").ok()?;
    Some(format!("{base}/hypr/{signature}/{which}"))
}

pub fn is_present() -> bool {
    socket(".socket.sock").is_some_and(|s| std::path::Path::new(&s).exists())
}

/// A question or a command, and what it answers.
pub fn request(command: &str) -> Result<String, String> {
    let path = socket(".socket.sock").ok_or("this does not look like Hyprland")?;
    let mut s = UnixStream::connect(path).map_err(|e| e.to_string())?;
    s.write_all(command.as_bytes()).map_err(|e| e.to_string())?;
    let mut r = String::new();
    s.read_to_string(&mut r).map_err(|e| e.to_string())?;
    Ok(r)
}

fn json(command: &str) -> Option<serde_json::Value> {
    serde_json::from_str(&request(command).ok()?).ok()
}

/// Where each monitor starts on Hyprland's plane, by its name.
fn monitors() -> Vec<(String, i64, i64, i64)> {
    json("j/monitors")
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|m| Some((m["name"].as_str()?.to_owned(), m["x"].as_i64()?, m["y"].as_i64()?, m["id"].as_i64()?)))
        .collect()
}

/// Where a layer of this process really is on its monitor, in logical
/// pixels: it looks it up by monitor, name (`pleamar`) and size. It's what
/// the layer protocol doesn't know when another bar pushes the others aside.
pub fn layer_position(monitor: &str, size: (u32, u32)) -> Option<(i32, i32)> {
    let me = std::process::id() as i64;
    let (_, mx, my, _) = monitors().into_iter().find(|m| m.0 == monitor)?;
    let layers = json("j/layers")?;
    let levels = layers.get(monitor)?.get("levels")?.as_object()?.clone();
    levels.values().filter_map(|n| n.as_array()).flatten().find_map(|c| {
        let is_it = c["pid"].as_i64() == Some(me) && c["namespace"].as_str() == Some("pleamar") && c["w"].as_i64() == Some(size.0 as i64) && c["h"].as_i64() == Some(size.1 as i64);
        is_it.then(|| Some(((c["x"].as_i64()? - mx) as i32, (c["y"].as_i64()? - my) as i32)))?
    })
}

/// And a window of this process: on which monitor, where inside it, and with
/// what opacity Hyprland paints it (`decoration:active_opacity` or the inactive
/// one, depending on whether it has focus): what multiplies its own when blending it.
pub fn window_position(size: (u32, u32)) -> Option<(String, (i32, i32), f32)> {
    let option = |n: &str| json(&format!("j/getoption {n}")).and_then(|v| v["float"].as_f64()).unwrap_or(1.0) as f32;
    let me = std::process::id() as i64;
    let monitors = monitors();
    json("j/clients")?.as_array()?.iter().find_map(|c| {
        let its_size = c["size"].as_array()?;
        if c["pid"].as_i64() != Some(me) || its_size.first()?.as_i64()? != size.0 as i64 || its_size.get(1)?.as_i64()? != size.1 as i64 {
            return None;
        }
        let at = c["at"].as_array()?;
        let (x, y) = (at.first()?.as_i64()?, at.get(1)?.as_i64()?);
        let m = monitors.iter().find(|m| Some(m.3) == c["monitor"].as_i64())?;
        let active = c["focusHistoryID"].as_i64() == Some(0);
        let opacity = option(if active { "decoration:active_opacity" } else { "decoration:inactive_opacity" });
        Some((m.0.clone(), ((x - m.1) as i32, (y - m.2) as i32), opacity))
    })
}

/// The mouse on the whole desktop, and the monitors as they are placed. The
/// monitors are asked again every two seconds; the mouse, every 33 ms, and it
/// is only told when it has moved.
pub fn watch_cursor(to_render: std::sync::mpsc::Sender<crate::scene::ToRender>) {
    use std::sync::atomic::Ordering;
    let _ = std::thread::Builder::new().name("hyprland·cursor".into()).spawn(move || {
        let mut last = (f32::NAN, f32::NAN);
        let mut monitors: Vec<(String, [i32; 4])> = Vec::new();
        let mut asked = std::time::Instant::now() - std::time::Duration::from_secs(10);
        loop {
            if !super::CURSOR_WANTED.load(Ordering::Relaxed) {
                last = (f32::NAN, f32::NAN);
                std::thread::sleep(std::time::Duration::from_millis(500));
                continue;
            }
            std::thread::sleep(std::time::Duration::from_millis(33));
            if asked.elapsed().as_secs() >= 2 {
                asked = std::time::Instant::now();
                monitors = json("j/monitors").and_then(|v| v.as_array().cloned()).unwrap_or_default().iter().filter_map(|m| {
                    let scale = m["scale"].as_f64().unwrap_or(1.0).max(0.1);
                    // Width and height come in real pixels; the plane is logical.
                    let (w, h) = ((m["width"].as_f64()? / scale) as i32, (m["height"].as_f64()? / scale) as i32);
                    let (w, h) = if m["transform"].as_i64().unwrap_or(0) % 2 == 1 { (h, w) } else { (w, h) };
                    Some((m["name"].as_str()?.to_owned(), [m["x"].as_i64()? as i32, m["y"].as_i64()? as i32, w, h]))
                }).collect();
            }
            let Some(v) = json("j/cursorpos") else { continue };
            let (Some(x), Some(y)) = (v["x"].as_f64(), v["y"].as_f64()) else { continue };
            let now = (x as f32, y as f32);
            if now == last {
                continue;
            }
            last = now;
            if to_render.send(crate::scene::ToRender::Cursor(now, monitors.clone())).is_err() {
                return;
            }
        }
    });
}

fn workspaces() -> SysValue {
    let active = json("j/activeworkspace").and_then(|v| v["id"].as_f64()).unwrap_or(0.0);
    // Each monitor has its own: it's what a bar per screen needs.
    let active_per_monitor: Vec<f64> = json("j/monitors")
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|m| m["activeWorkspace"]["id"].as_f64())
        .collect();
    let mut list: Vec<(i64, SysValue)> = json("j/workspaces")
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|w| {
            let id = w["id"].as_i64()?;
            // Negative ones are the special ones (the scratchpad and the like): they are not places to go to.
            (id > 0).then(|| {
                (id, SysValue::Map(vec![
                    ("id".into(), SysValue::Num(id as f64)),
                    ("name".into(), SysValue::Text(w["name"].as_str().unwrap_or("").to_owned())),
                    ("windows".into(), SysValue::Num(w["windows"].as_f64().unwrap_or(0.0))),
                    ("monitor".into(), SysValue::Text(w["monitor"].as_str().unwrap_or("").to_owned())),
                    // Whether it is the active one on its monitor, not just the one of the whole system.
                    ("active".into(), SysValue::Bool(active_per_monitor.contains(&(id as f64)))),
                ]))
            })
        })
        .collect();
    list.sort_by_key(|(id, _)| *id);
    SysValue::Map(vec![("active".into(), SysValue::Num(active)), ("list".into(), SysValue::List(list.into_iter().map(|(_, v)| v).collect()))])
}

fn window() -> SysValue {
    let v = json("j/activewindow").unwrap_or_default();
    let text = |k: &str| SysValue::Text(v[k].as_str().unwrap_or("").to_owned());
    // And which monitor has the FOCUS. Not the active window's one: a monitor
    // with no windows can also have focus, and that's exactly when a scene
    // that follows you has to go there. `j/monitors` always says it.
    let monitor = json("j/monitors")
        .and_then(|ms| ms.as_array()?.iter().find(|m| m["focused"].as_bool() == Some(true)).and_then(|m| m["name"].as_str().map(str::to_owned)))
        .unwrap_or_default();
    let (fullscreen, maximized) = fullscreen_mode(&v);
    SysValue::Map(vec![
        ("title".into(), text("title")),
        ("class".into(), text("class")),
        ("monitor".into(), SysValue::Text(monitor)),
        ("fullscreen".into(), SysValue::Bool(fullscreen)),
        ("maximized".into(), SysValue::Bool(maximized)),
    ])
}

/// Whether a window is fullscreen, and whether maximized, from its `fullscreen`
/// in `j/activewindow` (`fullscreenClient`, what the program asked for, is not
/// read). From Hyprland 0.42 it is a number: 0 none, 1 maximized, 2 fullscreen.
/// It is read as bits because in 0.42 the modes were flags, and 3 was both.
/// Up to 0.41 it was a boolean, and `fullscreenMode` said which (0 fullscreen,
/// 1 maximized). Anything else, or no window, is neither.
fn fullscreen_mode(window: &serde_json::Value) -> (bool, bool) {
    match (window["fullscreen"].as_i64(), window["fullscreen"].as_bool()) {
        (Some(n), _) => (n & 2 != 0, n & 1 != 0),
        (None, Some(true)) => if window["fullscreenMode"].as_i64() == Some(1) { (false, true) } else { (true, false) },
        _ => (false, false),
    }
}

/// Listens to what Hyprland tells and, when something of interest changes,
/// asks again and notifies. Returns whether the service exists.
pub fn service(name: &str, notify: Box<dyn Fn(SysValue) + Send>) -> bool {
    let (read, interesting): (fn() -> SysValue, &'static [&'static str]) = match name {
        "workspaces" => (workspaces, &["workspace", "createworkspace", "destroyworkspace", "openwindow", "closewindow", "movewindow", "focusedmon", "moveworkspace"]),
        "window" => (window, &["activewindow", "closewindow", "windowtitle", "focusedmon", "fullscreen"]),
        _ => return false,
    };
    let Some(path) = socket(".socket2.sock") else { return false };
    let Ok(s) = UnixStream::connect(path) else { return false };
    // News that comes in a burst is asked about once: a terminal can set its
    // title a hundred times a second (the program running changes), and each
    // one was two questions to Hyprland and a notice to the scene. And a
    // notice only goes if what it says has changed.
    let _ = s.set_read_timeout(Some(std::time::Duration::from_millis(50)));
    std::thread::Builder::new()
        .name(format!("hyprland·{name}"))
        .spawn(move || {
            let first = read();
            let mut last = format!("{first:?}");
            notify(first);
            let mut reader = BufReader::new(s);
            let mut line = String::new();
            let mut due: Option<std::time::Instant> = None;
            loop {
                // A line cut by the wait is finished on the next read, not lost.
                match reader.read_line(&mut line) {
                    Ok(0) => return,
                    Ok(_) => {
                        let event = line.split(">>").next().unwrap_or("");
                        // `workspacev2`, `activewindowv2`…: the same news twice.
                        if interesting.contains(&event) && due.is_none() {
                            due = Some(std::time::Instant::now());
                        }
                        line.clear();
                    }
                    Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                    Err(_) => return,
                }
                if due.is_some_and(|t| t.elapsed() >= std::time::Duration::from_millis(50)) {
                    due = None;
                    let v = read();
                    let fingerprint = format!("{v:?}");
                    if fingerprint != last {
                        last = fingerprint;
                        notify(v);
                    }
                }
            }
        })
        .is_ok()
}

pub fn command(name: &str, args: &[SysValue]) -> Result<(), String> {
    match (name, args) {
        // In this house Hyprland's configuration is Lua, and so are its commands.
        ("workspaces.focus", [SysValue::Num(n)]) => request(&format!("dispatch hl.dsp.focus({{ workspace = {} }})", *n as i64)).map(|_| ()),
        _ => Err(format!("I don't know how to do '{name}' with that")),
    }
}

#[cfg(test)]
mod tests {
    use super::fullscreen_mode;
    use serde_json::json;

    #[test]
    fn fullscreen_and_maximized_from_each_way_hyprland_says_it() {
        let mode = |v| fullscreen_mode(&v);
        assert_eq!(mode(json!({ "fullscreen": 0 })), (false, false));
        assert_eq!(mode(json!({ "fullscreen": 1 })), (false, true));
        assert_eq!(mode(json!({ "fullscreen": 2 })), (true, false));
        assert_eq!(mode(json!({ "fullscreen": 3 })), (true, true));
        assert_eq!(mode(json!({ "fullscreen": true, "fullscreenMode": 0 })), (true, false));
        assert_eq!(mode(json!({ "fullscreen": true, "fullscreenMode": 1 })), (false, true));
        assert_eq!(mode(json!({ "fullscreen": false, "fullscreenMode": 0 })), (false, false));
        // No window with the focus: `j/activewindow` answers `{}`.
        assert_eq!(mode(json!({})), (false, false));
    }
}
