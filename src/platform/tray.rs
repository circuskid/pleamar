//! The system tray: the icons left by applications that stay alive without a
//! window (StatusNotifierItem, the KDE agreement everyone uses by now).
//!
//! There are two roles. The **watcher** (`StatusNotifierWatcher`) is where
//! applications register, and there can only be one. The **host** is whoever
//! shows the icons, and there can be several. If nobody is the watcher, we are;
//! if someone else already is (another bar), we ask it: the tray shows up all the same.
//!
//! `{ { key, id, title, status, icon, menu, tooltip, tooltip_body }, … }`. `icon`
//! is an icon name or a path: it works as is for `image … = from`. `status`:
//! Active, Passive, NeedsAttention. `tooltip` and `tooltip_body` are what the
//! application says on hover, `""` when it says nothing. On Windows it will be
//! `Shell_NotifyIcon`; on macOS, `NSStatusItem` does not let you see other apps' ones.

use super::SysValue;
use std::hash::{Hash, Hasher};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use zbus::blocking::{Connection, MessageIterator, Proxy};
use zbus::proxy::CacheProperties;
use zbus::MatchRule;

const WATCHER: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";
const ITEM: &str = "org.kde.StatusNotifierItem";

/// The registered ones, when we are the watcher: "service/path".
#[derive(Default)]
struct Registry {
    items: Vec<String>,
    /// Whom to tell that it is time to look again, and what signal to give.
    updates: Option<Sender<Update>>,
}

enum Update {
    Rescan,
    Registered(String),
    /// A new listener: tell it everything.
    Listener,
}

struct Watcher(Arc<Mutex<Registry>>);

#[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
impl Watcher {
    fn register_status_notifier_item(&self, service: &str, #[zbus(header)] header: zbus::message::Header<'_>) {
        // Some give their bus name; others, only the object path, and the name is whoever is calling.
        let caller = header.sender().map(|s| s.to_string()).unwrap_or_default();
        let full = if service.starts_with('/') { format!("{caller}{service}") } else { format!("{service}/StatusNotifierItem") };
        let mut a = self.0.lock().unwrap();
        if !a.items.contains(&full) {
            a.items.push(full.clone());
            if let Some(tx) = &a.updates {
                let _ = tx.send(Update::Registered(full));
            }
        }
    }

    fn register_status_notifier_host(&self, _service: &str) {}

    #[zbus(property)]
    fn registered_status_notifier_items(&self) -> Vec<String> {
        self.0.lock().unwrap().items.clone()
    }

    #[zbus(property)]
    fn is_status_notifier_host_registered(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn protocol_version(&self) -> i32 {
        0
    }
}

static CONNECTION: OnceLock<Connection> = OnceLock::new();
/// Who hears the icons, and how to ask for a fresh look: the service is one per
/// process and outlives the logic, so a reloaded logic takes the listener's place
/// here instead of opening another connection that finds the watcher's name taken.
static LISTENER: Mutex<Option<Box<dyn Fn(SysValue) + Send>>> = Mutex::new(None);
static RESCAN: OnceLock<Mutex<std::sync::mpsc::Sender<Update>>> = OnceLock::new();

fn split(full: &str) -> (&str, String) {
    match full.find('/') {
        Some(k) => (&full[..k], full[k..].to_owned()),
        None => (full, "/StatusNotifierItem".to_owned()),
    }
}

fn element(c: &Connection, full: &str) -> Option<Proxy<'static>> {
    let (service, path) = split(full);
    // No cache: these do not announce with `PropertiesChanged`, but with `NewIcon` and friends.
    zbus::blocking::proxy::Builder::new(c).destination(service.to_owned()).ok()?.path(path).ok()?.interface(ITEM).ok()?.cache_properties(CacheProperties::No).build().ok()
}

/// Many —Electron, Telegram— do not give an icon's name but its pixels.
/// They are saved as PNG, named after what they contain: if the icon
/// changes, the path changes, and whoever paints it notices.
fn from_pixels(pixmaps: Vec<(i32, i32, Vec<u8>)>) -> Option<String> {
    // The biggest one there is: it gets scaled down later to whatever the scene asks for.
    let (w, h, argb) = pixmaps.into_iter().filter(|(w, h, d)| *w > 0 && *h > 0 && d.len() == (*w * *h * 4) as usize).max_by_key(|(w, _, _)| *w)?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    argb.hash(&mut hasher);
    let base = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().into_owned());
    let dir = std::path::Path::new(&base).join("pleamar").join("tray");
    let path = dir.join(format!("{:016x}.png", hasher.finish()));
    if !path.exists() {
        std::fs::create_dir_all(&dir).ok()?;
        // They arrive as ARGB, with the high byte first.
        let rgba: Vec<u8> = argb.chunks_exact(4).flat_map(|p| [p[1], p[2], p[3], p[0]]).collect();
        image::RgbaImage::from_raw(w as u32, h as u32, rgba)?.save(&path).ok()?;
    }
    Some(path.to_string_lossy().into_owned())
}

