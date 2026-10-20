//! What is playing, through MPRIS: the D-Bus agreement that players and browsers
//! follow on Linux. On Windows it will be SMTC; on macOS, MediaRemote.
//!
//! `{ playing, title, artist, album, length, position, rate, can_seek, art, player, players }`. If there are
//! several, the one chosen with `media.choose` is reported, while it is there; otherwise the one
//! that is playing; if none is, the one that played last; and before anything has played, the
//! first. With no players, `player = ""`. `players` is all of them, `{ id, name, playing,
//! chosen }`, to choose from.
//!
//! MPRIS does not signal `Position` as it moves, so it is read whenever something
//! else is reported, and on `Seeked`. Between reports the scene carries it on with
//! its own clock, at `rate`, while `playing`: nothing here polls.
//!
//! `media.seek` moves it, where the player says it can (`can_seek`): `SetPosition` with the song's
//! `mpris:trackid`, the way the spec asks for a place in the song, or `Seek` by the difference
//! where the player gives no trackid. A seek is reported back through `Seeked`, and read again
//! straight after in case the player does not send it.

use super::SysValue;
use std::collections::HashMap;
use std::sync::mpsc::Sender;
use std::sync::Mutex;
use zbus::blocking::{fdo::DBusProxy, Connection, MessageIterator, Proxy};
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue};
use zbus::MatchRule;

const PREFIX: &str = "org.mpris.MediaPlayer2.";
const PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER: &str = "org.mpris.MediaPlayer2.Player";

/// The player `media.choose` pinned, by its bus name without the prefix; "" is none. It is the
/// process's, like the default output is the system's: every scene reports the same one.
static CHOSEN: Mutex<String> = Mutex::new(String::new());
/// The last player seen playing, by its whole bus name; "" is none yet. A pause leaves `media` on
/// it: without this, nothing playing meant the first on the bus, and play after a pause went
/// there —a phone through KDE Connect sorts before most players—.
static LAST: Mutex<String> = Mutex::new(String::new());
/// The running services, to report again at once when the choice changes.
static WAKE: Mutex<Vec<Sender<()>>> = Mutex::new(Vec::new());

fn chosen() -> String {
    CHOSEN.lock().map(|c| c.clone()).unwrap_or_default()
}

/// Every running service reports again now; one whose thread has gone is dropped here.
fn wake() {
    if let Ok(mut w) = WAKE.lock() { w.retain(|tx| tx.send(()).is_ok()) }
}

/// Every player on the bus but `playerctld`, which is no player of its own: it mirrors whichever
/// one was active last, and would show up twice.
fn players(c: &Connection) -> Vec<String> {
    let Ok(bus) = DBusProxy::new(c) else { return Vec::new() };
    let mut names: Vec<String> = bus.list_names().unwrap_or_default().into_iter().map(|n| n.to_string()).filter(|n| n.starts_with(PREFIX) && n[PREFIX.len()..] != *"playerctld").collect();
    names.sort();
    names
}

fn player(c: &Connection, name: &str) -> Option<Proxy<'static>> {
    Proxy::new(c, name.to_owned(), PATH, PLAYER).ok()
}

fn is_playing(p: &Proxy) -> bool {
    p.get_property::<String>("PlaybackStatus").is_ok_and(|s| s == "Playing")
}

/// What a pause falls back to: see `LAST`.
fn remember(name: &str) {
    if let Ok(mut l) = LAST.lock() { name.clone_into(&mut l) }
}

/// The chosen one if it is there; otherwise the one that is playing, the one that played last, or
/// the first.
fn active_player(c: &Connection) -> Option<(String, Proxy<'static>, bool)> {
    let pinned = chosen();
    let names = players(c);
    if !pinned.is_empty() {
        if let Some(n) = names.iter().find(|n| n.strip_prefix(PREFIX) == Some(pinned.as_str())) {
            if let Some(p) = player(c, n) {
                let playing = is_playing(&p);
                if playing { remember(n) }
                return Some((n.clone(), p, playing));
            }
        }
    }
    let last = LAST.lock().map(|l| l.clone()).unwrap_or_default();
    let (mut first, mut previous) = (None, None);
    for n in names {
        let Some(p) = player(c, &n) else { continue };
        if is_playing(&p) {
            remember(&n);
            return Some((n, p, true));
        }
        if n == last { previous = Some((n, p, false)) } else { first.get_or_insert((n, p, false)); }
    }
    previous.or(first)
}

