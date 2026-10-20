//! The desktop's notifications. Here nobody is listened to: **we are** the
//! server. Applications call `org.freedesktop.Notifications.Notify`, and
//! whoever holds that name on the bus is the one who shows them. There can be
//! only one: the newest shell takes it from an older one (which gets it back
//! when the newest leaves); a daemon that will not give it up keeps it, and
//! the service waits behind it.
//!
//! On Windows it will be `UserNotificationListener`; macOS does not let you read other apps' ones.
//!
//! `{ { id, app, title, body, icon, image, urgency, time, actions = { { key, label }, … } }, … }`,
//! newest first. `urgency`: 0 low, 1 normal, 2 critical. `icon` is the
//! application's; `image` is the picture the notification brings —the sender's
//! photo, a screenshot—, a file even when it arrives as pixels. `time` is when
//! it arrived, in seconds since 1970.
//!
//! `notification_history` is the same shape: the ones that went away on their
//! own, unseen, newest first, the last 50. Not the ones someone closed or
//! opened: those were seen. It lives while the process does.

use super::SysValue;
use std::collections::HashMap;
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use zbus::zvariant::OwnedValue;

const NAME: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";
/// How long one lasts when it does not say how long it wants to last.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(6);

struct Notification {
    id: u32,
    app: String,
    title: String,
    body: String,
    icon: String,
    image: String,
    urgency: u8,
    time: u64,
    actions: Vec<(String, String)>,
    expires: Option<Instant>,
}

/// What has to be told to the bus, or done, outside the call that caused it.
enum Event {
    Change,
    /// Someone new is listening: tell them everything, even if nothing changed.
    Listener,
    Closed(u32, u32),
    Action(u32, String),
}

#[derive(Default)]
struct State {
    notifications: Vec<Notification>,
    /// The ones that expired, oldest first.
    history: Vec<Notification>,
    next: u32,
    /// Whether notifications stay until someone deals with them. Expiring after
    /// six seconds is what a bubble bar does; a notification CENTER —a history,
    /// a tray— keeps them, and the spec leaves it in the server's hands. And it
    /// is not only how long they are seen: once it is closed, the application
    /// stops listening for its action, so an expired one can no longer be opened.
    keep: bool,
}

struct Hub {
    state: Mutex<State>,
    events: Mutex<Sender<Event>>,
}

static HUB: OnceLock<Arc<Hub>> = OnceLock::new();
/// How many the history keeps.
const HISTORY: usize = 50;

/// Whoever hears the history, as `LISTENER` hears the list.
static HISTORY_LISTENER: Mutex<Option<Box<dyn Fn(SysValue) + Send>>> = Mutex::new(None);

/// Whoever hears the list: the logic that asked last. The server is one per
/// process and outlives the logic: when the logic is reloaded, the new one
/// takes the name's place in here instead of asking D-Bus for the name again,
/// which the old one still held —and the new logic, finding it taken, fell
/// back to made-up notices—.
static LISTENER: Mutex<Option<Box<dyn Fn(SysValue) + Send>>> = Mutex::new(None);

impl Hub {
    fn send(&self, c: Event) {
        let _ = self.events.lock().unwrap().send(c);
    }

    /// Why it is closed: 1 it expired, 2 the user closed it, 3 the application asked.
    fn close(&self, id: u32, reason: u32) -> bool {
        let mut e = self.state.lock().unwrap();
        let Some(k) = e.notifications.iter().position(|a| a.id == id) else { return false };
        let gone = e.notifications.remove(k);
        if reason == 1 {
            e.history.push(gone);
            let extra = e.history.len().saturating_sub(HISTORY);
            e.history.drain(..extra);
        }
        drop(e);
        self.send(Event::Closed(id, reason));
        true
    }

    fn report(&self) -> SysValue {
        Self::list(&self.state.lock().unwrap().notifications)
    }

    fn history(&self) -> SysValue {
        Self::list(&self.state.lock().unwrap().history)
    }