fn icon_of(p: &Proxy) -> String {
    let name: String = p.get_property("IconName").unwrap_or_default();
    if !name.is_empty() {
        // Some bring their own icon folder, outside the theme.
        let dir: String = p.get_property("IconThemePath").unwrap_or_default();
        if !dir.is_empty() {
            for ext in ["svg", "png"] {
                let path = std::path::Path::new(&dir).join(format!("{name}.{ext}"));
                if path.exists() {
                    return path.to_string_lossy().into_owned();
                }
            }
        }
        return name;
    }
    p.get_property::<Vec<(i32, i32, Vec<u8>)>>("IconPixmap").ok().and_then(from_pixels).unwrap_or_default()
}

fn list(c: &Connection, own: Option<&Mutex<Registry>>) -> Vec<String> {
    match own {
        Some(a) => a.lock().unwrap().items.clone(),
        None => Proxy::new(c, WATCHER, WATCHER_PATH, WATCHER).ok().and_then(|p| p.get_property::<Vec<String>>("RegisteredStatusNotifierItems").ok()).unwrap_or_default(),
    }
}

/// A tooltip's description may carry a little HTML (`<b>`, `<i>`, `<br>`,
/// `<a href>`, `<img>`), and a text paints none of it: the words stay, a
/// `<br>` is a new line, and the rest goes, as with a notification's body.
fn plain(markup: &str) -> String {
    let mut lines = String::with_capacity(markup.len());
    let mut rest = markup;
    while let Some(k) = rest.find('<') {
        let end = rest[k..].find('>').map_or(rest.len(), |e| k + e + 1);
        let tag = &rest[k..end];
        let name: String = tag[1..].trim_start_matches('/').chars().take_while(char::is_ascii_alphabetic).collect();
        lines.push_str(&rest[..k]);
        lines.push_str(if name.eq_ignore_ascii_case("br") { "\n" } else { tag });
        rest = &rest[end..];
    }
    lines.push_str(rest);
    super::notifications::strip_markup(&lines).trim().to_owned()
}

fn report(c: &Connection, own: Option<&Mutex<Registry>>) -> SysValue {
    let mut gone = Vec::new();
    let v = list(c, own).into_iter().filter_map(|full| {
        let p = element(c, &full)?;
        // If not even its name answers, the application has gone away.
        let Ok(id) = p.get_property::<String>("Id") else {
            gone.push(full);
            return None;
        };
        let title: String = p.get_property("Title").unwrap_or_default();
        let menu = p.get_property::<zbus::zvariant::OwnedObjectPath>("Menu").is_ok_and(|m| m.as_str() != "/" && !m.as_str().is_empty());
        // (icon name, icon pixels, title, description): only the words are taken.
        let (_, _, tip, tip_body) = p.get_property::<(String, Vec<(i32, i32, Vec<u8>)>, String, String)>("ToolTip").unwrap_or_default();
        Some(SysValue::Map(vec![
            ("key".into(), SysValue::Text(full)),
            ("title".into(), SysValue::Text(if title.is_empty() { id.clone() } else { title })),
            ("id".into(), SysValue::Text(id)),
            ("status".into(), SysValue::Text(p.get_property("Status").unwrap_or_else(|_| "Active".to_owned()))),
            ("icon".into(), SysValue::Text(icon_of(&p))),
            ("menu".into(), SysValue::Bool(menu)),
            ("tooltip".into(), SysValue::Text(tip)),
            ("tooltip_body".into(), SysValue::Text(plain(&tip_body))),
        ]))
    }).collect();
    if let Some(a) = own {
        a.lock().unwrap().items.retain(|e| !gone.contains(e));
    }
    SysValue::List(v)
}