/// Every player, to choose from: `id` is what `media.choose` takes, `name` what the player
/// calls itself (`Identity`), which tells two of the same program apart where `player` cannot
/// —two phones through KDE Connect are both `kdeconnect`—.
fn all_players(c: &Connection) -> SysValue {
    let pinned = chosen();
    SysValue::List(players(c).into_iter().map(|n| {
        let id = n.trim_start_matches(PREFIX).to_owned();
        let name = Proxy::new(c, n.clone(), PATH, "org.mpris.MediaPlayer2").ok().and_then(|r| r.get_property::<String>("Identity").ok()).unwrap_or_else(|| id.clone());
        let playing = player(c, &n).is_some_and(|p| is_playing(&p));
        SysValue::Map(vec![("id".into(), SysValue::Text(id.clone())), ("name".into(), SysValue::Text(name)), ("playing".into(), SysValue::Bool(playing)), ("chosen".into(), SysValue::Bool(id == pinned))])
    }).collect())
}

fn text(v: &OwnedValue) -> String {
    if let Ok(s) = <&str>::try_from(v) { return s.to_owned() }
    // `xesam:artist` is a list: the artists, with commas.
    <Vec<String>>::try_from(v.clone()).map(|l| l.join(", ")).unwrap_or_default()
}

/// `mpris:length` is in microseconds, and the spec says `x`, but some players send `t`.
fn seconds(v: &OwnedValue) -> f64 {
    let us = i64::try_from(v).ok().or_else(|| u64::try_from(v).ok().map(|n| n as i64)).unwrap_or(0);
    us.max(0) as f64 / 1e6
}

/// `Position` is not in the metadata: it is a property of its own, and the one that
/// never says it changed. A player that does not know it answers with an error: 0.
fn position(p: &Proxy) -> f64 {
    p.get_property::<OwnedValue>("Position").map(|v| seconds(&v)).unwrap_or(0.0)
}

/// Whether `media.seek` can move it. The spec has every player say; one that does not is taken at
/// its silence, so a scene does not offer a scrubber that would do nothing.
fn can_seek(p: &Proxy) -> bool {
    p.get_property::<bool>("CanSeek").unwrap_or(false)
}

/// `mpris:trackid`, which `SetPosition` needs to know the seek is for the song still playing. The
/// spec makes it an object path; a player that sends it as text is taken if the text is one.
/// `NoTrack` is the spec's way of saying there is none.
fn track_id(v: &OwnedValue) -> Option<OwnedObjectPath> {
    let path = OwnedObjectPath::try_from(v.clone()).ok().or_else(|| <&str>::try_from(v).ok().and_then(|s| ObjectPath::try_from(s).ok()).map(OwnedObjectPath::from))?;
    (path.as_str() != "/org/mpris/MediaPlayer2/TrackList/NoTrack").then_some(path)
}

/// How a player is moved to `to` seconds: not before 0, and not past `length` where the player
/// says how long the song is (with no length there is no end to hold it at), in microseconds; to
/// that place in this track if it has an id, otherwise by the difference from where it is now
/// (`Seek`). A `Seek` past the end goes to the next song, and `SetPosition` past it is ignored:
/// neither is what a scrubber at its end means.
#[derive(Debug, PartialEq)]
enum Jump {
    To(OwnedObjectPath, i64),
    By(i64),
}

/// How far short of the end a `Seek` stops: `now` was read a moment before the call, and a song
/// that plays on meanwhile would be carried past its end by the full difference.
const SEEK_END_MARGIN: f64 = 0.5;

fn jump(to: f64, now: f64, length: f64, track: Option<OwnedObjectPath>) -> Jump {
    let to = if length > 0.0 { to.clamp(0.0, length) } else { to.max(0.0) };
    let us = |s: f64| (s * 1e6).round() as i64;
    match track {
        // A trackid gone stale (the song changed meanwhile) makes the spec's player ignore the
        // call, and the wake after it reports the new song.
        Some(t) => Jump::To(t, us(to)),
        None => {
            let to = if length > 0.0 { to.min((length - SEEK_END_MARGIN).max(0.0)) } else { to };
            Jump::By(us(to) - us(now))
        }
    }
}