    fn list(all: &[Notification]) -> SysValue {
        let text = |s: &str| SysValue::Text(s.to_owned());
        SysValue::List(all.iter().rev().map(|a| SysValue::Map(vec![
            ("id".into(), SysValue::Num(a.id as f64)),
            ("app".into(), text(&a.app)),
            ("title".into(), text(&a.title)),
            ("body".into(), text(&a.body)),
            ("icon".into(), text(&a.icon)),
            ("image".into(), text(&a.image)),
            ("urgency".into(), SysValue::Num(a.urgency as f64)),
            ("time".into(), SysValue::Num(a.time as f64)),
            ("actions".into(), SysValue::List(a.actions.iter().map(|(k, l)| SysValue::Map(vec![("key".into(), text(k)), ("label".into(), text(l))])).collect())),
        ])).collect())
    }
}

/// We say we do not understand markup, but some send it anyway.
fn strip_markup(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut inside = false;
    for c in s.chars() {
        match c {
            '<' => inside = true,
            '>' if inside => inside = false,
            _ if !inside => out.push(c),
            _ => {}
        }
    }
    // One pass, so that what an entity gives is not read again: `&amp;lt;`
    // is the text `&lt;`, not `<`.
    let mut text = String::with_capacity(out.len());
    let mut rest = out.as_str();
    while let Some(k) = rest.find('&') {
        text.push_str(&rest[..k]);
        rest = &rest[k..];
        let entity = [("&amp;", '&'), ("&lt;", '<'), ("&gt;", '>'), ("&quot;", '"'), ("&apos;", '\'')].into_iter().find(|(e, _)| rest.starts_with(e));
        let (taken, c) = entity.map_or((1, '&'), |(e, c)| (e.len(), c));
        text.push(c);
        rest = &rest[taken..];
    }
    text.push_str(rest);
    text
}

/// `image-data`: `(width, height, rowstride, has_alpha, bits_per_sample, channels, data)`.
/// Saved as a PNG named after what it contains, like the tray's pixel icons,
/// so the scene paints it as any other file.
type Pixels = (i32, i32, i32, bool, i32, i32, Vec<u8>);

fn from_pixels((w, h, stride, _, bits, channels, data): Pixels) -> Option<String> {
    use std::hash::{Hash, Hasher};
    if w <= 0 || h <= 0 || bits != 8 || !(3..=4).contains(&channels) || stride < w * channels || data.len() < (stride * (h - 1) + w * channels) as usize {
        return None;
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    data.hash(&mut hasher);
    let base = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().into_owned());
    let dir = std::path::Path::new(&base).join("pleamar").join("notifications");
    let path = dir.join(format!("{:016x}.png", hasher.finish()));
    if !path.exists() {
        std::fs::create_dir_all(&dir).ok()?;
        let (w, h, stride, channels) = (w as usize, h as usize, stride as usize, channels as usize);
        let mut rgba = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            for p in data[y * stride..y * stride + w * channels].chunks_exact(channels) {
                rgba.extend_from_slice(&[p[0], p[1], p[2], if channels == 4 { p[3] } else { 255 }]);
            }
        }
        image::RgbaImage::from_raw(w as u32, h as u32, rgba)?.save(&path).ok()?;
    }
    Some(path.to_string_lossy().into_owned())
}

