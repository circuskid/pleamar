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
//! all); `pairing_id` says which question it is, `pairing_address` and
//! `pairing_name` which device. Only a scene that declares `pairing` among the
//! service's fields is asked (`handles`); with none, pairing asks nobody, as
//! before.
//!
//! Commands (`bluetooth.*`): `power(on)`, `scan(on)`, `connect(address)` —it
//! pairs first if it has to, and trusts it; one pairing at a time—,
//! `disconnect(address)`, `forget(address)`. Connecting takes seconds: it goes
//! on its own thread, and the change arrives through the service. While
//! pairing, each naming the `pairing_id` it answers: `answer(id, code)` to a
//! `pin` or a `passkey`, `confirm(id)` to a `confirm` or an `authorize`, and
//! `cancel(id)`, which says no to what is asked and gives up on the pairing.

use super::SysValue;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;
use zbus::blocking::{Connection, MessageIterator, Proxy};
use zbus::message::Header;
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

/// No adapter, or no BlueZ: and nothing asked either, or a scene would go on
/// showing a question nobody can answer any more.
fn absent() -> SysValue {
    let mut v = vec![("present".into(), SysValue::Bool(false))];
    v.extend(["pairing", "pairing_address", "pairing_name", "pairing_code"].map(|k| (k.into(), SysValue::Text(if k == "pairing" { "none" } else { "" }.into()))));
    v.push(("pairing_id".into(), SysValue::Num(0.0)));
    SysValue::Map(v)
}