fn now(c: &Connection) -> SysValue {
    let field = |k: &str, v: SysValue| (k.to_owned(), v);
    let Some((name, p, playing)) = active_player(c) else {
        return SysValue::Map(vec![field("playing", SysValue::Bool(false)), field("title", SysValue::Text(String::new())), field("artist", SysValue::Text(String::new())), field("album", SysValue::Text(String::new())), field("length", SysValue::Num(0.0)), field("position", SysValue::Num(0.0)), field("rate", SysValue::Num(1.0)), field("can_seek", SysValue::Bool(false)), field("art", SysValue::Text(String::new())), field("player", SysValue::Text(String::new())), field("players", SysValue::List(Vec::new()))]);
    };
    let metadata: HashMap<String, OwnedValue> = p.get_property("Metadata").unwrap_or_default();
    let from_metadata = |k: &str| SysValue::Text(metadata.get(k).map(text).unwrap_or_default());
    SysValue::Map(vec![
        field("playing", SysValue::Bool(playing)),
        field("title", from_metadata("xesam:title")),
        field("artist", from_metadata("xesam:artist")),
        field("album", from_metadata("xesam:album")),
        field("length", SysValue::Num(metadata.get("mpris:length").map(seconds).unwrap_or(0.0))),
        field("position", SysValue::Num(position(&p))),
        // The spec's default; a player that cannot change it may leave it out.
        field("rate", SysValue::Num(p.get_property::<f64>("Rate").unwrap_or(1.0))),
        field("can_seek", SysValue::Bool(can_seek(&p))),
        field("art", SysValue::Text(metadata.get("mpris:artUrl").map(text).map_or_else(String::new, |u| art_path(&u)))),
        field("player", SysValue::Text(name.trim_start_matches(PREFIX).split('.').next().unwrap_or_default().to_owned())),
        field("players", all_players(c)),
    ])
}

pub fn service(dispatch: Box<dyn Fn(SysValue) + Send>) -> bool {
    let Ok(c) = Connection::session() else { return false };
    std::thread::Builder::new().name("media".into()).spawn(move || {
        let mut last = String::new();
        let mut report = |c: &Connection| {
            let v = now(c);
            let fingerprint = format!("{v:?}");
            if fingerprint != last { last = fingerprint; dispatch(v) }
        };
        report(&c);
        // Three things matter: a player changing something, one jumping to another
        // point of the song (`Seeked`, the only time the position says it moved),
        // and one appearing or going away.
        let rules = [
            MatchRule::builder().msg_type(zbus::message::Type::Signal).interface("org.freedesktop.DBus.Properties").and_then(|b| b.member("PropertiesChanged")).and_then(|b| b.path(PATH)).map(|b| b.build()),
            MatchRule::builder().msg_type(zbus::message::Type::Signal).interface(PLAYER).and_then(|b| b.member("Seeked")).and_then(|b| b.path(PATH)).map(|b| b.build()),
            MatchRule::builder().msg_type(zbus::message::Type::Signal).interface("org.freedesktop.DBus").and_then(|b| b.member("NameOwnerChanged")).and_then(|b| b.arg0ns(PREFIX.trim_end_matches('.'))).map(|b| b.build()),
        ];
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        if let Ok(mut w) = WAKE.lock() { w.push(tx.clone()) }
        for rule in rules.into_iter().flatten() {
            let (c, tx) = (c.clone(), tx.clone());
            std::thread::spawn(move || {
                let Ok(messages) = MessageIterator::for_match_rule(rule, &c, Some(16)) else { return };
                for _ in messages { if tx.send(()).is_err() { break } }
            });
        }
        drop(tx);
        while rx.recv().is_ok() {
            // A song change is several signals in a row: it is reported once.
            std::thread::sleep(std::time::Duration::from_millis(40));
            while rx.try_recv().is_ok() {}
            report(&c);
        }
    }).is_ok()
}

