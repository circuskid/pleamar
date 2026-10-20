//! Bluetooth, through BlueZ on the system bus: told of changes, not polled.
//!
//! `{ present, powered, discovering, devices = { { name, address, paired,
//!    connected, battery, icon }, … } }`: the first adapter, and the devices it
//! knows or sees, the connected first, then the paired. `battery` is 0..1, or
//! -1 when the device does not say. With no adapter, `{ present = false }`.
//! And what pairing asks of whoever sits there: `pairing` is `none`, or `pin`
//! or `passkey` (a code is to be typed: one the device shows, or the one typed
//! on it too), `confirm` (does `pairing_code` match the one the device shows?),
//! `display` (type `pairing_code` on the device) or `authorize` (let it pair at
//! all); `pairing_address` and `pairing_name` say which device.
//!
//! Commands (`bluetooth.*`): `power(on)`, `scan(on)`, `connect(address)` —it
//! pairs first if it has to, and trusts it—, `disconnect(address)`,
//! `forget(address)`. Connecting takes seconds: it goes on its own thread, and
//! the change arrives through the service. While pairing: `answer(code)` to a
//! `pin` or a `passkey`, `confirm()` to a `confirm` or an `authorize`, and
//! `cancel()`, which says no to what is asked and gives up on the pairing.

use super::SysValue;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;
use zbus::blocking::{Connection, MessageIterator, Proxy};
use zbus::zvariant::{OwnedObjectPath, OwnedValue};
use zbus::MatchRule;

const BLUEZ: &str = "org.bluez";
const ADAPTER: &str = "org.bluez.Adapter1";
const DEVICE: &str = "org.bluez.Device1";
const BATTERY: &str = "org.bluez.Battery1";

type Objects = HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>>;

fn objects(c: &Connection) -> Option<Objects> {
    Proxy::new(c, BLUEZ, "/", "org.freedesktop.DBus.ObjectManager").ok()?.call("GetManagedObjects", &()).ok()
}

fn adapter(o: &Objects) -> Option<(&OwnedObjectPath, &HashMap<String, OwnedValue>)> {
    let mut all: Vec<_> = o.iter().filter_map(|(p, i)| i.get(ADAPTER).map(|a| (p, a))).collect();
    all.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
    all.into_iter().next()
}

fn flag(m: &HashMap<String, OwnedValue>, k: &str) -> bool {
    m.get(k).and_then(|v| bool::try_from(v).ok()).unwrap_or(false)
}

fn word(m: &HashMap<String, OwnedValue>, k: &str) -> String {
    m.get(k).and_then(|v| <&str>::try_from(v).ok()).unwrap_or_default().to_owned()
}

pub fn available() -> bool {
    Connection::system().ok().and_then(|c| objects(&c)).is_some_and(|o| adapter(&o).is_some())
}

fn now(c: &Connection) -> SysValue {
    let Some(o) = objects(c) else { return SysValue::Map(vec![("present".into(), SysValue::Bool(false))]) };
    let Some((adapter_path, a)) = adapter(&o) else { return SysValue::Map(vec![("present".into(), SysValue::Bool(false))]) };
    let mut devices: Vec<(bool, bool, SysValue)> = o
        .iter()
        .filter(|(p, _)| p.as_str().starts_with(adapter_path.as_str()))
        .filter_map(|(_, i)| {
            let d = i.get(DEVICE)?;
            let paired = flag(d, "Paired");
            let connected = flag(d, "Connected");
            let address = word(d, "Address");
            let name = name(d);
            // A device that has not even said its name and was never paired is noise.
            if name.is_empty() && !paired {
                return None;
            }
            let name = if name.is_empty() { address.clone() } else { name };
            let battery = i.get(BATTERY).and_then(|b| b.get("Percentage")).and_then(|v| u8::try_from(v).ok()).map_or(-1.0, |p| p as f64 / 100.0);
            Some((
                connected,
                paired,
                SysValue::Map(vec![
                    ("name".into(), SysValue::Text(name)),
                    ("address".into(), SysValue::Text(address.clone())),
                    ("paired".into(), SysValue::Bool(paired)),
                    ("connected".into(), SysValue::Bool(connected)),
                    ("battery".into(), SysValue::Num(battery)),
                    ("icon".into(), SysValue::Text(word(d, "Icon"))),
                ]),
            ))
        })
        .collect();
    devices.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(format!("{:?}", a.2).cmp(&format!("{:?}", b.2))));
    devices.truncate(24);
    let mut v = vec![
        ("present".into(), SysValue::Bool(true)),
        ("powered".into(), SysValue::Bool(flag(a, "Powered"))),
        ("discovering".into(), SysValue::Bool(flag(a, "Discovering"))),
        ("devices".into(), SysValue::List(devices.into_iter().map(|(_, _, v)| v).collect())),
    ];
    v.extend(pairing(&o));
    SysValue::Map(v)
}