struct Server(Arc<Hub>);

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl Server {
    #[allow(clippy::too_many_arguments)]
    fn notify(&self, app_name: &str, replaces_id: u32, app_icon: &str, summary: &str, body: &str, actions: Vec<String>, hints: HashMap<String, OwnedValue>, expire_timeout: i32) -> u32 {
        let hint = |k: &str| hints.get(k).and_then(|v| <&str>::try_from(v).ok()).map(str::to_owned);
        let icon = if app_icon.is_empty() { hint("image-path").or_else(|| hint("image_path")).unwrap_or_default() } else { app_icon.to_owned() };
        let urgency = hints.get("urgency").and_then(|v| u8::try_from(v).ok()).unwrap_or(1);
        // The picture: pixels first, as the spec says, then a path.
        let pixels = ["image-data", "image_data", "icon_data"].iter().find_map(|k| hints.get(*k)).and_then(|v| v.try_clone().ok()).and_then(|v| Pixels::try_from(v).ok());
        let image = pixels.and_then(from_pixels).or_else(|| hint("image-path").or_else(|| hint("image_path"))).unwrap_or_default();
        let time = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let mut e = self.0.state.lock().unwrap();
        // With `replaces_id`, the same notification changing: the volume going up, a download progressing.
        let id = if replaces_id != 0 && e.notifications.iter().any(|a| a.id == replaces_id) {
            e.notifications.retain(|a| a.id != replaces_id);
            replaces_id
        } else {
            e.next += 1;
            e.next
        };
        let expires = match expire_timeout {
            _ if e.keep => None,
            0 => None,
            // Critical ones do not go away on their own, whatever they say.
            _ if urgency >= 2 => None,
            ms if ms > 0 => Some(Instant::now() + Duration::from_millis(ms as u64)),
            _ => Some(Instant::now() + DEFAULT_TIMEOUT),
        };
        e.notifications.push(Notification {
            id,
            app: app_name.to_owned(),
            title: strip_markup(summary),
            body: strip_markup(body),
            icon: icon.trim_start_matches("file://").to_owned(),
            image: image.trim_start_matches("file://").to_owned(),
            urgency,
            time,
            // They come in pairs: key, label.
            actions: actions.chunks(2).filter(|p| p.len() == 2).map(|p| (p[0].clone(), p[1].clone())).collect(),
            expires,
        });
        drop(e);
        self.0.send(Event::Change);
        id
    }

    fn close_notification(&self, id: u32) {
        if self.0.close(id, 3) {
            self.0.send(Event::Change);
        }
    }

    fn get_capabilities(&self) -> Vec<String> {
        vec!["body".into(), "actions".into(), "icon-static".into(), "persistence".into(), "body-images".into()]
    }

    fn get_server_information(&self) -> (String, String, String, String) {
        ("pleamar".into(), "k4ditano".into(), env!("CARGO_PKG_VERSION").into(), "1.2".into())
    }
}

pub fn service(dispatch: Box<dyn Fn(SysValue) + Send>) -> bool {
    *LISTENER.lock().unwrap() = Some(dispatch);
    serve()
}

/// `notification_history`: it needs the server as much as the list does.
pub fn history(dispatch: Box<dyn Fn(SysValue) + Send>) -> bool {
    *HISTORY_LISTENER.lock().unwrap() = Some(dispatch);
    serve()
}