pub fn service(dispatch: Box<dyn Fn(SysValue) + Send>) -> bool {
    *LISTENER.lock().unwrap() = Some(dispatch);
    if let Some(tx) = RESCAN.get() {
        let _ = tx.lock().unwrap().send(Update::Listener);
        return true;
    }
    let registry = Arc::new(Mutex::new(Registry::default()));
    let Ok(c) = zbus::blocking::connection::Builder::session().and_then(|b| b.serve_at(WATCHER_PATH, Watcher(registry.clone()))).and_then(|b| b.build()) else { return false };
    // Is there a watcher already? If not, us. Without queueing: either you are or you are not.
    let we_are = c.request_name_with_flags(WATCHER, zbus::fdo::RequestNameFlags::DoNotQueue.into()).is_ok();
    if we_are {
        println!("tray   · there was no watcher: applications register here");
    } else {
        // Host for someone else's tray: we need a name and to tell it.
        let _ = c.object_server().remove::<Watcher, _>(WATCHER_PATH);
        let mine = format!("org.kde.StatusNotifierHost-{}-pleamar", std::process::id());
        let _ = c.request_name(mine.as_str());
        let Ok(watcher) = Proxy::new(&c, WATCHER, WATCHER_PATH, WATCHER) else { return false };
        if watcher.call_method("RegisterStatusNotifierHost", &(mine.as_str(),)).is_err() {
            return false;
        }
    }
    let _ = CONNECTION.set(c.clone());
    let (tx, updates) = channel();
    registry.lock().unwrap().updates = Some(tx.clone());
    let _ = RESCAN.set(Mutex::new(tx.clone()));

    // Three things mean we have to look again: an icon changing (`NewIcon`,
    // `NewTitle`, `NewToolTip`, `NewStatus`… every signal of the item), the
    // watcher (if it is someone else) registering or removing someone, and someone leaving the bus.
    let signal = |interface: &'static str, member: Option<&'static str>| {
        let b = MatchRule::builder().msg_type(zbus::message::Type::Signal).interface(interface).ok()?;
        Some(match member { Some(m) => b.member(m).ok()?, None => b }.build())
    };
    for rule in [signal(ITEM, None), signal(WATCHER, None), signal("org.freedesktop.DBus", Some("NameOwnerChanged"))].into_iter().flatten() {
        let (c, tx) = (c.clone(), tx.clone());
        std::thread::spawn(move || {
            let Ok(messages) = MessageIterator::for_match_rule(rule, &c, Some(32)) else { return };
            for _ in messages {
                if tx.send(Update::Rescan).is_err() { break }
            }
        });
    }

    std::thread::Builder::new().name("tray".into()).spawn(move || {
        let own = we_are.then_some(&*registry);
        let last = std::cell::RefCell::new(String::new());
        let rescan = |c: &Connection| {
            let v = report(c, own);
            let fingerprint = format!("{v:?}");
            if fingerprint != *last.borrow() {
                *last.borrow_mut() = fingerprint;
                if let Some(f) = LISTENER.lock().unwrap().as_ref() {
                    f(v);
                }
            }
        };
        rescan(&c);
        while let Ok(n) = updates.recv() {
            // Someone new is listening: everything, even if nothing changed.
            if matches!(n, Update::Listener) {
                last.borrow_mut().clear();
            }
            if let Update::Registered(full) = &n {
                let _ = c.emit_signal(None::<&str>, WATCHER_PATH, WATCHER, "StatusNotifierItemRegistered", &(full.as_str(),));
            }
            // A change is usually several signals in a row: we look once.
            std::thread::sleep(std::time::Duration::from_millis(60));
            while updates.try_recv().is_ok() {}
            rescan(&c);
        }
    }).is_ok()
}

// ── the menus ─────────────────────────────────────────────────────
//
// Another agreement, `com.canonical.dbusmenu`: the application publishes its menu as a
// tree and is told what was clicked. Here we only read and notify; painting it is
// the scene's job, which is what it has `popup` for.

const MENU: &str = "com.canonical.dbusmenu";

fn menu_of(c: &Connection, key: &str) -> Result<Proxy<'static>, String> {
    let p = element(c, key).ok_or("that icon is gone")?;
    let path: zbus::zvariant::OwnedObjectPath = p.get_property("Menu").map_err(|_| "that icon has no menu")?;
    let (service, _) = split(key);
    zbus::blocking::proxy::Builder::new(c).destination(service.to_owned()).map_err(|e| e.to_string())?.path(path).map_err(|e| e.to_string())?.interface(MENU).map_err(|e| e.to_string())?.cache_properties(CacheProperties::No).build().map_err(|e| e.to_string())
}