/// `media.toggle`, `media.next`, `media.previous`: to the one `now` is reporting.
/// `media.seek(seconds)`: to that point of its song; `media.step(seconds)`: that far from where
/// it is, back if negative. Both stay inside the song where the player says how long it is (a
/// `Seek` a little short of its end); with no length, only above 0.
/// `media.choose(id)`: report that one, and command it, while it is there; `""` goes back to
/// the one playing, else the one that played last, else the first.
pub fn command(what: &str, args: &[SysValue]) -> Result<(), String> {
    let method = match (what, args) {
        ("media.toggle", _) => "PlayPause",
        ("media.next", _) => "Next",
        ("media.previous", _) => "Previous",
        ("media.seek" | "media.step", [SysValue::Num(s)]) => return seek(what == "media.step", *s),
        ("media.seek", _) => return Err("media.seek takes the seconds into the song to go to".into()),
        ("media.step", _) => return Err("media.step takes how many seconds to move, back if negative".into()),
        ("media.choose", [SysValue::Text(id)]) => {
            if let Ok(mut c) = CHOSEN.lock() { c.clone_from(id) }
            wake();
            return Ok(());
        }
        ("media.choose", _) => return Err("media.choose takes the `id` of one of `players`, or \"\" to let it choose".into()),
        _ => return Err(format!("'{what}' does not exist: media.toggle, media.next, media.previous, media.seek(seconds), media.step(seconds), media.choose(id)")),
    };
    let c = Connection::session().map_err(|e| e.to_string())?;
    let (_, p, _) = active_player(&c).ok_or("there is no player open")?;
    p.call_method(method, &()).map(|_| ()).map_err(|e| e.to_string())
}

/// `media.seek` and `media.step`, to the one `now` is reporting.
fn seek(relative: bool, s: f64) -> Result<(), String> {
    if !s.is_finite() { return Err(format!("media.seek and media.step take seconds, not {s}")) }
    let c = Connection::session().map_err(|e| e.to_string())?;
    let (name, p, _) = active_player(&c).ok_or("there is no player open")?;
    if !can_seek(&p) {
        return Err(format!("{} cannot seek (its CanSeek is false)", name.trim_start_matches(PREFIX)));
    }
    let who = name.trim_start_matches(PREFIX);
    // Unlike `now`, which can report 0 for what it cannot read, a seek from a wrong place or for an
    // unknown song goes to the wrong point: an error that could not be read is not "no trackid".
    let metadata: HashMap<String, OwnedValue> = p.get_property("Metadata").map_err(|e| format!("{who} does not say what it is playing: {e}"))?;
    let length = metadata.get("mpris:length").map(seconds).unwrap_or(0.0);
    let track = metadata.get("mpris:trackid").and_then(track_id);
    // Where it is matters to a step, and to a `Seek`, which goes by the difference.
    let now = if relative || track.is_none() {
        p.get_property::<OwnedValue>("Position").map(|v| seconds(&v)).map_err(|e| format!("{who} does not say where it is: {e}"))?
    } else {
        0.0
    };
    let to = if relative { now + s } else { s };
    let sent = match jump(to, now, length, track) {
        Jump::To(track, us) => p.call_method("SetPosition", &(track, us)),
        Jump::By(us) => p.call_method("Seek", &(us,)),
    };
    sent.map_err(|e| e.to_string())?;
    // A player that does not send `Seeked` would leave the scene's position where it was.
    wake();
    Ok(())
}

/// The cover as something a scene can paint: a `file://` URL as its path,
/// percent-decoding and all, and a `data:` one (a browser's stream hands its
/// picture over inline) decoded once into a file named after what it holds, the
/// way a notification's pixels are. Anything else —`http(s)`— is left as the
/// player said it: fetching from the network is the scene's to decide.
fn art_path(url: &str) -> String {
    let base = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().into_owned());
    art_path_in(url, &std::path::Path::new(&base).join("pleamar").join("media"))
}

/// The same, writing a `data:` picture into `dir`.
fn art_path_in(url: &str, dir: &std::path::Path) -> String {
    if let Some(rest) = url.strip_prefix("file://") {
        return percent_decoded(rest);
    }
    let Some(rest) = url.strip_prefix("data:") else { return url.to_owned() };
    let Some((head, payload)) = rest.split_once(',') else { return String::new() };
    let Some(mime) = head.strip_suffix(";base64") else { return String::new() };
    let Some(bytes) = base64(payload) else { return String::new() };
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    // Raster pictures are told apart by what they hold; an svg, by its name.
    let ext = if mime.contains("svg") { "svg" } else { "img" };
    let path = dir.join(format!("{:016x}.{ext}", hasher.finish()));
    // A player that keeps the same picture for many songs costs one write.
    if !path.exists() && (std::fs::create_dir_all(dir).is_err() || std::fs::write(&path, &bytes).is_err()) {
        return String::new();
    }
    path.to_string_lossy().into_owned()
}