fn now(c: &Connection) -> SysValue {
    let Some(o) = objects(c) else { return absent() };
    let Some((adapter_path, a)) = adapter(&o) else { return absent() };
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

/// The receiver is told when what is asked changes: BlueZ signals nothing of it.
fn watch() -> (Sender<()>, Receiver<()>) {
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    WATCHERS.lock().unwrap().push(tx.clone());
    (tx, rx)
}

/// The scenes that show what pairing asks, and answer it. Watching the service
/// is not saying that: a scene that only shows the state (a bar's icon) would
/// turn a pairing that just works into a number to compare that nobody sees.
static HANDLERS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Whether `who` shows what pairing asks. When the last one that did no longer
/// does, what it was asked is over.
pub fn handles(who: &str, on: bool) {
    let none_left = {
        let mut h = HANDLERS.lock().unwrap();
        let before = h.len();
        h.retain(|w| w != who);
        if on {
            h.push(who.to_owned());
        }
        before > 0 && h.is_empty()
    };
    if none_left {
        // A code that is only shown waits for no answer: the pairing itself is called off.
        if let Some(a) = withdraw(|_| true, Answer::Over).filter(|a| a.reply.is_none()) {
            call_off(a.device);
        }
    }
}

/// Whether a pairing can ask anyone.
fn asks() -> bool {
    !HANDLERS.lock().unwrap().is_empty()
}

pub fn service(dispatch: Box<dyn Fn(SysValue) + Send>) -> bool {
    let Ok(c) = Connection::system() else { return false };
    let (tx, rx) = watch();
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
            if let Ok(rule) = MatchRule::builder().msg_type(zbus::message::Type::Signal).sender(BLUEZ).map(|b| b.build()) {
                let c = c.clone();
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
    /// Whoever sits there said no (`bluetooth.cancel`).
    No,
    /// Nobody said anything, and it is over: something else was asked, BlueZ
    /// gave up, the pairing ended or took too long, or no scene shows it any more.
    Over,
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
    /// Which question it is (`pairing_id`): an answer names it, so one meant
    /// for a question that is over does not answer the next.
    id: u64,
    kind: Kind,
    device: OwnedObjectPath,
    /// The `connect` whose pairing asks: only it, or BlueZ through its agent, ends it.
    attempt: u64,
    /// Kept from when it was asked, should the device go before it is answered.
    address: String,
    name: String,
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
    let mut asked = ASKED.lock().unwrap();
    // Its name is remembered while it is there to be read.
    if let Some(a) = asked.as_mut()
        && let Some(n) = o.get(&a.device).and_then(|i| i.get(DEVICE)).map(name).filter(|n| !n.is_empty())
    {
        a.name = n;
    }
    let address = asked.as_ref().map(|a| a.address.clone()).unwrap_or_default();
    let name = asked.as_ref().map(|a| a.name.clone()).filter(|n| !n.is_empty()).unwrap_or_else(|| address.clone());
    vec![
        ("pairing".into(), SysValue::Text(asked.as_ref().map_or("none", |a| a.kind.word()).into())),
        ("pairing_id".into(), SysValue::Num(asked.as_ref().map_or(0, |a| a.id) as f64)),
        ("pairing_address".into(), SysValue::Text(address)),
        ("pairing_name".into(), SysValue::Text(name)),
        ("pairing_code".into(), SysValue::Text(asked.as_ref().map(|a| a.code.clone()).unwrap_or_default())),
    ]
}

/// The address in a device's path (`…/dev_AA_BB_CC_DD_EE_FF`), which BlueZ
/// makes from it.
fn address_of(device: &OwnedObjectPath) -> String {
    device.as_str().rsplit('/').next().and_then(|last| last.strip_prefix("dev_")).map(|a| a.replace('_', ":")).unwrap_or_default()
}

/// Puts something new to whoever sits there, and says which question it is.
/// What was asked before and not answered is over: one question is shown at a time.
fn put(attempt: u64, kind: Kind, device: OwnedObjectPath, code: String, reply: Option<Arc<Reply>>) -> u64 {
    static QUESTIONS: AtomicU64 = AtomicU64::new(1);
    let id = QUESTIONS.fetch_add(1, Ordering::Relaxed);
    let address = address_of(&device);
    let before = ASKED.lock().unwrap().replace(Asked { id, kind, device, attempt, address, name: String::new(), code, reply });
    if let Some(r) = before.and_then(|b| b.reply) {
        r.give(Answer::Over);
    }
    tell();
    // The last scene that shows it may have gone between the caller's `asks()`
    // and now; `handles` found nothing to withdraw then, so it is withdrawn here.
    if !asks()
        && let Some(a) = withdraw(|a| a.id == id, Answer::Over).filter(|a| a.reply.is_none())
    {
        call_off(a.device);
    }
    id
}

/// Asks, and waits for the answer, which may be that it is over: BlueZ gave up
/// (`Cancel`), the pairing took too long (`deadline`), or no scene shows it any
/// more. With none showing it, it is over before it is asked.
async fn ask(attempt: u64, kind: Kind, device: OwnedObjectPath, code: String) -> Answer {
    if !asks() {
        return Answer::Over;
    }
    let reply = Arc::new(Reply::default());
    let id = put(attempt, kind, device, code, Some(reply.clone()));
    let answer = Wait(reply).await;
    {
        let mut asked = ASKED.lock().unwrap();
        if asked.as_ref().is_some_and(|a| a.id == id) {
            *asked = None;
        }
    }
    tell();
    answer
}

/// What is asked is no longer asked, if it is `which`: whoever waited is given
/// `answer`. What was asked, if anything.
fn withdraw(which: impl Fn(&Asked) -> bool, answer: Answer) -> Option<Asked> {
    let asked = {
        let mut a = ASKED.lock().unwrap();
        if !a.as_ref().is_some_and(&which) {
            return None;
        }
        a.take()?
    };
    if let Some(r) = &asked.reply {
        r.give(answer);
    }
    tell();
    Some(asked)
}

/// Calls the device's pairing off: for a code that is only shown, which waits
/// for no answer of ours that could say no. On its own thread: a BlueZ that
/// does not answer would hold the logic for as long as a call may wait.
fn call_off(device: OwnedObjectPath) {
    std::thread::spawn(move || {
        if let Ok(c) = commands() {
            let _ = Proxy::new(&c, BLUEZ, device.as_str(), DEVICE).and_then(|d| d.call_method("CancelPairing", &()));
        }
    });
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
        Answer::No => AgentError::Rejected("whoever sits there said no".into()),
        Answer::Over => AgentError::Canceled("it was cancelled".into()),
        _ => AgentError::Rejected("that is not what was asked".into()),
    }
}

/// Only while pairing, and only for what this process asked for. With a scene
/// that shows what pairing asks, what the device needs is put to it
/// (`KeyboardDisplay`); with none, nobody could answer, and it says yes as it
/// always did (`NoInputNoOutput`: what pairing a pair of headphones needs).
struct Agent {
    asks: bool,
    /// Which `connect` this agent pairs for.
    attempt: u64,
    /// BlueZ's unique name on the bus: the only one that asks an agent.
    bluez: String,
}

impl Agent {
    /// Anyone on the bus can call an object it finds there: one that is not
    /// BlueZ could put a question of its own to the scene, or end one.
    fn sent_by_bluez(&self, sender: Option<&str>) -> Result<(), AgentError> {
        if sender == Some(self.bluez.as_str()) {
            Ok(())
        } else {
            Err(AgentError::Rejected("only BlueZ asks this agent".into()))
        }
    }

    fn check(&self, h: &Header<'_>) -> Result<(), AgentError> {
        self.sent_by_bluez(h.sender().map(|s| s.as_str()))
    }
}

#[zbus::interface(name = "org.bluez.Agent1")]
impl Agent {
    fn release(&self, #[zbus(header)] h: Header<'_>) -> Result<(), AgentError> {
        self.check(&h)
    }

    async fn request_pin_code(&self, #[zbus(header)] h: Header<'_>, device: OwnedObjectPath) -> Result<String, AgentError> {
        self.check(&h)?;
        if !self.asks {
            return Ok("0000".into());
        }
        match ask(self.attempt, Kind::Pin, device, String::new()).await {
            Answer::Code(pin) => Ok(pin),
            other => Err(refused(other)),
        }
    }

    fn display_pin_code(&self, #[zbus(header)] h: Header<'_>, device: OwnedObjectPath, pincode: String) -> Result<(), AgentError> {
        self.check(&h)?;
        if self.asks && asks() {
            put(self.attempt, Kind::Display, device, pincode, None);
        }
        Ok(())
    }

    async fn request_passkey(&self, #[zbus(header)] h: Header<'_>, device: OwnedObjectPath) -> Result<u32, AgentError> {
        self.check(&h)?;
        if !self.asks {
            return Ok(0);
        }
        match ask(self.attempt, Kind::Passkey, device, String::new()).await {
            // Checked when it was given: six digits at most.
            Answer::Code(passkey) => passkey.parse().map_err(|_| refused(Answer::Yes)),
            other => Err(refused(other)),
        }
    }

    /// Called again with each key typed on the device (`entered`); the code is the same.
    fn display_passkey(&self, #[zbus(header)] h: Header<'_>, device: OwnedObjectPath, passkey: u32, _entered: u16) -> Result<(), AgentError> {
        self.check(&h)?;
        if self.asks && asks() {
            put(self.attempt, Kind::Display, device, format!("{passkey:06}"), None);
        }
        Ok(())
    }

    async fn request_confirmation(&self, #[zbus(header)] h: Header<'_>, device: OwnedObjectPath, passkey: u32) -> Result<(), AgentError> {
        self.check(&h)?;
        if !self.asks {
            return Ok(());
        }
        match ask(self.attempt, Kind::Confirm, device, format!("{passkey:06}")).await {
            Answer::Yes => Ok(()),
            other => Err(refused(other)),
        }
    }

    async fn request_authorization(&self, #[zbus(header)] h: Header<'_>, device: OwnedObjectPath) -> Result<(), AgentError> {
        self.check(&h)?;
        if !self.asks {
            return Ok(());
        }
        match ask(self.attempt, Kind::Authorize, device, String::new()).await {
            Answer::Yes => Ok(()),
            other => Err(refused(other)),
        }
    }

    /// A service of the device being paired, which this process asked for: yes.
    fn authorize_service(&self, #[zbus(header)] h: Header<'_>, _device: OwnedObjectPath, _uuid: String) -> Result<(), AgentError> {
        self.check(&h)
    }

    /// BlueZ gave up on what it asked this agent: the device went, or nobody
    /// answered a confirm or an authorize in time (src/agent.c, REQUEST_TIMEOUT:
    /// 60 s). A PIN or passkey not answered in time gets no `Cancel`: `Pair`
    /// fails, or the `deadline` comes, and `connect` withdraws it.
    fn cancel(&self, #[zbus(header)] h: Header<'_>) -> Result<(), AgentError> {
        self.check(&h)?;
        withdraw(|a| a.attempt == self.attempt, Answer::Over);
        Ok(())
    }
}

/// Where each `connect`'s agent is, one path per attempt: two at once under
/// one path, the second's `UnregisterAgent` could take the first's away.
const AGENT_PATH: &str = "/org/pleamar/bluetooth_agent";

/// How long a pairing may take before it is called off. Longer than BlueZ's
/// own 60 s for a confirm or an authorize, so that BlueZ ends those first; a
/// PIN or a passkey it does not time out, nor a `Pair` that never returns.
const PATIENCE: Duration = Duration::from_secs(90);

/// Ends the attempt's pairing if it still goes on after `patience`: what it
/// asks is over (BlueZ is told `Canceled`), and `then` calls the pairing off.
/// Dropping what it returns, it is not needed any more.
fn deadline(attempt: u64, patience: Duration, then: impl FnOnce() + Send + 'static) -> Sender<()> {
    let (done, wait) = std::sync::mpsc::channel::<()>();
    let _ = std::thread::Builder::new().name("bluetooth pairing".into()).spawn(move || {
        if matches!(wait.recv_timeout(patience), Err(std::sync::mpsc::RecvTimeoutError::Timeout)) {
            withdraw(|a| a.attempt == attempt, Answer::Over);
            then();
        }
    });
    done
}

/// The `connect` that pairs now, if one does.
static PAIRING: Mutex<Option<u64>> = Mutex::new(None);

/// One pairing at a time: a second one would end what the first asks before
/// anyone answers it. Held while the attempt pairs.
struct Pairing(u64);

impl Pairing {
    fn begin(attempt: u64) -> Result<Pairing, String> {
        let mut now = PAIRING.lock().unwrap();
        if now.is_some() {
            return Err("busy: a device is pairing already".into());
        }
        *now = Some(attempt);
        Ok(Pairing(attempt))
    }
}

impl Drop for Pairing {
    fn drop(&mut self) {
        let mut now = PAIRING.lock().unwrap();
        if *now == Some(self.0) {
            *now = None;
        }
    }
}

fn connect(address: String, attempt: u64, pairing: Option<Pairing>) -> Result<(), String> {
    // Its own connection, which goes with the agent on it; a call BlueZ never
    // answers ends a little after the `deadline` should have ended it.
    let c = zbus::blocking::connection::Builder::system().map(|b| b.method_timeout(PATIENCE + Duration::from_secs(10))).and_then(|b| b.build()).map_err(|e| e.to_string())?;
    let o = objects(&c).ok_or("BlueZ does not answer")?;
    let path = device_path(&o, &address).ok_or("that device is not known: scan first")?;
    let device = Proxy::new(&c, BLUEZ, path.as_str(), DEVICE).map_err(|e| e.to_string())?;
    if !device.get_property::<bool>("Paired").unwrap_or(false) {
        let _pairing = match pairing {
            Some(p) => p,
            None => Pairing::begin(attempt)?,
        };
        let asks = asks();
        let bluez = zbus::blocking::fdo::DBusProxy::new(&c)
            .and_then(|d| d.get_name_owner(zbus::names::BusName::try_from(BLUEZ)?).map_err(zbus::Error::from))
            .map_err(|e| format!("BlueZ is not on the bus: {e}"))?
            .to_string();
        let at = format!("{AGENT_PATH}/{attempt}");
        c.object_server().at(at.as_str(), Agent { asks, attempt, bluez }).map_err(|e| e.to_string())?;
        let manager = Proxy::new(&c, BLUEZ, "/org/bluez", "org.bluez.AgentManager1").map_err(|e| e.to_string())?;
        let agent = OwnedObjectPath::try_from(at).map_err(|e| e.to_string())?;
        // Without its agent, BlueZ would pair through another one, or through none.
        manager.call_method("RegisterAgent", &(agent.clone(), if asks { "KeyboardDisplay" } else { "NoInputNoOutput" })).map_err(|e| format!("BlueZ did not take the agent: {e}"))?;
        let stop = {
            let (c, path) = (c.clone(), path.clone());
            deadline(attempt, PATIENCE, move || {
                let _ = Proxy::new(&c, BLUEZ, path.as_str(), DEVICE).and_then(|d| d.call_method("CancelPairing", &()));
            })
        };
        let paired = device.call_method("Pair", &());
        drop(stop);
        // It goes with this connection anyway.
        let _ = manager.call_method("UnregisterAgent", &(agent,));
        // Paired or not, what was shown for it is over; only for it.
        withdraw(|a| a.attempt == attempt, Answer::Over);
        paired.map_err(|e| format!("could not pair: {e}"))?;
        // Trusted: it may connect again by itself.
        let _ = device.set_property("Trusted", true);
    }
    device.call_method("Connect", &()).map(|_| ()).map_err(|e| format!("could not connect: {e}"))
}

/// The answers to what pairing asks, which need nothing of BlueZ but the last.
/// Each names the question it answers, its `pairing_id`.
fn reply(what: &str, args: &[SysValue]) -> Option<Result<(), String>> {
    if !matches!(what, "bluetooth.answer" | "bluetooth.confirm" | "bluetooth.cancel") {
        return None;
    }
    let (id, args) = match args {
        [SysValue::Num(id), rest @ ..] if *id >= 1.0 && id.fract() == 0.0 => (*id as u64, rest),
        _ => return Some(Err(format!("{what} takes the pairing_id of the question it answers first"))),
    };
    // A question that is over answers nothing: not the one asked after it.
    let kind = ASKED.lock().unwrap().as_ref().filter(|a| a.id == id).map(|a| a.kind);
    let Some(kind) = kind else { return Some(Err("that question is not being asked any more".into())) };
    let give = |kinds: &[Kind], answer: Answer| {
        if !kinds.contains(&kind) {
            return Err(format!("{what} does not answer a {}", kind.word()));
        }
        let asked = ASKED.lock().unwrap();
        let a = asked.as_ref().filter(|a| a.id == id).ok_or("that question is not being asked any more")?;
        a.reply.as_ref().ok_or("nobody is waiting for that answer")?.give(answer);
        Ok(())
    };
    Some(match (what, args) {
        ("bluetooth.answer", [code]) => {
            let code = match code {
                SysValue::Text(t) => t.trim().to_owned(),
                // A number has no leading zeros, and a PIN's are part of it.
                SysValue::Num(_) if kind == Kind::Pin => return Some(Err("a PIN is given as text: \"0000\", not 0000".into())),
                SysValue::Num(n) if *n >= 0.0 && n.fract() == 0.0 => format!("{n}"),
                _ => return Some(Err("bluetooth.answer takes the code, as text".into())),
            };
            match kind {
                Kind::Pin if code.is_empty() || code.len() > 16 || !code.bytes().all(|b| b.is_ascii_alphanumeric()) => Err("a PIN is 1 to 16 letters or digits".into()),
                Kind::Passkey if code.is_empty() || code.len() > 6 || !code.bytes().all(|b| b.is_ascii_digit()) => Err("a passkey is up to six digits".into()),
                _ => give(&[Kind::Pin, Kind::Passkey], Answer::Code(code)),
            }
        }
        ("bluetooth.confirm", []) => give(&[Kind::Confirm, Kind::Authorize], Answer::Yes),
        ("bluetooth.cancel", []) => {
            // A no from whoever sits there: BlueZ is told `Rejected`.
            match withdraw(|a| a.id == id, Answer::No) {
                // `Rejected` already fails the pairing; a code that is only shown
                // waits for no answer that could say no, so it is called off.
                Some(a) => {
                    if a.reply.is_none() {
                        call_off(a.device);
                    }
                    Ok(())
                }
                None => Err("that question is not being asked any more".into()),
            }
        }
        _ => Err(format!("'{what}' is not asked like that: bluetooth.answer(id, code), bluetooth.confirm(id), bluetooth.cancel(id)")),
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
            static ATTEMPTS: AtomicU64 = AtomicU64::new(1);
            let attempt = ATTEMPTS.fetch_add(1, Ordering::Relaxed);
            // Busy is said here, to whoever asked, not on the thread.
            let paired = device_path(&o, address).and_then(|p| o.get(&p)?.get(DEVICE).map(|d| flag(d, "Paired"))).unwrap_or(false);
            let pairing = if paired { None } else { Some(Pairing::begin(attempt)?) };
            let address = address.clone();
            // Pairing waits for the device: seconds, not for the logic to wait.
            std::thread::Builder::new()
                .name("bluetooth connect".into())
                .spawn(move || {
                    if let Err(e) = connect(address.clone(), attempt, pairing) {
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
        _ => Err(format!("'{what}' is not asked like that: bluetooth.power(true|false), bluetooth.scan(true|false), bluetooth.connect(address), bluetooth.disconnect(address), bluetooth.forget(address), bluetooth.answer(id, code), bluetooth.confirm(id), bluetooth.cancel(id)")),
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

    /// The question asked now, as the scene names it in its answer.
    fn id() -> SysValue {
        SysValue::Num(ASKED.lock().unwrap().as_ref().expect("something is asked").id as f64)
    }

    /// What BlueZ asks, on its own thread as the bus would, until it is put to the
    /// scene, which shows what pairing asks.
    fn asked(kind: Kind, code: &str) -> std::thread::JoinHandle<Answer> {
        asked_by(1, kind, code)
    }

    /// Nothing left asked by a test that failed: a code only shown would
    /// otherwise be called off, on the real bus, when the next one's scene goes.
    fn clean() {
        let left = ASKED.lock().unwrap().take();
        if let Some(r) = left.and_then(|a| a.reply) {
            r.give(Answer::Over);
        }
    }

    fn asked_by(attempt: u64, kind: Kind, code: &str) -> std::thread::JoinHandle<Answer> {
        clean();
        handles("Test", true);
        let code = code.to_owned();
        let waiting = std::thread::spawn(move || zbus::block_on(ask(attempt, kind, OwnedObjectPath::try_from(DEV).unwrap(), code)));
        while ASKED.lock().unwrap().is_none() {
            std::thread::yield_now();
        }
        waiting
    }

    #[test]
    fn a_question_put_after_the_last_scene_went_is_not_left_waiting() {
        let _one_at_a_time = ONE.lock().unwrap();
        // `ask` saw a scene; it went before the question was put, so `handles`
        // had nothing to withdraw.
        clean();
        handles("Test", true);
        handles("Test", false);
        let reply = Arc::new(Reply::default());
        put(1, Kind::Confirm, OwnedObjectPath::try_from(DEV).unwrap(), "333333".into(), Some(reply.clone()));
        assert!(ASKED.lock().unwrap().is_none());
        // Read, not waited for: left waiting, it fails rather than hangs.
        assert_eq!(reply.0.lock().unwrap().0, Some(Answer::Over));
    }

    #[test]
    fn a_number_to_compare_is_shown_and_the_yes_goes_back() {
        let _one_at_a_time = ONE.lock().unwrap();
        let waiting = asked(Kind::Confirm, "012345");
        let o = known();
        assert_eq!((field(&o, "pairing").as_str(), field(&o, "pairing_code").as_str()), ("confirm", "012345"));
        assert_eq!((field(&o, "pairing_address").as_str(), field(&o, "pairing_name").as_str()), ("AA:BB:CC:DD:EE:FF", "Keys"));
        // Not what is asked: it waits on.
        assert!(command("bluetooth.answer", &[id(), SysValue::Text("012345".into())]).is_err());
        // Nor without saying which question it answers.
        assert!(command("bluetooth.confirm", &[]).is_err());
        command("bluetooth.confirm", &[id()]).unwrap();
        assert_eq!(waiting.join().unwrap(), Answer::Yes);
        assert_eq!(field(&o, "pairing"), "none");
    }

    #[test]
    fn a_passkey_is_checked_before_it_is_given() {
        let _one_at_a_time = ONE.lock().unwrap();
        let waiting = asked(Kind::Passkey, "");
        let question = id();
        for wrong in ["", "abc", "1234567"] {
            assert!(command("bluetooth.answer", &[question.clone(), SysValue::Text(wrong.into())]).is_err(), "{wrong}");
        }
        assert!(command("bluetooth.confirm", std::slice::from_ref(&question)).is_err());
        command("bluetooth.answer", &[question.clone(), SysValue::Text(" 004321 ".into())]).unwrap();
        assert_eq!(waiting.join().unwrap(), Answer::Code("004321".into()));
        assert!(command("bluetooth.answer", &[question, SysValue::Text("1".into())]).is_err(), "nothing is asked any more");
    }

    #[test]
    fn an_answer_to_a_question_that_is_over_does_not_answer_the_next() {
        let _one_at_a_time = ONE.lock().unwrap();
        let first = asked(Kind::Confirm, "111111");
        let stale = id();
        let second = asked(Kind::Confirm, "222222");
        assert_eq!(first.join().unwrap(), Answer::Over);
        assert_ne!(format!("{:?}", id()), format!("{stale:?}"));
        assert!(command("bluetooth.confirm", std::slice::from_ref(&stale)).unwrap_err().contains("not being asked"));
        assert!(command("bluetooth.cancel", &[stale]).is_err());
        assert_eq!(field(&known(), "pairing_code"), "222222");
        command("bluetooth.confirm", &[id()]).unwrap();
        assert_eq!(second.join().unwrap(), Answer::Yes);
    }

    #[test]
    fn whoever_waits_is_told_no_when_bluez_gives_up_or_something_else_is_asked() {
        let _one_at_a_time = ONE.lock().unwrap();
        let waiting = asked(Kind::Pin, "");
        // BlueZ's `Cancel`: nobody answered in time.
        assert!(withdraw(|a| a.attempt == 1, Answer::Over).is_some());
        assert_eq!(waiting.join().unwrap(), Answer::Over);
        let waiting = asked(Kind::Authorize, "");
        put(1, Kind::Display, OwnedObjectPath::try_from(DEV).unwrap(), "123456".into(), None);
        assert_eq!(waiting.join().unwrap(), Answer::Over);
        assert_eq!(field(&known(), "pairing"), "display");
        // Another pairing ends: this one is still shown.
        assert!(withdraw(|a| a.attempt == 2, Answer::Over).is_none());
        assert!(withdraw(|a| a.attempt == 1, Answer::Over).is_some());
        assert_eq!(field(&known(), "pairing"), "none");
    }

    #[test]
    fn a_second_connect_while_one_pairs_is_refused_as_busy() {
        let _one_at_a_time = ONE.lock().unwrap();
        let first = Pairing::begin(7).unwrap();
        assert!(Pairing::begin(8).err().unwrap().contains("busy"));
        drop(first);
        let second = Pairing::begin(8).unwrap();
        // An attempt that is not the one pairing lets go of nothing.
        drop(Pairing(7));
        assert!(Pairing::begin(9).is_err());
        drop(second);
    }

    #[test]
    fn an_attempt_ends_only_its_own_question() {
        let _one_at_a_time = ONE.lock().unwrap();
        let waiting = asked_by(7, Kind::Confirm, "123456");
        assert!(withdraw(|a| a.attempt == 8, Answer::Over).is_none());
        assert_eq!(field(&known(), "pairing"), "confirm");
        assert!(withdraw(|a| a.attempt == 7, Answer::Over).is_some());
        assert_eq!(waiting.join().unwrap(), Answer::Over);
    }

    #[test]
    fn a_pairing_that_never_ends_is_called_off() {
        let _one_at_a_time = ONE.lock().unwrap();
        let waiting = asked_by(5, Kind::Confirm, "123456");
        let called_off = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = called_off.clone();
        let _stop = deadline(5, Duration::from_millis(50), move || flag.store(true, Ordering::SeqCst));
        // `Pair` never returned: BlueZ is told `Canceled`, and the pairing is called off.
        let over = waiting.join().unwrap();
        assert_eq!(over, Answer::Over);
        assert!(matches!(refused(over), AgentError::Canceled(_)));
        while !called_off.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
        assert_eq!(field(&known(), "pairing"), "none");
        // One that ends in time leaves the deadline nothing to do.
        let waiting = asked_by(6, Kind::Confirm, "123456");
        let called_off = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = called_off.clone();
        drop(deadline(6, Duration::from_millis(50), move || flag.store(true, Ordering::SeqCst)));
        std::thread::sleep(Duration::from_millis(150));
        assert!(!called_off.load(Ordering::SeqCst));
        assert_eq!(field(&known(), "pairing"), "confirm");
        command("bluetooth.confirm", &[id()]).unwrap();
        assert_eq!(waiting.join().unwrap(), Answer::Yes);
    }

    #[test]
    fn a_no_is_rejected_and_an_end_is_cancelled() {
        let _one_at_a_time = ONE.lock().unwrap();
        // `bluetooth.cancel` withdraws with `No` (not called here: it would reach BlueZ).
        let waiting = asked(Kind::Confirm, "123456");
        assert!(withdraw(|_| true, Answer::No).is_some());
        let no = waiting.join().unwrap();
        assert_eq!(no, Answer::No);
        assert!(matches!(refused(no), AgentError::Rejected(_)));
        assert!(matches!(refused(Answer::Over), AgentError::Canceled(_)));
    }

    #[test]
    fn a_pin_is_text_and_keeps_its_zeros() {
        let _one_at_a_time = ONE.lock().unwrap();
        let waiting = asked(Kind::Pin, "");
        let question = id();
        for wrong in ["", "12 34", "12-34", "pïn", "12345678901234567"] {
            assert!(command("bluetooth.answer", &[question.clone(), SysValue::Text(wrong.into())]).is_err(), "{wrong}");
        }
        assert!(command("bluetooth.answer", &[question.clone(), SysValue::Num(12.0)]).unwrap_err().contains("text"));
        command("bluetooth.answer", &[question, SysValue::Text("0012ab".into())]).unwrap();
        assert_eq!(waiting.join().unwrap(), Answer::Code("0012ab".into()));
        // A passkey may still be a number.
        let waiting = asked(Kind::Passkey, "");
        command("bluetooth.answer", &[id(), SysValue::Num(4321.0)]).unwrap();
        assert_eq!(waiting.join().unwrap(), Answer::Code("4321".into()));
    }

    #[test]
    fn the_device_is_still_named_when_it_goes_before_the_answer() {
        let _one_at_a_time = ONE.lock().unwrap();
        let waiting = asked(Kind::Confirm, "123456");
        assert_eq!(field(&known(), "pairing_name"), "Keys");
        let gone = Objects::new();
        assert_eq!((field(&gone, "pairing_address").as_str(), field(&gone, "pairing_name").as_str()), ("AA:BB:CC:DD:EE:FF", "Keys"));
        withdraw(|_| true, Answer::Over);
        waiting.join().unwrap();
    }

    #[test]
    fn with_no_adapter_nothing_is_asked() {
        let SysValue::Map(v) = absent() else { panic!() };
        let get = |k: &str| v.iter().find(|(n, _)| n == k).map(|(_, v)| format!("{v:?}"));
        assert_eq!(get("present"), Some(format!("{:?}", SysValue::Bool(false))));
        assert_eq!(get("pairing"), Some(format!("{:?}", SysValue::Text("none".into()))));
        assert_eq!(get("pairing_id"), Some(format!("{:?}", SysValue::Num(0.0))));
        for k in ["pairing_address", "pairing_name", "pairing_code"] {
            assert_eq!(get(k), Some(format!("{:?}", SysValue::Text(String::new()))), "{k}");
        }
    }

    #[test]
    fn only_a_scene_that_shows_it_is_asked_and_its_going_ends_the_question() {
        let _one_at_a_time = ONE.lock().unwrap();
        clean();
        handles("Test", false);
        // Watching is not showing: nothing is asked, and BlueZ is told `Canceled`.
        let _rx = watch();
        assert!(!asks());
        assert_eq!(zbus::block_on(ask(1, Kind::Confirm, OwnedObjectPath::try_from(DEV).unwrap(), "123456".into())), Answer::Over);
        assert!(ASKED.lock().unwrap().is_none());
        // Two scenes show it; the question stays while one of them is left.
        handles("Bar", true);
        let waiting = asked(Kind::Confirm, "123456");
        handles("Test", false);
        assert!(asks());
        assert_eq!(field(&known(), "pairing"), "confirm");
        handles("Bar", false);
        assert!(!asks());
        assert_eq!(waiting.join().unwrap(), Answer::Over);
        assert_eq!(field(&known(), "pairing"), "none");
    }

    #[test]
    fn only_bluez_asks_the_agent() {
        let agent = Agent { asks: true, attempt: 1, bluez: ":1.7".into() };
        assert!(agent.sent_by_bluez(Some(":1.7")).is_ok());
        assert!(matches!(agent.sent_by_bluez(Some(":1.42")), Err(AgentError::Rejected(_))));
        assert!(agent.sent_by_bluez(None).is_err());
    }

    /// These share the one thing a process is asked at a time: one at a time.
    static ONE: Mutex<()> = Mutex::new(());
}