/// A tree node arrives as (id, properties, children), and each child, wrapped
/// in a variant, is another one of the same.
fn node(v: &zbus::zvariant::Value) -> Option<SysValue> {
    use zbus::zvariant::Value;
    let v = if let Value::Value(inner) = v { inner } else { v };
    let Value::Structure(s) = v else { return None };
    let [Value::I32(id), Value::Dict(props), Value::Array(children)] = s.fields() else { return None };
    let text = |k: &str| props.get::<&str, &str>(&k).ok().flatten().map(str::to_owned);
    let flag = |k: &str| props.get::<&str, bool>(&k).ok().flatten();
    if flag("visible") == Some(false) {
        return None;
    }
    let children: Vec<SysValue> = children.iter().filter_map(node).collect();
    // `_File` marks the shortcut letter; `__` is a real underscore.
    let label = text("label").unwrap_or_default().replace("__", "\u{1}").replace('_', "").replace('\u{1}', "_");
    let toggle = text("toggle-type").filter(|t| !t.is_empty());
    let mut m = vec![
        ("id".to_owned(), SysValue::Num(*id as f64)),
        ("label".to_owned(), SysValue::Text(label)),
        ("enabled".to_owned(), SysValue::Bool(flag("enabled").unwrap_or(true))),
        ("separator".to_owned(), SysValue::Bool(text("type").as_deref() == Some("separator"))),
        ("children".to_owned(), SysValue::List(children)),
    ];
    if toggle.is_some() {
        m.push(("checked".to_owned(), SysValue::Bool(props.get::<&str, i32>(&"toggle-state").ok().flatten() == Some(1))));
    }
    Some(SysValue::Map(m))
}

/// `sys.ask("tray.menu", key)` → `{ { id, label, enabled, separator, checked, children }, … }`
pub fn query(what: &str, args: &[SysValue]) -> Result<SysValue, String> {
    let c = CONNECTION.get().ok_or("the tray is not running: sys.watch(\"tray\", …) is missing")?;
    let ("tray.menu", [SysValue::Text(key)]) = (what, args) else { return Err(format!("'{what}' is not asked like that: tray.menu(key)")) };
    let menu = menu_of(c, key)?;
    // Many do not fill in their menu until they are told it is about to be shown.
    let _ = menu.call_method("AboutToShow", &(0i32,));
    let reply = menu.call_method("GetLayout", &(0i32, -1i32, Vec::<&str>::new())).map_err(|e| e.to_string())?;
    let body = reply.body();
    // Of the root only its children matter: the root itself is nothing that can be clicked.
    type Root = (i32, std::collections::HashMap<String, zbus::zvariant::OwnedValue>, Vec<zbus::zvariant::OwnedValue>);
    let (_, (_, _, children)): (u32, Root) = body.deserialize().map_err(|e| format!("I don't understand the menu that application sent: {e}"))?;
    Ok(SysValue::List(children.iter().filter_map(|h| node(h)).collect()))
}

/// `tray.activate(key)` —the usual click—, `tray.secondary(key)` —the middle
/// one—, `tray.context(key)` —the application shows its own menu, if it knows how— and
/// `tray.scroll(key, notches)`.
pub fn command(what: &str, args: &[SysValue]) -> Result<(), String> {
    let c = CONNECTION.get().ok_or("the tray is not running: sys.watch(\"tray\", …) is missing")?;
    let [SysValue::Text(key), rest @ ..] = args else { return Err(format!("'{what}' wants the icon's `key`")) };
    let p = element(c, key).ok_or("that icon is gone")?;
    let result = match (what, rest) {
        // Where on the screen it was clicked. We do not know (Wayland does not tell): 0, 0.
        ("tray.activate", []) => p.call_method("Activate", &(0i32, 0i32)),
        ("tray.secondary", []) => p.call_method("SecondaryActivate", &(0i32, 0i32)),
        ("tray.context", []) => p.call_method("ContextMenu", &(0i32, 0i32)),
        ("tray.scroll", [SysValue::Num(d)]) => p.call_method("Scroll", &(*d as i32 * 120, "vertical")),
        // Something was picked from its menu: the `id` is the one that came in `tray.menu`.
        ("tray.menu_click", [SysValue::Num(id)]) => {
            let menu = menu_of(c, key)?;
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as u32);
            return menu.call_method("Event", &(*id as i32, "clicked", zbus::zvariant::Value::I32(0), now)).map(|_| ()).map_err(|e| e.to_string());
        }
        _ => return Err(format!("'{what}' does not exist: tray.activate, tray.secondary, tray.context, tray.scroll, tray.menu_click")),
    };
    result.map(|_| ()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::plain;

    #[test]
    fn a_tooltip_body_is_read_as_plain_text() {
        assert_eq!(plain("3 new messages"), "3 new messages");
        assert_eq!(plain("<b>Telegram</b><br/>3 new &amp; 1 <i>muted</i>"), "Telegram\n3 new & 1 muted");
        assert_eq!(plain("one<BR>two<br />three"), "one\ntwo\nthree");
        assert_eq!(plain("<a href=\"https://example.org\">a link</a> <img src=\"x.png\"/>"), "a link");
        assert_eq!(plain("  <p>padded</p>\n"), "padded");
        assert_eq!(plain(""), "");
    }
}