/// `%20` back to a space; a `%` that is not followed by two hex digits stays.
fn percent_decoded(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |k: usize| b.get(k).and_then(|c| (*c as char).to_digit(16));
        match (b[i], hex(i + 1), hex(i + 2)) {
            (b'%', Some(h), Some(l)) => {
                out.push((h * 16 + l) as u8);
                i += 3;
            }
            (c, _, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Standard base64 (and its URL-safe letters), padding optional, spaces ignored.
fn base64(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' | b' ' | b'\n' | b'\r' => continue,
            _ => return None,
        } as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_seek_goes_to_the_track_inside_the_song_or_by_the_difference() {
        let track = || Some(OwnedObjectPath::try_from("/org/mpris/MediaPlayer2/Track/7").unwrap());
        let path = |p: &str| OwnedObjectPath::try_from(p).unwrap();
        assert_eq!(jump(42.5, 10.0, 300.0, track()), Jump::To(path("/org/mpris/MediaPlayer2/Track/7"), 42_500_000));
        // Past either end it stays inside the song: a `Seek` past the end would go to the next one.
        assert_eq!(jump(-3.0, 10.0, 300.0, track()), Jump::To(path("/org/mpris/MediaPlayer2/Track/7"), 0));
        assert_eq!(jump(400.0, 10.0, 300.0, track()), Jump::To(path("/org/mpris/MediaPlayer2/Track/7"), 300_000_000));
        // Without an id, by the difference; a song with no length is not held at 0.
        assert_eq!(jump(42.5, 10.0, 300.0, None), Jump::By(32_500_000));
        assert_eq!(jump(5.0, 10.0, 300.0, None), Jump::By(-5_000_000));
        assert_eq!(jump(90.0, 10.0, 0.0, None), Jump::By(80_000_000));
        assert_eq!(jump(-1.0, 10.0, 0.0, None), Jump::By(-10_000_000));
    }

    #[test]
    fn a_seek_by_the_difference_stops_short_of_the_end() {
        // The song plays on between reading `now` and the `Seek`: the full difference could carry it over.
        assert_eq!(jump(400.0, 10.0, 300.0, None), Jump::By(289_500_000));
        assert_eq!(jump(300.0, 10.0, 300.0, None), Jump::By(289_500_000));
        assert_eq!(jump(5.0, 0.0, 0.3, None), Jump::By(0));
    }

    #[test]
    fn a_track_id_is_an_object_path_or_text_that_is_one() {
        let v = |x: zbus::zvariant::Value| OwnedValue::try_from(x).unwrap();
        assert_eq!(track_id(&v(ObjectPath::try_from("/com/spotify/track/abc").unwrap().into())).unwrap().as_str(), "/com/spotify/track/abc");
        assert_eq!(track_id(&v("/org/mpd/Tracks/12".into())).unwrap().as_str(), "/org/mpd/Tracks/12");
        assert_eq!(track_id(&v("spotify:track:abc".into())), None);
        assert_eq!(track_id(&v(ObjectPath::try_from("/org/mpris/MediaPlayer2/TrackList/NoTrack").unwrap().into())), None);
    }

    #[test]
    fn a_cover_url_becomes_a_path_a_scene_can_paint() {
        assert_eq!(art_path("file:///home/me/My%20Music/cover%2B1.jpg"), "/home/me/My Music/cover+1.jpg");
        assert_eq!(art_path("https://example.org/c.jpg"), "https://example.org/c.jpg");
        assert_eq!(base64("aGVsbG8gd29ybGQ=").unwrap(), b"hello world");
        assert_eq!(base64("aGk").unwrap(), b"hi");
        assert!(base64("a*b").is_none());
        assert_eq!(art_path("data:image/png,not-base64"), "");
        // A data: picture is written once, under a name taken from what it holds.
        let dir = std::env::temp_dir().join(format!("pleamar-art-{}", std::process::id()));
        let p = art_path_in("data:image/png;base64,iVBORw0KGgo=", &dir);
        assert!(p.starts_with(&dir.to_string_lossy().into_owned()) && p.ends_with(".img"));
        assert_eq!(std::fs::read(&p).unwrap(), b"\x89PNG\r\n\x1a\n");
        assert_eq!(art_path_in("data:image/png;base64,iVBORw0KGgo=", &dir), p);
        assert!(art_path_in("data:image/svg+xml;base64,PHN2Zy8+", &dir).ends_with(".svg"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