pub fn service(dispatch: Box<dyn Fn(SysValue) + Send>) -> bool {
    let Ok(c) = Connection::system() else { return false };
    std::thread::Builder::new()
        .name("bluetooth".into())
        .spawn(move || {
            let mut last = String::new();
            let mut report = |c: &Connection| {
                let v = now(c);
                let fingerprint = format!("{v:?}");
                if fingerprint != last {
                    last = fingerprint;
                    dispatch(v);
                }
            };
            report(&c);
            let (tx, rx) = std::sync::mpsc::channel::<()>();
            // What pairing asks changes nothing BlueZ signals: the agent says so here.
            WATCHERS.lock().unwrap().push(tx.clone());
            if let Ok(rule) = MatchRule::builder().msg_type(zbus::message::Type::Signal).sender(BLUEZ).map(|b| b.build()) {
                let (c, tx) = (c.clone(), tx.clone());
                std::thread::spawn(move || {
                    let Ok(messages) = MessageIterator::for_match_rule(rule, &c, Some(64)) else { return };
                    for _ in messages {
                        if tx.send(()).is_err() {
                            break;
                        }
                    }
                });
            }
            loop {
                match rx.recv_timeout(Duration::from_secs(30)) {
                    Ok(()) => {
                        // Discovering is a stream of devices appearing: told in batches.
                        std::thread::sleep(Duration::from_millis(300));
                        while rx.try_recv().is_ok() {}
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
                report(&c);
            }
        })
        .is_ok()
}

fn device_path(o: &Objects, address: &str) -> Option<OwnedObjectPath> {
    o.iter().find(|(_, i)| i.get(DEVICE).is_some_and(|d| word(d, "Address").eq_ignore_ascii_case(address))).map(|(p, _)| p.clone())
}

/// What pairing asks of whoever sits there.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    /// A PIN to type, for the old pairing: up to 16 characters.
    Pin,
    /// Six digits to type: the ones the device shows, or typed on it too.
    Passkey,
    /// Do the six digits match the ones the device shows?
    Confirm,
    /// Type these on the device: nothing is answered here.
    Display,
    /// May it pair at all? (A device with nothing to show or type.)
    Authorize,
}

impl Kind {
    fn word(self) -> &'static str {
        match self {
            Kind::Pin => "pin",
            Kind::Passkey => "passkey",
            Kind::Confirm => "confirm",
            Kind::Display => "display",
            Kind::Authorize => "authorize",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Answer {
    Code(String),
    Yes,
    No,
}

/// An answer BlueZ's call waits on, given from another thread: the first one counts.
#[derive(Default)]
struct Reply(Mutex<(Option<Answer>, Option<Waker>)>);

impl Reply {
    fn give(&self, answer: Answer) {
        let mut r = self.0.lock().unwrap();
        if r.0.is_none() {
            r.0 = Some(answer);
            if let Some(w) = r.1.take() {
                w.wake();
            }
        }
    }
}

struct Wait(Arc<Reply>);

impl Future for Wait {
    type Output = Answer;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Answer> {
        let mut r = self.0 .0.lock().unwrap();
        match r.0.clone() {
            Some(answer) => Poll::Ready(answer),
            None => {
                r.1 = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

/// What is being asked now: one thing at a time, as BlueZ asks an agent.
struct Asked {
    kind: Kind,
    device: OwnedObjectPath,
    code: String,
    /// Whoever waits for the answer; none for what is only shown.
    reply: Option<Arc<Reply>>,
}

static ASKED: Mutex<Option<Asked>> = Mutex::new(None);
/// The services watching, to be told when that changes: BlueZ signals nothing of it.
static WATCHERS: Mutex<Vec<Sender<()>>> = Mutex::new(Vec::new());

fn tell() {
    WATCHERS.lock().unwrap().retain(|w| w.send(()).is_ok());
}

/// The device's own name, or an empty one: with none of its own, BlueZ's alias
/// is its address with dashes («45-1B-85-DD-04-24»), and that is no name.
fn name(d: &HashMap<String, OwnedValue>) -> String {
    let address = word(d, "Address");
    let no_name = |n: &String| n.is_empty() || n.replace('-', ":").eq_ignore_ascii_case(&address);
    [word(d, "Alias"), word(d, "Name")].into_iter().find(|n| !no_name(n)).unwrap_or_default()
}

/// The service's pairing fields, read from what is being asked now.
fn pairing(o: &Objects) -> Vec<(String, SysValue)> {
    let asked = ASKED.lock().unwrap();
    let device = asked.as_ref().and_then(|a| o.get(&a.device)).and_then(|i| i.get(DEVICE));
    let address = device.map(|d| word(d, "Address")).unwrap_or_default();
    let name = device.map(name).filter(|n| !n.is_empty()).unwrap_or_else(|| address.clone());
    vec![
        ("pairing".into(), SysValue::Text(asked.as_ref().map_or("none", |a| a.kind.word()).into())),
        ("pairing_address".into(), SysValue::Text(address)),
        ("pairing_name".into(), SysValue::Text(name)),
        ("pairing_code".into(), SysValue::Text(asked.as_ref().map(|a| a.code.clone()).unwrap_or_default())),
    ]
}

/// Puts something new to whoever sits there. What was asked before and not
/// answered is said no to: BlueZ asks one thing at a time, so it is over.
fn put(kind: Kind, device: OwnedObjectPath, code: String, reply: Option<Arc<Reply>>) {
    let before = ASKED.lock().unwrap().replace(Asked { kind, device, code, reply });
    if let Some(r) = before.and_then(|b| b.reply) {
        r.give(Answer::No);
    }
    tell();
}

/// Asks, and waits for the answer however long it takes: BlueZ gives up on its
/// own after a while, and says so with `Cancel`.
async fn ask(kind: Kind, device: OwnedObjectPath, code: String) -> Answer {
    let reply = Arc::new(Reply::default());
    put(kind, device, code, Some(reply.clone()));
    let answer = Wait(reply.clone()).await;
    {
        let mut asked = ASKED.lock().unwrap();
        if asked.as_ref().and_then(|a| a.reply.as_ref()).is_some_and(|r| Arc::ptr_eq(r, &reply)) {
            *asked = None;
        }
    }
    tell();
    answer
}

/// Nothing is asked any more (about that device, if it says which): whoever
/// waited is said no to. What was asked, if anything.
fn withdraw(device: Option<&OwnedObjectPath>) -> Option<Asked> {
    let asked = {
        let mut a = ASKED.lock().unwrap();
        if device.is_some_and(|d| a.as_ref().is_some_and(|a| a.device != *d)) {
            return None;
        }
        a.take()?
    };
    if let Some(r) = &asked.reply {
        r.give(Answer::No);
    }
    tell();
    Some(asked)
}

/// What BlueZ is told of a request that did not end in a yes.
#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.bluez.Error")]
enum AgentError {
    #[zbus(error)]
    ZBus(zbus::Error),
    Rejected(String),
    Canceled(String),
}

fn refused(answer: Answer) -> AgentError {
    match answer {
        Answer::No => AgentError::Canceled("it was cancelled".into()),
        _ => AgentError::Rejected("that is not what was asked".into()),
    }
}

/// Only while pairing, and only for what this process asked for. With a scene
/// watching the service, what the device needs is put to it (`KeyboardDisplay`);
/// with none, nobody could answer, and it says yes as it always did
/// (`NoInputNoOutput`: what pairing a pair of headphones needs).
struct Agent {
    asks: bool,
}

#[zbus::interface(name = "org.bluez.Agent1")]
impl Agent {
    fn release(&self) {}

    async fn request_pin_code(&self, device: OwnedObjectPath) -> Result<String, AgentError> {
        if !self.asks {
            return Ok("0000".into());
        }
        match ask(Kind::Pin, device, String::new()).await {
            Answer::Code(pin) => Ok(pin),
            other => Err(refused(other)),
        }
    }

    fn display_pin_code(&self, device: OwnedObjectPath, pincode: String) {
        if self.asks {
            put(Kind::Display, device, pincode, None);
        }
    }

    async fn request_passkey(&self, device: OwnedObjectPath) -> Result<u32, AgentError> {
        if !self.asks {
            return Ok(0);
        }
        match ask(Kind::Passkey, device, String::new()).await {
            // Checked when it was given: six digits at most.
            Answer::Code(passkey) => passkey.parse().map_err(|_| refused(Answer::Yes)),
            other => Err(refused(other)),
        }
    }

    /// Called again with each key typed on the device (`entered`); the code is the same.
    fn display_passkey(&self, device: OwnedObjectPath, passkey: u32, _entered: u16) {
        if self.asks {
            put(Kind::Display, device, format!("{passkey:06}"), None);
        }
    }

    async fn request_confirmation(&self, device: OwnedObjectPath, passkey: u32) -> Result<(), AgentError> {
        if !self.asks {
            return Ok(());
        }
        match ask(Kind::Confirm, device, format!("{passkey:06}")).await {
            Answer::Yes => Ok(()),
            other => Err(refused(other)),
        }
    }

    async fn request_authorization(&self, device: OwnedObjectPath) -> Result<(), AgentError> {
        if !self.asks {
            return Ok(());
        }
        match ask(Kind::Authorize, device, String::new()).await {
            Answer::Yes => Ok(()),
            other => Err(refused(other)),
        }
    }

    /// A service of the device being paired, which this process asked for: yes.
    fn authorize_service(&self, _device: OwnedObjectPath, _uuid: String) {}

    /// BlueZ gave up on what it asked: the device went, or nobody answered in time.
    fn cancel(&self) {
        withdraw(None);
    }
}

const AGENT_PATH: &str = "/org/pleamar/bluetooth_agent";

fn connect(address: String) -> Result<(), String> {
    let asks = !WATCHERS.lock().unwrap().is_empty();
    let c = zbus::blocking::connection::Builder::system().and_then(|b| b.serve_at(AGENT_PATH, Agent { asks })).and_then(|b| b.build()).map_err(|e| e.to_string())?;
    let o = objects(&c).ok_or("BlueZ does not answer")?;
    let path = device_path(&o, &address).ok_or("that device is not known: scan first")?;
    let device = Proxy::new(&c, BLUEZ, path.as_str(), DEVICE).map_err(|e| e.to_string())?;
    if !device.get_property::<bool>("Paired").unwrap_or(false) {
        let manager = Proxy::new(&c, BLUEZ, "/org/bluez", "org.bluez.AgentManager1").map_err(|e| e.to_string())?;
        let agent = OwnedObjectPath::try_from(AGENT_PATH).map_err(|e| e.to_string())?;
        let _ = manager.call_method("RegisterAgent", &(agent.clone(), if asks { "KeyboardDisplay" } else { "NoInputNoOutput" }));
        let paired = device.call_method("Pair", &());
        let _ = manager.call_method("UnregisterAgent", &(agent,));
        // Paired or not, what was shown for it is over.
        withdraw(Some(&path));
        paired.map_err(|e| format!("could not pair: {e}"))?;
        // Trusted: it may connect again by itself.
        let _ = device.set_property("Trusted", true);
    }
    device.call_method("Connect", &()).map(|_| ()).map_err(|e| format!("could not connect: {e}"))
}

/// The answers to what pairing asks, which need nothing of BlueZ but the last.
fn reply(what: &str, args: &[SysValue]) -> Option<Result<(), String>> {
    let give = |kinds: &[Kind], answer: Answer| {
        let asked = ASKED.lock().unwrap();
        let a = asked.as_ref().filter(|a| kinds.contains(&a.kind)).ok_or_else(|| format!("nothing is being asked that takes {what}"))?;
        a.reply.as_ref().ok_or("nobody is waiting for that answer")?.give(answer);
        Ok(())
    };
    Some(match (what, args) {
        ("bluetooth.answer", [code]) => {
            let code = match code {
                SysValue::Text(t) => t.trim().to_owned(),
                SysValue::Num(n) if *n >= 0.0 && n.fract() == 0.0 => format!("{n}"),
                _ => return Some(Err("bluetooth.answer takes the code, as text".into())),
            };
            let kind = ASKED.lock().unwrap().as_ref().map(|a| a.kind);
            match kind {
                Some(Kind::Pin) if code.is_empty() || code.len() > 16 => Err("a PIN is 1 to 16 characters".into()),
                Some(Kind::Passkey) if code.is_empty() || code.len() > 6 || !code.bytes().all(|b| b.is_ascii_digit()) => Err("a passkey is up to six digits".into()),
                _ => give(&[Kind::Pin, Kind::Passkey], Answer::Code(code)),
            }
        }
        ("bluetooth.confirm", []) => give(&[Kind::Confirm, Kind::Authorize], Answer::Yes),
        ("bluetooth.cancel", []) => {
            let asked = withdraw(None).ok_or_else(|| "nothing is being asked".to_owned());
            // What is only shown waits for nothing here: the pairing itself is called off.
            asked.and_then(|a| {
                let c = commands()?;
                let _ = Proxy::new(&c, BLUEZ, a.device.as_str(), DEVICE).map_err(|e| e.to_string())?.call_method("CancelPairing", &());
                Ok(())
            })
        }
        _ => return None,
    })
}

/// The connection commands go through, kept: BlueZ stops a discovery the
/// moment whoever asked for it leaves the bus, and a connection per command
/// left as soon as it had asked —«scan» started and stopped at once—.
fn commands() -> Result<Connection, String> {
    static KEPT: std::sync::OnceLock<Connection> = std::sync::OnceLock::new();
    if let Some(c) = KEPT.get() {
        return Ok(c.clone());
    }
    let c = Connection::system().map_err(|e| e.to_string())?;
    Ok(KEPT.get_or_init(|| c).clone())
}

pub fn command(what: &str, args: &[SysValue]) -> Result<(), String> {
    if let Some(done) = reply(what, args) {
        return done;
    }
    let c = commands()?;
    let o = objects(&c).ok_or("BlueZ is not running")?;
    let (adapter_path, _) = adapter(&o).ok_or("there is no Bluetooth adapter")?;
    let adapter = Proxy::new(&c, BLUEZ, adapter_path.as_str(), ADAPTER).map_err(|e| e.to_string())?;
    let err = |e: zbus::Error| e.to_string();
    match (what, args) {
        ("bluetooth.power", [SysValue::Bool(on)]) => adapter.set_property("Powered", *on).map_err(|e| e.to_string()),
        ("bluetooth.scan", [SysValue::Bool(on)]) => adapter.call_method(if *on { "StartDiscovery" } else { "StopDiscovery" }, &()).map(|_| ()).map_err(err),
        ("bluetooth.connect", [SysValue::Text(address)]) => {
            let address = address.clone();
            // Pairing waits for the device: seconds, not for the logic to wait.
            std::thread::Builder::new()
                .name("bluetooth connect".into())
                .spawn(move || {
                    if let Err(e) = connect(address.clone()) {
                        eprintln!("bluetooth · {address}: {e}");
                    }
                })
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        ("bluetooth.disconnect", [SysValue::Text(address)]) => {
            let path = device_path(&o, address).ok_or("that device is not known")?;
            Proxy::new(&c, BLUEZ, path.as_str(), DEVICE).map_err(|e| e.to_string())?.call_method("Disconnect", &()).map(|_| ()).map_err(err)
        }
        ("bluetooth.forget", [SysValue::Text(address)]) => {
            let path = device_path(&o, address).ok_or("that device is not known")?;
            adapter.call_method("RemoveDevice", &(path,)).map(|_| ()).map_err(err)
        }
        _ => Err(format!("'{what}' is not asked like that: bluetooth.power(true|false), bluetooth.scan(true|false), bluetooth.connect(address), bluetooth.disconnect(address), bluetooth.forget(address), bluetooth.answer(code), bluetooth.confirm(), bluetooth.cancel()")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::Value;

    const DEV: &str = "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF";

    fn known() -> Objects {
        let device: HashMap<String, OwnedValue> = [("Address", "AA:BB:CC:DD:EE:FF"), ("Alias", "Keys")].into_iter().map(|(k, v)| (k.to_owned(), OwnedValue::try_from(Value::from(v)).unwrap())).collect();
        HashMap::from([(OwnedObjectPath::try_from(DEV).unwrap(), HashMap::from([(DEVICE.to_owned(), device)]))])
    }

    fn field(o: &Objects, name: &str) -> String {
        match pairing(o).into_iter().find(|(k, _)| k == name).map(|(_, v)| v) {
            Some(SysValue::Text(t)) => t,
            other => panic!("{name}: {other:?}"),
        }
    }

    /// What BlueZ asks, on its own thread as the bus would, until it is put to the scene.
    fn asked(kind: Kind, code: &str) -> std::thread::JoinHandle<Answer> {
        let code = code.to_owned();
        let waiting = std::thread::spawn(move || zbus::block_on(ask(kind, OwnedObjectPath::try_from(DEV).unwrap(), code)));
        while ASKED.lock().unwrap().is_none() {
            std::thread::yield_now();
        }
        waiting
    }

    #[test]
    fn a_number_to_compare_is_shown_and_the_yes_goes_back() {
        let _one_at_a_time = ONE.lock().unwrap();
        let waiting = asked(Kind::Confirm, "012345");
        let o = known();
        assert_eq!((field(&o, "pairing").as_str(), field(&o, "pairing_code").as_str()), ("confirm", "012345"));
        assert_eq!((field(&o, "pairing_address").as_str(), field(&o, "pairing_name").as_str()), ("AA:BB:CC:DD:EE:FF", "Keys"));
        // Not what is asked: it waits on.
        assert!(command("bluetooth.answer", &[SysValue::Text("012345".into())]).is_err());
        command("bluetooth.confirm", &[]).unwrap();
        assert_eq!(waiting.join().unwrap(), Answer::Yes);
        assert_eq!(field(&o, "pairing"), "none");
    }

    #[test]
    fn a_passkey_is_checked_before_it_is_given() {
        let _one_at_a_time = ONE.lock().unwrap();
        let waiting = asked(Kind::Passkey, "");
        for wrong in ["", "abc", "1234567"] {
            assert!(command("bluetooth.answer", &[SysValue::Text(wrong.into())]).is_err(), "{wrong}");
        }
        assert!(command("bluetooth.confirm", &[]).is_err());
        command("bluetooth.answer", &[SysValue::Text(" 004321 ".into())]).unwrap();
        assert_eq!(waiting.join().unwrap(), Answer::Code("004321".into()));
        assert!(command("bluetooth.answer", &[SysValue::Text("1".into())]).is_err(), "nothing is asked any more");
    }

    #[test]
    fn whoever_waits_is_told_no_when_bluez_gives_up_or_something_else_is_asked() {
        let _one_at_a_time = ONE.lock().unwrap();
        let waiting = asked(Kind::Pin, "");
        // BlueZ's `Cancel`: nobody answered in time.
        assert!(withdraw(None).is_some());
        assert_eq!(waiting.join().unwrap(), Answer::No);
        let waiting = asked(Kind::Authorize, "");
        put(Kind::Display, OwnedObjectPath::try_from(DEV).unwrap(), "123456".into(), None);
        assert_eq!(waiting.join().unwrap(), Answer::No);
        assert_eq!(field(&known(), "pairing"), "display");
        // The pairing of another device ends: this one is still shown.
        assert!(withdraw(Some(&OwnedObjectPath::try_from("/org/bluez/hci0/dev_00").unwrap())).is_none());
        assert!(withdraw(Some(&OwnedObjectPath::try_from(DEV).unwrap())).is_some());
        assert_eq!(field(&known(), "pairing"), "none");
    }

    /// These share the one thing a process is asked at a time: one at a time.
    static ONE: Mutex<()> = Mutex::new(());
}