fn serve() -> bool {
    // Already serving, in this process: the new listener gets the list, and that is all.
    if let Some(hub) = HUB.get() {
        hub.send(Event::Listener);
        return true;
    }
    let (tx, events) = channel();
    let hub = Arc::new(Hub { state: Mutex::default(), events: Mutex::new(tx) });
    // The newest one takes the name, and lets the next one take it too: the
    // bus is one per user, so a shell started in another session (another
    // TTY) would otherwise find it held by the one left behind. Whoever had
    // it waits in the queue and gets it back when this one goes. A daemon
    // that does not let it go (mako, a desktop's own) keeps it: this one
    // waits behind it, and serves from the moment it leaves.
    use zbus::fdo::{RequestNameFlags, RequestNameReply};
    let flags = RequestNameFlags::AllowReplacement | RequestNameFlags::ReplaceExisting;
    let connection = zbus::blocking::connection::Builder::session()
        .and_then(|b| b.serve_at(PATH, Server(hub.clone())))
        .and_then(|b| b.build())
        .and_then(|c| c.request_name_with_flags(NAME, flags).map(|reply| (c, reply)));
    let connection = match connection {
        Ok((c, reply)) => {
            if reply == RequestNameReply::InQueue {
                eprintln!("notifications · another program has them and will not let them go: they come here once it leaves");
            }
            c
        }
        Err(e) => {
            eprintln!("notifications · I cannot be the one receiving notifications ({e})");
            return false;
        }
    };
    let _ = HUB.set(hub.clone());
    std::thread::Builder::new().name("notifications".into()).spawn(move || {
        // Only reported if the list is different: discarding what is no longer there is not news.
        let last = std::cell::RefCell::new((String::new(), String::new()));
        let dispatch = |_: SysValue| {
            let (now, past) = (hub.report(), hub.history());
            let fingerprints = (format!("{now:?}"), format!("{past:?}"));
            let mut last = last.borrow_mut();
            if last.0 != fingerprints.0 {
                if let Some(f) = LISTENER.lock().unwrap().as_ref() {
                    f(now);
                }
            }
            if last.1 != fingerprints.1 {
                if let Some(f) = HISTORY_LISTENER.lock().unwrap().as_ref() {
                    f(past);
                }
            }
            *last = fingerprints;
        };
        dispatch(hub.report());
        loop {
            // Sleeps until something happens or it is the next one's turn to expire.
            let now = Instant::now();
            let wait = hub.state.lock().unwrap().notifications.iter().filter_map(|a| a.expires).min().map(|c| c.saturating_duration_since(now));
            let event = match wait {
                Some(d) => events.recv_timeout(d),
                None => events.recv().map_err(|_| RecvTimeoutError::Disconnected),
            };
            match event {
                Ok(Event::Change) => dispatch(hub.report()),
                Ok(Event::Listener) => {
                    *last.borrow_mut() = Default::default();
                    dispatch(hub.report());
                }
                Ok(Event::Closed(id, reason)) => {
                    let _ = connection.emit_signal(None::<&str>, PATH, NAME, "NotificationClosed", &(id, reason));
                }
                Ok(Event::Action(id, key)) => {
                    let _ = connection.emit_signal(None::<&str>, PATH, NAME, "ActionInvoked", &(id, key.as_str()));
                }
                Err(RecvTimeoutError::Timeout) => {
                    let now = Instant::now();
                    let expired: Vec<u32> = hub.state.lock().unwrap().notifications.iter().filter(|a| a.expires.is_some_and(|c| c <= now)).map(|a| a.id).collect();
                    for id in expired {
                        hub.close(id, 1);
                    }
                    dispatch(hub.report());
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }).is_ok()
}

/// `notifications.dismiss(id)`, `notifications.invoke(id, "default")`, `notifications.clear()`,
/// and `notifications.keep(true)`: so they do not expire on their own, which is what a notification center wants.
pub fn command(what: &str, args: &[SysValue]) -> Result<(), String> {
    let hub = HUB.get().ok_or("the notifications service is not running: sys.watch(\"notifications\", …) is missing")?;
    match (what, args) {
        ("notifications.dismiss", [SysValue::Num(id)]) => {
            hub.close(*id as u32, 2);
        }
        ("notifications.invoke", [SysValue::Num(id), SysValue::Text(key)]) => {
            // The application learns which button it was, and the notification has done its job.
            hub.send(Event::Action(*id as u32, key.clone()));
            hub.close(*id as u32, 2);
        }
        ("notifications.keep", [SysValue::Bool(yes)]) => {
            let mut e = hub.state.lock().unwrap();
            e.keep = *yes;
            if *yes {
                // And the ones that were already counting down stop counting.
                e.notifications.iter_mut().for_each(|a| a.expires = None);
            }
        }
        // The history: one out, or all of it.
        ("notifications.forget", [SysValue::Num(id)]) => {
            hub.state.lock().unwrap().history.retain(|a| a.id != *id as u32);
        }
        ("notifications.clear_history", []) => {
            hub.state.lock().unwrap().history.clear();
        }
        ("notifications.clear", []) => {
            let ids: Vec<u32> = hub.state.lock().unwrap().notifications.iter().map(|a| a.id).collect();
            ids.into_iter().for_each(|id| { hub.close(id, 2); });
        }
        _ => return Err(format!("'{what}' is not asked like that: notifications.dismiss(id), notifications.invoke(id, key), notifications.keep(true), notifications.clear(), notifications.forget(id), notifications.clear_history()")),
    }
    hub.send(Event::Change);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::strip_markup;

    #[test]
    fn markup_is_dropped_and_each_entity_read_once() {
        assert_eq!(strip_markup("<b>3</b> new &amp; unread"), "3 new & unread");
        assert_eq!(strip_markup("&lt;&gt;&quot;&apos;"), "<>\"'");
        assert_eq!(strip_markup("&amp;lt;b&amp;gt; is bold"), "&lt;b&gt; is bold");
        assert_eq!(strip_markup("fish & chips &copy;"), "fish & chips &copy;");
    }
}
