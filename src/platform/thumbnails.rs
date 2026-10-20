//! `thumbnails`: every window of the desktop, with a small picture of it —an
//! overview, a switcher, a dock's previews—, including windows the scene does
//! not host. Spoken in the standard protocols: ext-foreign-toplevel-list to
//! know the windows, ext-foreign-toplevel-image-capture-source to name one,
//! and ext-image-copy-capture to copy it. Any compositor that ships them
//! (pleamar-wm, Hyprland, sway, niri…) works the same; where they are missing,
//! the windows are listed with no picture.
//!
//! Nothing is copied until the logic asks for it (`thumbnails.want`, the ids
//! it shows, or "all"), and then only when the window changes: the compositor
//! holds each frame until there is something new, and here no more than a few
//! a second are taken. A frame with the same pixels as the last one —Hyprland
//! fills every frame waiting on a monitor when anything on it redraws— is
//! nothing new: no file is written and nobody is told. Each picture is made small (`SIDE` at most) and written
//! as a PNG under `$XDG_RUNTIME_DIR/pleamar/thumbnails`; `picture` is that
//! path with a version (`…/3.png?12`), which `image … = from` paints and
//! reloads as it changes.
//!
//! `thumbnails.live` asks the same of the windows an overview shows moving:
//! their frames skip the file. Each one is kept here, in memory, as it came,
//! and `picture` names it (`thumbnails:3?12`); `image … = from` takes it from
//! here and makes it the size it is drawn at. Up to `LIVE_EVERY` a second,
//! and still only when the pixels change. A window no longer asked for live
//! gives its frame back as a file, so a whole frame is never held for nobody.
//!
//! Where the compositor offers it (its session names a card and BGRA layouts
//! of one plane the render reads), a live window is copied on the card instead:
//! the render makes the buffer (`wants`, answered with `offer`), the compositor
//! fills it, and the render reads it once `ready` has come, into the size it is
//! drawn at, with nothing passing through memory. Each window falls back to the
//! memory copy on its own, if the card cannot be used for it.
//!
//! A window the compositor stops copying keeps its last picture, and `stale`
//! says it. Only the compositor can say so (the session `stopped`): a frame
//! that does not come is the same, from here, whether the window did not
//! change or the compositor will not copy it —Hyprland copies only windows that
//! overlap a monitor, so a column scrolled away waits for good, not stale—.

use super::SysValue;
use std::collections::{HashMap, HashSet};
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use wayland_client::protocol::{wl_buffer, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1::{self as toplevel, ExtForeignToplevelHandleV1},
    ext_foreign_toplevel_list_v1::{self as toplevel_list, ExtForeignToplevelListV1},
};
use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_foreign_toplevel_image_capture_source_manager_v1::ExtForeignToplevelImageCaptureSourceManagerV1,
    ext_image_capture_source_v1::ExtImageCaptureSourceV1,
};
use smithay_client_toolkit::reexports::protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1},
    zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1::{self as copy_frame, ExtImageCopyCaptureFrameV1},
    ext_image_copy_capture_manager_v1::{self as copy_manager, ExtImageCopyCaptureManagerV1},
    ext_image_copy_capture_session_v1::{self as copy_session, ExtImageCopyCaptureSessionV1},
};

/// The longest side of a thumbnail, in pixels.
const SIDE: u32 = 400;
/// A window's pictures, at most this often.
const EVERY: Duration = Duration::from_millis(300);
/// A live window's, at most this often: Hyprland fills every frame waiting on
/// a monitor each time anything on it redraws, so without a pace a 144 Hz
/// monitor would have every live window read and compared 144 times a second.
const LIVE_EVERY: Duration = Duration::from_millis(33);
/// A window copied on the card is not copied again until the render has read
/// the last copy, or this long has passed (a frame nobody draws is not read).
const HOLD: Duration = Duration::from_millis(100);

/// A live window's last frame, as the compositor left it.
pub struct Frame {
    /// Bumped each time it changes: whoever drew the last one can tell this is the same.
    pub version: u64,
    pub width: u32,
    pub height: u32,
    /// Blue, green, red and alpha, premultiplied, `width * 4` bytes a row.
    pub pixels: Vec<u8>,
    /// XRGB: the alpha byte is not to be read.
    pub opaque: bool,
    /// Copied on the card: `pixels` is empty, and the frame is in this buffer.
    pub card: Option<Arc<OnCard>>,
}

/// A buffer on the card the compositor copies a live window into, made by the
/// render: what any render needs to read it, and how far it has.
pub struct OnCard {
    /// The render that made it keeps its texture under this.
    pub ticket: u64,
    pub fd: OwnedFd,
    pub fourcc: u32,
    pub modifier: u64,
    /// Offset and stride, in bytes, of its one plane.
    pub planes: Vec<(u32, u32)>,
    pub size: (u32, u32),
    /// The last version read: the compositor is not asked to fill it again before.
    read: AtomicU64,
    /// Reads the card is still doing, and `CLOSED` while the compositor
    /// fills it: neither starts while the other is under way, so `HOLD`
    /// (for a frame nobody draws) never cuts a read short.
    readers: AtomicU64,
    /// The last version a render could not read (the buffer was being filled
    /// again): drawn by nobody, so the next copy is new even if it is the same.
    skipped: AtomicU64,
}

const CLOSED: u64 = 1 << 63;

impl OnCard {
    /// The render has finished reading that version: the next copy may come.
    pub fn read(&self, version: u64) {
        self.read.fetch_max(version, Ordering::AcqRel);
        wake();
    }

    /// The render is about to read the buffer: false if the compositor is
    /// filling it, and it is not to be read until the next copy.
    pub fn claim(&self) -> bool {
        let mut r = self.readers.load(Ordering::Acquire);
        while r & CLOSED == 0 {
            match self.readers.compare_exchange_weak(r, r + 1, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return true,
                Err(now) => r = now,
            }
        }
        false
    }

    /// A read `claim`ed is over, on the card too.
    pub fn release(&self) {
        if self.readers.fetch_sub(1, Ordering::AcqRel) == 1 {
            wake();
        }
    }

    /// The render could not read that version (`claim`).
    pub fn skipped(&self, version: u64) {
        self.skipped.fetch_max(version, Ordering::AcqRel);
    }

    /// Whether the compositor may fill it now: no read under way, and none
    /// starts until `open`.
    fn close(&self) -> bool {
        match self.readers.compare_exchange(0, CLOSED, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => true,
            Err(r) => r == CLOSED,
        }
    }

    /// Filled: it may be read. Only ever from `CLOSED`, so reads under way keep their count.
    fn open(&self) {
        let _ = self.readers.compare_exchange(CLOSED, 0, Ordering::AcqRel, Ordering::Acquire);
    }
}

/// A buffer on the card a live window could be copied into, asked of the render.
pub struct Want {
    pub ticket: u64,
    pub size: (u32, u32),
    /// The compositor's card, as a `dev_t`.
    pub device: u64,
    /// The formats it copies into on the card, each with its layouts.
    pub formats: Vec<(u32, Vec<u64>)>,
}

/// The render's answer to a `Want`: a buffer it made, and the texture it keeps.
pub struct Offer {
    pub fd: OwnedFd,
    pub fourcc: u32,
    pub modifier: u64,
    pub planes: Vec<(u32, u32)>,
}

static WANTS: Mutex<Vec<Want>> = Mutex::new(Vec::new());
static OFFERS: Mutex<Option<HashMap<u64, Result<Offer, String>>>> = Mutex::new(None);
/// Every ticket whose buffer is still in use (asked, copied into, or drawn):
/// the render keeps the texture of these and lets go of the rest.
static CARDS: Mutex<Option<HashSet<u64>>> = Mutex::new(None);
static TICKETS: AtomicU64 = AtomicU64::new(0);
/// What wakes the loop when the render has answered or read.
static WAKE: OnceLock<OwnedFd> = OnceLock::new();

fn wake() {
    if let Some(fd) = WAKE.get() {
        use std::os::fd::AsRawFd;
        let one = 1u64;
        // SAFETY: eight bytes of ours to an eventfd of ours.
        unsafe { libc::write(fd.as_raw_fd(), &one as *const u64 as *const libc::c_void, 8) };
    }
}

/// The buffers on the card asked for since the last call.
pub fn wants() -> Vec<Want> {
    std::mem::take(&mut *WANTS.lock().unwrap())
}

/// The render's answer to `wants`: the buffer, or why not.
pub fn offer(ticket: u64, answer: Result<Offer, String>) {
    OFFERS.lock().unwrap().get_or_insert_with(HashMap::new).insert(ticket, answer);
    wake();
}

/// A frame on the card to tell apart from the last one: its buffer, which of
/// its buffer's copies it is, and whether its alpha means nothing (XRGB).
pub struct Check {
    pub on: Arc<OnCard>,
    pub n: u64,
    pub opaque: bool,
}

/// Frames on the card to tell apart from the last one; the render's sums for
/// them, by ticket, with the copy each is of; and how to wake a render at rest
/// to make them.
static CHECKS: Mutex<Vec<Check>> = Mutex::new(Vec::new());
type Sums = HashMap<u64, (u64, Option<u64>)>;
static SUMS: Mutex<Option<Sums>> = Mutex::new(None);
static RENDERS: Mutex<Vec<Box<dyn Fn() + Send>>> = Mutex::new(Vec::new());

/// How a render is woken to make the sums it is asked for (`checks`).
pub fn on_check(wake: Box<dyn Fn() + Send>) {
    RENDERS.lock().unwrap().push(wake);
}

/// The frames to sum of the buffers a render has (`has`); the others stay for
/// the render that does, and those of buffers no longer in use go.
pub fn checks(has: impl Fn(u64) -> bool) -> Vec<Check> {
    // Asked every round: with nothing to sum, nothing else is looked at.
    if CHECKS.lock().unwrap().is_empty() {
        return Vec::new();
    }
    let used = cards();
    let mut c = CHECKS.lock().unwrap();
    c.retain(|t| used.contains(&t.on.ticket));
    let (mine, rest) = c.drain(..).partition(|t| has(t.on.ticket));
    *c = rest;
    mine
}

/// The render's sum of a frame (`Check::n` of its buffer), or none if it could not say.
pub fn checked(ticket: u64, n: u64, sum: Option<u64>) {
    SUMS.lock().unwrap().get_or_insert_with(HashMap::new).insert(ticket, (n, sum));
    wake();
}

/// The tickets whose buffers are still in use.
pub fn cards() -> HashSet<u64> {
    CARDS.lock().unwrap().clone().unwrap_or_default()
}

fn card_used(ticket: u64, used: bool) {
    let mut c = CARDS.lock().unwrap();
    let c = c.get_or_insert_with(HashSet::new);
    if used {
        c.insert(ticket);
    } else {
        c.remove(&ticket);
    }
}

/// Said once each: how live windows are being copied, and why not on the card.
fn say(what: String) {
    static SAID: Mutex<Option<HashSet<String>>> = Mutex::new(None);
    if SAID.lock().unwrap().get_or_insert_with(HashSet::new).insert(what.clone()) {
        println!("thumbnails · {what}");
    }
}

/// Every live window's last frame, by the number in its `picture`.
static FRAMES: Mutex<Option<HashMap<u64, Arc<Frame>>>> = Mutex::new(None);

/// The frame a `picture` names (`thumbnails:3`, without its version), if it is one.
pub fn frame(name: &str) -> Option<Arc<Frame>> {
    let seq: u64 = name.strip_prefix("thumbnails:")?.parse().ok()?;
    FRAMES.lock().unwrap().as_ref()?.get(&seq).cloned()
}

fn forget_frame(seq: u64) {
    let gone = FRAMES.lock().unwrap().as_mut().and_then(|f| f.remove(&seq));
    if let Some(card) = gone.and_then(|f| f.card.clone()) {
        card_used(card.ticket, false);
    }
}

fn card_frame(seq: u64) -> bool {
    FRAMES.lock().unwrap().as_ref().and_then(|f| f.get(&seq)).is_some_and(|f| f.card.is_some())
}

/// Which windows to copy.
#[derive(Default, Clone, PartialEq)]
enum Wanted {
    #[default]
    None,
    All,
    These(HashSet<String>),
}

impl Wanted {
    fn has(&self, w: &Window) -> bool {
        match self {
            Wanted::None => false,
            Wanted::All => true,
            Wanted::These(ids) => ids.contains(&w.id),
        }
    }
}

/// A memory buffer the compositor copies a window into (or, for `capture`, a
/// monitor).
pub(super) struct Shm {
    pub(super) buffer: wl_buffer::WlBuffer,
    pool: wl_shm_pool::WlShmPool,
    map: *mut u8,
    len: usize,
    pub(super) size: (u32, u32),
    pub(super) format: wl_shm::Format,
}

// SAFETY: the map is only touched with the state's lock held.
unsafe impl Send for Shm {}

impl Drop for Shm {
    fn drop(&mut self) {
        self.buffer.destroy();
        self.pool.destroy();
        // SAFETY: mapped by `Shm::new` with this length, and not used after.
        unsafe { libc::munmap(self.map as *mut libc::c_void, self.len) };
    }
}

impl Shm {
    pub(super) fn new<D>(shm: &wl_shm::WlShm, size: (u32, u32), format: wl_shm::Format, qh: &QueueHandle<D>) -> Option<Shm>
    where
        D: Dispatch<wl_shm_pool::WlShmPool, ()> + Dispatch<wl_buffer::WlBuffer, ()> + 'static,
    {
        let (w, h) = size;
        let stride = w as usize * 4;
        let len = stride * h as usize;
        if len == 0 {
            return None;
        }
        // SAFETY: a name of our own; the fd is owned from here on.
        let fd = unsafe { libc::memfd_create(c"pleamar-thumbnail".as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return None;
        }
        // SAFETY: just made, and owned by nothing else.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        use std::os::fd::AsRawFd;
        // SAFETY: our own fd, made as long as the buffer.
        if unsafe { libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) } != 0 {
            return None;
        }
        // SAFETY: the whole of the fd just sized, shared with the compositor.
        let map = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd.as_raw_fd(), 0) };
        if map == libc::MAP_FAILED {
            return None;
        }
        let pool = shm.create_pool(fd.as_fd(), len as i32, qh, ());
        let buffer = pool.create_buffer(0, w as i32, h as i32, stride as i32, format, qh, ());
        Some(Shm { buffer, pool, map: map as *mut u8, len, size, format })
    }

    /// What the compositor left in it, `size.0 * 4` bytes a row.
    pub(super) fn bytes(&self) -> &[u8] {
        // SAFETY: mapped by `new` with this length, for as long as `self`.
        unsafe { std::slice::from_raw_parts(self.map, self.len) }
    }
}

/// A buffer on the card a live window is copied into, on this connection.
struct Card {
    buffer: wl_buffer::WlBuffer,
    on: Arc<OnCard>,
    /// The last version made of it, and when (or when its sum was asked).
    shown: u64,
    at: Instant,
    /// A copy waiting for the render to sum it, which copy that is (a late
    /// sum of an earlier one is not its answer), and the last copy's sum.
    checking: bool,
    n: u64,
    sum: Option<u64>,
}

impl Card {
    /// Whether the render may still be summing or reading the last copy.
    fn held(&self) -> bool {
        (self.checking || self.shown > self.on.read.load(Ordering::Acquire)) && self.at.elapsed() < HOLD
    }
}

impl Drop for Card {
    fn drop(&mut self) {
        self.buffer.destroy();
        // Never filled again: its last frame may be read for as long as it is drawn.
        self.on.open();
        // Still drawn from, if it is the window's last frame: let go of with it.
        if !FRAMES.lock().unwrap().as_ref().is_some_and(|f| f.values().any(|f| f.card.as_ref().is_some_and(|c| c.ticket == self.on.ticket))) {
            card_used(self.on.ticket, false);
        }
    }
}

/// Copying one window: its session, what the compositor said it takes, the
/// buffer, and the frame on its way.
struct Capture {
    session: ExtImageCopyCaptureSessionV1,
    _source: ExtImageCaptureSourceV1,
    size: Option<(u32, u32)>,
    formats: Vec<wl_shm::Format>,
    shm: Option<Shm>,
    /// The compositor's card and what it copies into there; `constrained`
    /// once it has said all of it (`done`).
    device: Option<u64>,
    dmabuf: Vec<(u32, Vec<u64>)>,
    constrained: bool,
    /// Copied on the card: the buffer, the one asked for (ticket and size),
    /// and whether the card failed this window at this size.
    card: Option<Card>,
    asked: Option<(u64, (u32, u32))>,
    /// Handed to the compositor, and not yet made into a buffer (or refused).
    making: Option<Arc<OnCard>>,
    no_card: bool,
    /// The frame on its way is in `card`, not in `shm`.
    on_card: bool,
    frame: Option<ExtImageCopyCaptureFrameV1>,
    next: Instant,
}

impl Drop for Capture {
    fn drop(&mut self) {
        if let Some(f) = self.frame.take() {
            f.destroy();
        }
        if let Some((t, _)) = self.asked.take() {
            card_used(t, false);
        }
        if let Some(on) = self.making.take() {
            card_used(on.ticket, false);
        }
        self.card = None;
        self.session.destroy();
        self._source.destroy();
    }
}

struct Window {
    handle: ExtForeignToplevelHandleV1,
    /// Its turn in the list, and the name of its file.
    seq: u64,
    id: String,
    title: String,
    app: String,
    picture: String,
    version: u64,
    /// The pixels of the last picture, to tell a new one from the same again.
    pixels: u64,
    /// Whether `picture` names a frame in memory (`thumbnails.live`) or a file.
    live: bool,
    stale: bool,
    capture: Option<Capture>,
}

#[derive(Default)]
struct State {
    list: Option<ExtForeignToplevelListV1>,
    sources: Option<ExtForeignToplevelImageCaptureSourceManagerV1>,
    copies: Option<ExtImageCopyCaptureManagerV1>,
    shm: Option<wl_shm::WlShm>,
    dmabuf: Option<ZwpLinuxDmabufV1>,
    windows: HashMap<u32, Window>,
    seq: u64,
    wanted: Wanted,
    /// Which windows to copy live, to the renderer and not to a file.
    live: Wanted,
    /// Everyone listening (the scene's `service thumbnails`, the logic's `sys.watch`…).
    dispatch: Vec<Box<dyn Fn(SysValue) + Send>>,
    last: Option<Vec<u8>>,
    dir: std::path::PathBuf,
}

fn key(p: &impl Proxy) -> u32 {
    p.id().protocol_id()
}

impl State {
    fn report(&mut self) {
        if self.dispatch.is_empty() {
            return;
        }
        let mut windows: Vec<&Window> = self.windows.values().filter(|w| !w.id.is_empty()).collect();
        windows.sort_by_key(|w| w.seq);
        let list = SysValue::List(
            windows
                .iter()
                .map(|w| {
                    SysValue::Map(vec![
                        ("id".into(), SysValue::Text(w.id.clone())),
                        ("title".into(), SysValue::Text(w.title.clone())),
                        ("app".into(), SysValue::Text(w.app.clone())),
                        ("picture".into(), SysValue::Text(w.picture.clone())),
                        ("stale".into(), SysValue::Bool(w.stale)),
                    ])
                })
                .collect(),
        );
        // Told only what changed: the same list twice is nothing new.
        let fingerprint = format!("{:?}", windows.iter().map(|w| (&w.id, &w.title, &w.app, &w.picture, w.stale)).collect::<Vec<_>>()).into_bytes();
        if self.last.as_ref() == Some(&fingerprint) {
            return;
        }
        self.last = Some(fingerprint);
        let value = SysValue::Map(vec![("list".into(), list), ("capturing".into(), SysValue::Bool(self.copies.is_some() && self.sources.is_some()))]);
        for d in &self.dispatch {
            d(value.clone());
        }
    }

    fn wants(&self, w: &Window) -> bool {
        self.wanted.has(w) || self.lives(w)
    }

    fn lives(&self, w: &Window) -> bool {
        self.live.has(w)
    }

    /// A window no longer live keeps its picture, but as a file like any
    /// other: the whole frame is not held for nobody, and a window that does
    /// not change again would never come back through `taken`.
    fn settle(&mut self, k: u32) {
        let Some(w) = self.windows.get_mut(&k) else { return };
        if card_frame(w.seq) {
            // Nothing in memory to write: drawn from the card until the next
            // copy, asked for now, comes as a file (or the window stops being
            // wanted at all, and its picture goes).
            w.live = false;
            w.pixels = 0;
            if let Some(c) = &mut w.capture {
                c.next = Instant::now();
            }
            return;
        }
        let frame = FRAMES.lock().unwrap().as_mut().and_then(|f| f.remove(&w.seq));
        w.live = false;
        let Some(f) = frame else {
            w.picture.clear();
            self.report();
            return;
        };
        match write_png(&self.dir, w.seq, &f.pixels, (f.width, f.height), f.opaque) {
            Some(path) => {
                // Live, it was compared byte for byte and never hashed: hashed
                // now, so the same frame coming again is not written twice.
                w.pixels = fingerprint(&f.pixels, (f.width, f.height));
                w.version += 1;
                w.picture = format!("{}?{}", path.display(), w.version);
            }
            None => w.picture.clear(),
        }
        self.report();
    }

    /// How long the loop may sleep: until the soonest copy is due, and never
    /// more than a tenth of a second (what the logic wants is looked at on
    /// the way round).
    fn wait(&self) -> i32 {
        let now = Instant::now();
        self.windows
            .values()
            .filter_map(|w| w.capture.as_ref())
            .filter(|c| c.frame.is_none() && c.size.is_some())
            .map(|c| {
                // Held for the render: its answer wakes the loop, and `HOLD` ends it.
                let held = c.card.as_ref().filter(|d| d.held()).map(|d| (d.at + HOLD).saturating_duration_since(now));
                c.next.saturating_duration_since(now).max(held.unwrap_or_default()).as_millis() as i32
            })
            .fold(100, i32::min)
    }

    /// Each round: copies started and stopped as wanted, and frames asked for
    /// when it is their time.
    fn tick(&mut self, qh: &QueueHandle<State>) {
        let (Some(sources), Some(copies), Some(shm)) = (self.sources.clone(), self.copies.clone(), self.shm.clone()) else { return };
        let mut offers = OFFERS.lock().unwrap().take().unwrap_or_default();
        let mut sums = SUMS.lock().unwrap().take().unwrap_or_default();
        let keys: Vec<u32> = self.windows.keys().copied().collect();
        for k in keys {
            if self.windows[&k].live && !self.lives(&self.windows[&k]) {
                self.settle(k);
            }
            self.summed(k, &mut sums);
            let want = { let w = &self.windows[&k]; !w.id.is_empty() && self.wants(w) };
            let live = self.lives(&self.windows[&k]);
            let w = self.windows.get_mut(&k).unwrap();
            if !want {
                w.capture = None;
                // Its last frame was on the card, and nobody keeps it any more.
                if card_frame(w.seq) {
                    forget_frame(w.seq);
                    w.picture.clear();
                    self.last = None;
                    self.report();
                }
                continue;
            }
            let handle = w.handle.clone();
            let c = w.capture.get_or_insert_with(|| {
                let source = sources.create_source(&handle, qh, ());
                let session = copies.create_session(&source, copy_manager::Options::empty(), qh, k);
                Capture { session, _source: source, size: None, formats: Vec::new(), shm: None, device: None, dmabuf: Vec::new(), constrained: false, card: None, asked: None, making: None, no_card: false, on_card: false, frame: None, next: Instant::now() }
            });
            // The render's answer: the buffer on the card on this connection,
            // or the memory copy until the size changes.
            if let Some((ticket, asked)) = c.asked
                && let Some(answer) = offers.remove(&ticket)
            {
                c.asked = None;
                match (answer, &self.dmabuf) {
                    (Ok(o), Some(d)) if c.size == Some(asked) => {
                        // Asked, not made at once: a buffer the compositor
                        // cannot import is then `failed`, not the end of the
                        // connection, and the window goes through memory.
                        let params = d.create_params(qh, k);
                        for (n, (offset, stride)) in o.planes.iter().enumerate() {
                            params.add(o.fd.as_fd(), n as u32, *offset, *stride, (o.modifier >> 32) as u32, o.modifier as u32);
                        }
                        params.create(asked.0 as i32, asked.1 as i32, o.fourcc, zwp_linux_buffer_params_v1::Flags::empty());
                        c.making = Some(Arc::new(OnCard { ticket, fd: o.fd, fourcc: o.fourcc, modifier: o.modifier, planes: o.planes, size: asked, read: AtomicU64::new(0), readers: AtomicU64::new(0), skipped: AtomicU64::new(0) }));
                    }
                    (Ok(_), _) => card_used(ticket, false),
                    (Err(e), _) => {
                        card_used(ticket, false);
                        c.no_card = true;
                        say(format!("a live window copied through memory: {e}"));
                    }
                }
            }
            // One on its way: the compositor holds it until the window changes.
            if c.frame.is_some() {
                continue;
            }
            // Made again only for another size; let go of when no longer live.
            if c.card.as_ref().is_some_and(|d| !live || c.size != Some(d.on.size)) {
                c.card = None;
            }
            let Some(size) = c.size else { continue };
            // Live, and the compositor copies to a card in BGRA: the render is
            // asked for a buffer there, and meanwhile it goes through memory.
            if live && c.card.is_none() && c.asked.is_none() && c.making.is_none() && !c.no_card && c.constrained && self.dmabuf.is_some()
                && let Some(device) = c.device
            {
                let formats: Vec<(u32, Vec<u64>)> = c.dmabuf.iter().filter(|(f, _)| *f == crate::dmabuf::ARGB || *f == crate::dmabuf::XRGB).cloned().collect();
                if formats.is_empty() {
                    c.no_card = true;
                    say("a live window copied through memory: the compositor copies to the card in no format of BGRA".into());
                } else {
                    let ticket = TICKETS.fetch_add(1, Ordering::Relaxed) + 1;
                    card_used(ticket, true);
                    WANTS.lock().unwrap().push(Want { ticket, size, device, formats });
                    c.asked = Some((ticket, size));
                }
            }
            if Instant::now() < c.next {
                continue;
            }
            if let Some(d) = &c.card {
                // Not filled again while the render reads the last copy.
                if d.held() || !d.on.close() {
                    continue;
                }
                let frame = c.session.create_frame(qh, k);
                frame.attach_buffer(&d.buffer);
                frame.damage_buffer(0, 0, size.0 as i32, size.1 as i32);
                frame.capture();
                c.frame = Some(frame);
                c.on_card = true;
                continue;
            }
            // Its buffer, again if the size changed; ARGB if it is offered (a
            // window may be see-through), XRGB if not.
            if c.shm.as_ref().is_none_or(|s| s.size != size) {
                let format = if c.formats.contains(&wl_shm::Format::Argb8888) { wl_shm::Format::Argb8888 } else { wl_shm::Format::Xrgb8888 };
                c.shm = Shm::new(&shm, size, format, qh);
            }
            let Some(s) = &c.shm else { continue };
            let frame = c.session.create_frame(qh, k);
            frame.attach_buffer(&s.buffer);
            frame.damage_buffer(0, 0, size.0 as i32, size.1 as i32);
            frame.capture();
            c.frame = Some(frame);
            c.on_card = false;
        }
        // Answers nobody waits for any more: their buffers go.
        for ticket in offers.into_keys() {
            card_used(ticket, false);
        }
    }

    /// The render's sum of a window's copy on the card, if it has come (or
    /// waiting for it is over): a new picture if the pixels changed.
    fn summed(&mut self, k: u32, sums: &mut Sums) {
        let Some(d) = self.windows.get_mut(&k).and_then(|w| w.capture.as_mut()).and_then(|c| c.card.as_mut()) else { return };
        if !d.checking {
            return;
        }
        let sum = match sums.remove(&d.on.ticket) {
            Some((n, sum)) if n == d.n => sum,
            _ if d.at.elapsed() < HOLD => return,
            // Not summed in time: new, as it would be without the sum.
            _ => None,
        };
        d.checking = false;
        // The last version skipped by a render is not on screen: drawn again.
        let same = sum.is_some() && sum == d.sum && d.on.skipped.load(Ordering::Acquire) < d.shown;
        d.sum = sum;
        let w = &mut self.windows.get_mut(&k).unwrap();
        if same && w.live {
            // The same picture again: nothing to draw, and only `stale` to undo.
            if w.stale {
                w.stale = false;
                self.report();
            }
            return;
        }
        if self.lives(&self.windows[&k]) {
            self.shown(k);
        }
    }

    /// A window's copy on the card is its new picture: told.
    fn shown(&mut self, k: u32) {
        let Some(w) = self.windows.get_mut(&k) else { return };
        let Some(d) = w.capture.as_mut().and_then(|c| c.card.as_mut()) else { return };
        w.version += 1;
        d.shown = w.version;
        d.at = Instant::now();
        let frame = Frame { version: w.version, width: d.on.size.0, height: d.on.size.1, pixels: Vec::new(), opaque: d.on.fourcc == crate::dmabuf::XRGB, card: Some(d.on.clone()) };
        let old = FRAMES.lock().unwrap().get_or_insert_with(HashMap::new).insert(w.seq, Arc::new(frame));
        // The last frame of a card made for another size: drawn from by nobody now.
        if let Some(card) = old.and_then(|f| f.card.clone())
            && card.ticket != d.on.ticket
        {
            card_used(card.ticket, false);
        }
        if !w.live {
            let _ = std::fs::remove_file(self.dir.join(format!("{}.png", w.seq)));
        }
        w.live = true;
        w.pixels = 0;
        w.picture = format!("thumbnails:{}?{}", w.seq, w.version);
        w.stale = false;
        self.report();
    }

    /// A frame came: kept for the renderer if the window is live; if not,
    /// made small, written. And told.
    fn taken(&mut self, k: u32) {
        let live = match self.windows.get(&k) {
            Some(w) => self.lives(w),
            None => return,
        };
        let Some(w) = self.windows.get_mut(&k) else { return };
        let Some(c) = &mut w.capture else { return };
        if let Some(f) = c.frame.take() {
            f.destroy();
        }
        c.next = Instant::now() + if live { LIVE_EVERY } else { EVERY };
        if std::mem::take(&mut c.on_card) {
            if let Some(d) = &c.card {
                d.on.open();
            }
            // On the card: summed there before it is anything new. Hyprland
            // fills the frame each time the monitor redraws, the overview
            // drawing the last one included, and damages all of it, so without
            // the sum a still window would be drawn again for ever.
            let Some(d) = c.card.as_mut().filter(|_| live) else { return };
            let renders = RENDERS.lock().unwrap();
            if renders.is_empty() {
                drop(renders);
                self.shown(k);
                return;
            }
            d.checking = true;
            d.at = Instant::now();
            d.n += 1;
            let mut checks = CHECKS.lock().unwrap();
            checks.retain(|c| c.on.ticket != d.on.ticket);
            checks.push(Check { on: d.on.clone(), n: d.n, opaque: d.on.fourcc == crate::dmabuf::XRGB });
            drop(checks);
            for wake in renders.iter() {
                wake();
            }
            return;
        }
        let Some(s) = &c.shm else { return };
        let (pw, ph) = s.size;
        // SAFETY: our own mapping, `len` bytes, filled by the compositor before `ready`.
        let px = unsafe { std::slice::from_raw_parts(s.map, s.len) };
        // The same pixels, kept the same way, are nothing new. Kept the other
        // way —the window went live, or stopped being live—, they are. A live
        // window's are compared with the frame kept for it, byte for byte: a
        // hash reads the whole frame at a fraction of the speed of a compare,
        // and a live window is looked at up to 30 times a second.
        let (same, pixels) = if live {
            let kept = w.live.then(|| FRAMES.lock().unwrap().as_ref().and_then(|f| f.get(&w.seq).cloned())).flatten();
            (kept.is_some_and(|f| (f.width, f.height) == s.size && f.pixels[..] == px[..]), 0)
        } else {
            let pixels = fingerprint(px, s.size);
            (pixels == w.pixels && w.version > 0 && !w.live, pixels)
        };
        if same {
            // The same picture again: nothing to write, and only `stale` to undo.
            if w.stale {
                w.stale = false;
                self.report();
            }
            return;
        }
        let opaque = s.format == wl_shm::Format::Xrgb8888;
        if live {
            // One copy, out of the buffer the compositor fills again next time;
            // made small where it is drawn, to the size it is drawn at.
            w.version += 1;
            let frame = Frame { version: w.version, width: pw, height: ph, pixels: px.to_vec(), opaque, card: None };
            let old = FRAMES.lock().unwrap().get_or_insert_with(HashMap::new).insert(w.seq, Arc::new(frame));
            // A frame left on a card no longer copied into: drawn from by nobody now.
            if let Some(card) = old.and_then(|f| f.card.clone())
                && c.card.as_ref().is_none_or(|d| d.on.ticket != card.ticket)
            {
                card_used(card.ticket, false);
            }
            if !w.live {
                let _ = std::fs::remove_file(self.dir.join(format!("{}.png", w.seq)));
            }
            w.live = true;
            w.pixels = pixels;
            w.picture = format!("thumbnails:{}?{}", w.seq, w.version);
            w.stale = false;
            self.report();
            return;
        }
        let Some(path) = write_png(&self.dir, w.seq, px, s.size, opaque) else { return };
        w.version += 1;
        // A live frame, or one left on the card: the file replaces it.
        forget_frame(w.seq);
        w.live = false;
        w.pixels = pixels;
        w.picture = format!("{}?{}", path.display(), w.version);
        w.stale = false;
        self.report();
    }
}

/// What tells two pictures apart, quickly: a window's frame is megabytes, and
/// all of them come again whenever the monitor redraws.
fn fingerprint(px: &[u8], (w, h): (u32, u32)) -> u64 {
    let mut f = ((w as u64) << 32 | h as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    let mut words = px.chunks_exact(8);
    for c in &mut words {
        f = (f.rotate_left(5) ^ u64::from_le_bytes(c.try_into().unwrap())).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }
    for &b in words.remainder() {
        f = (f.rotate_left(5) ^ b as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }
    f
}

/// A frame, made small (`SIDE` at most) and written as `<seq>.png` in `dir`:
/// BGRA premultiplied (ARGB8888 in memory) to RGBA as a PNG wants it.
fn write_png(dir: &std::path::Path, seq: u64, px: &[u8], (pw, ph): (u32, u32), opaque: bool) -> Option<std::path::PathBuf> {
    let mut rgba = Vec::with_capacity(px.len());
    for p in px.chunks_exact(4) {
        let a = if opaque { 255 } else { p[3] };
        let un = |c: u8| if a == 0 || a == 255 { c } else { ((c as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8 };
        rgba.extend_from_slice(&[un(p[2]), un(p[1]), un(p[0]), a]);
    }
    let full = image::RgbaImage::from_raw(pw, ph, rgba)?;
    let k_side = (SIDE as f32 / pw.max(ph) as f32).min(1.0);
    let (tw, th) = (((pw as f32 * k_side).round() as u32).max(1), ((ph as f32 * k_side).round() as u32).max(1));
    let small = if (tw, th) == (pw, ph) { full } else { image::imageops::thumbnail(&full, tw, th) };
    let path = dir.join(format!("{seq}.png"));
    let tmp = dir.join(format!("{seq}.png.new"));
    if small.save_with_format(&tmp, image::ImageFormat::Png).is_err() || std::fs::rename(&tmp, &path).is_err() {
        return None;
    }
    Some(path)
}

static CONTROL: OnceLock<(Connection, Arc<Mutex<State>>)> = OnceLock::new();

/// Starts listing the windows (once), and tells `dispatch` the list each time it changes.
pub fn service(dispatch: Box<dyn Fn(SysValue) + Send>) -> bool {
    let Some(state) = start() else { return false };
    let mut e = state.lock().unwrap();
    e.dispatch.push(dispatch);
    // The newcomer is told now; the others hear the same list again, which is nothing new to them.
    e.last = None;
    e.report();
    true
}

/// `thumbnails.want`: which windows to copy, by their `id`s (a list), `"all"`,
/// or none. `thumbnails.live`, the same, for the ones to copy live.
pub fn command(name: &str, args: &[SysValue]) -> Result<(), String> {
    if name != "thumbnails.want" && name != "thumbnails.live" {
        return Err(format!("the thumbnails cannot do '{name}': only thumbnails.want and thumbnails.live"));
    }
    let wanted = match args {
        [] | [SysValue::Null] => Wanted::None,
        [SysValue::Text(t)] if t == "all" => Wanted::All,
        [SysValue::Text(t)] => Wanted::These(HashSet::from([t.clone()])),
        [SysValue::List(ids)] => Wanted::These(ids.iter().filter_map(|v| if let SysValue::Text(t) = v { Some(t.clone()) } else { None }).collect()),
        _ => return Err(format!("{name} takes a list of window ids, \"all\", or nothing")),
    };
    let state = start().ok_or("this compositor does not list its windows (ext-foreign-toplevel-list)")?;
    let mut e = state.lock().unwrap();
    if name == "thumbnails.live" {
        e.live = wanted;
    } else {
        e.wanted = wanted;
    }
    Ok(())
}

fn start() -> Option<Arc<Mutex<State>>> {
    if let Some((_, s)) = CONTROL.get() {
        return Some(s.clone());
    }
    let connection = Connection::connect_to_env().ok()?;
    let mut queue = connection.new_event_queue::<State>();
    let qh = queue.handle();
    connection.display().get_registry(&qh, ());
    // A folder per process; those of processes already gone, out (a shell
    // started again leaves its last pictures behind).
    let root = std::path::PathBuf::from(std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().into_owned())).join("pleamar").join("thumbnails");
    if let Ok(old) = std::fs::read_dir(&root) {
        for d in old.filter_map(Result::ok) {
            let pid = d.file_name().to_string_lossy().parse::<u32>().ok();
            if pid.is_some_and(|p| !std::path::Path::new(&format!("/proc/{p}")).exists()) {
                let _ = std::fs::remove_dir_all(d.path());
            }
        }
    }
    // SAFETY: a new eventfd, owned from here on.
    let wake_fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if wake_fd >= 0 {
        // SAFETY: just made, and owned by nothing else.
        let _ = WAKE.set(unsafe { OwnedFd::from_raw_fd(wake_fd) });
    }
    let dir = root.join(std::process::id().to_string());
    let _ = std::fs::create_dir_all(&dir);
    let mut state = State { dir, ..State::default() };
    queue.roundtrip(&mut state).ok()?;
    queue.roundtrip(&mut state).ok()?;
    // Without a list of windows there is nothing to say; without the copies,
    // the list all the same, with no pictures.
    state.list.as_ref()?;
    let state = Arc::new(Mutex::new(state));
    let _ = CONTROL.set((connection.clone(), state.clone()));
    let shared = state.clone();
    std::thread::Builder::new()
        .name("thumbnails".into())
        .spawn(move || loop {
            let wait = {
                let mut e = shared.lock().unwrap();
                if queue.dispatch_pending(&mut e).is_err() {
                    return;
                }
                e.tick(&qh);
                e.wait()
            };
            let _ = connection.flush();
            let Some(guard) = queue.prepare_read() else { continue };
            {
                use std::os::fd::AsRawFd;
                let woken = WAKE.get().map_or(-1, |w| w.as_raw_fd());
                let mut fds = [libc::pollfd { fd: guard.connection_fd().as_raw_fd(), events: libc::POLLIN, revents: 0 }, libc::pollfd { fd: woken, events: libc::POLLIN, revents: 0 }];
                // Until the next copy is due, a tenth of a second at most, or
                // the render answers.
                // SAFETY: pollfds of our own, for as long as the call.
                unsafe { libc::poll(fds.as_mut_ptr(), 2, wait) };
                if fds[1].revents & libc::POLLIN != 0 {
                    let mut n = 0u64;
                    // SAFETY: eight bytes into our own, from our eventfd.
                    unsafe { libc::read(woken, &mut n as *mut u64 as *mut libc::c_void, 8) };
                }
            }
            match guard.read() {
                Ok(_) => {}
                Err(wayland_client::backend::WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => return,
            }
        })
        .ok()?;
    Some(state)
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(e: &mut Self, registry: &wl_registry::WlRegistry, ev: wl_registry::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_registry::Event::Global { name, interface, version } = ev {
            match interface.as_str() {
                "ext_foreign_toplevel_list_v1" => e.list = Some(registry.bind(name, 1, qh, ())),
                "ext_foreign_toplevel_image_capture_source_manager_v1" => e.sources = Some(registry.bind(name, 1, qh, ())),
                "ext_image_copy_capture_manager_v1" => e.copies = Some(registry.bind(name, 1, qh, ())),
                "wl_shm" => e.shm = Some(registry.bind(name, 1, qh, ())),
                "zwp_linux_dmabuf_v1" if version >= 3 => e.dmabuf = Some(registry.bind(name, 3, qh, ())),
                _ => {}
            }
        }
    }
}

impl Dispatch<ExtForeignToplevelListV1, ()> for State {
    fn event(e: &mut Self, _: &ExtForeignToplevelListV1, ev: toplevel_list::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let toplevel_list::Event::Toplevel { toplevel } = ev {
            e.seq += 1;
            e.windows.insert(key(&toplevel), Window { handle: toplevel, seq: e.seq, id: String::new(), title: String::new(), app: String::new(), picture: String::new(), version: 0, pixels: 0, live: false, stale: false, capture: None });
        }
    }

    wayland_client::event_created_child!(State, ExtForeignToplevelListV1, [
        toplevel_list::EVT_TOPLEVEL_OPCODE => (ExtForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ExtForeignToplevelHandleV1, ()> for State {
    fn event(e: &mut Self, h: &ExtForeignToplevelHandleV1, ev: toplevel::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        let k = key(h);
        match ev {
            toplevel::Event::Identifier { identifier } => {
                if let Some(w) = e.windows.get_mut(&k) {
                    w.id = identifier;
                }
            }
            toplevel::Event::Title { title } => {
                if let Some(w) = e.windows.get_mut(&k) {
                    w.title = title;
                }
            }
            toplevel::Event::AppId { app_id } => {
                if let Some(w) = e.windows.get_mut(&k) {
                    w.app = app_id;
                }
            }
            toplevel::Event::Done => e.report(),
            toplevel::Event::Closed => {
                if let Some(w) = e.windows.remove(&k) {
                    let _ = std::fs::remove_file(e.dir.join(format!("{}.png", w.seq)));
                    forget_frame(w.seq);
                    w.handle.destroy();
                }
                e.report();
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, u32> for State {
    fn event(e: &mut Self, _: &ExtImageCopyCaptureSessionV1, ev: copy_session::Event, k: &u32, _: &Connection, _: &QueueHandle<Self>) {
        let Some(c) = e.windows.get_mut(k).and_then(|w| w.capture.as_mut()) else { return };
        // Every constraint is said again, in any order, ending with `done`: the
        // first one after a `done` starts them over. (Hyprland says the formats
        // and the card before the size.)
        if c.constrained && !matches!(ev, copy_session::Event::Done | copy_session::Event::Stopped) {
            c.formats.clear();
            c.device = None;
            c.dmabuf.clear();
            c.constrained = false;
            c.no_card = false;
        }
        match ev {
            copy_session::Event::BufferSize { width, height } => {
                c.size = Some((width, height));
            }
            copy_session::Event::DmabufDevice { device } => {
                c.device = device.get(..8).map(|b| u64::from_ne_bytes(b.try_into().unwrap()));
            }
            copy_session::Event::DmabufFormat { format, modifiers } => {
                c.dmabuf.push((format, modifiers.as_chunks::<8>().0.iter().map(|m| u64::from_ne_bytes(*m)).collect()));
            }
            copy_session::Event::Done => c.constrained = true,
            copy_session::Event::ShmFormat { format } => {
                if let wayland_client::WEnum::Value(f) = format {
                    c.formats.push(f);
                }
            }
            // The window is no longer copied (it closed, or the compositor
            // stopped): its last picture stays, and a new session is made if
            // it is still wanted.
            copy_session::Event::Stopped => {
                if let Some(w) = e.windows.get_mut(k) {
                    w.capture = None;
                    w.stale = true;
                }
                e.report();
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, u32> for State {
    fn event(e: &mut Self, _: &ExtImageCopyCaptureFrameV1, ev: copy_frame::Event, k: &u32, _: &Connection, _: &QueueHandle<Self>) {
        match ev {
            copy_frame::Event::Ready => e.taken(*k),
            copy_frame::Event::Failed { reason } => {
                // Another size coming (it says so in the session), or a frame
                // lost: asked for again a little later.
                if let Some(c) = e.windows.get_mut(k).and_then(|w| w.capture.as_mut()) {
                    if let Some(f) = c.frame.take() {
                        f.destroy();
                    }
                    c.next = Instant::now() + EVERY;
                    // The compositor would not copy into the buffer on the
                    // card: through memory, until the size changes.
                    if std::mem::take(&mut c.on_card) && matches!(reason, wayland_client::WEnum::Value(copy_frame::FailureReason::BufferConstraints)) {
                        c.card = None;
                        c.no_card = true;
                        say("a live window copied through memory: the compositor would not copy into the buffer on the card".into());
                    }
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwpLinuxBufferParamsV1, u32> for State {
    fn event(e: &mut Self, params: &ZwpLinuxBufferParamsV1, ev: zwp_linux_buffer_params_v1::Event, k: &u32, _: &Connection, _: &QueueHandle<Self>) {
        params.destroy();
        let c = e.windows.get_mut(k).and_then(|w| w.capture.as_mut());
        let on = c.as_ref().and_then(|c| c.making.clone());
        match ev {
            zwp_linux_buffer_params_v1::Event::Created { buffer } => match (c, on) {
                (Some(c), Some(on)) => {
                    c.making = None;
                    c.card = Some(Card { buffer, on, shown: 0, at: Instant::now(), checking: false, n: 0, sum: None });
                    say("live windows copied on the card".into());
                }
                // Its window went meanwhile (and the ticket with it).
                _ => buffer.destroy(),
            },
            zwp_linux_buffer_params_v1::Event::Failed => {
                if let (Some(c), Some(on)) = (c, on) {
                    c.making = None;
                    card_used(on.ticket, false);
                    c.no_card = true;
                    say("a live window copied through memory: the compositor would not take the buffer on the card".into());
                }
            }
            _ => {}
        }
    }

    wayland_client::event_created_child!(State, ZwpLinuxBufferParamsV1, [
        zwp_linux_buffer_params_v1::EVT_CREATED_OPCODE => (wl_buffer::WlBuffer, ()),
    ]);
}

macro_rules! ignore {
    ($($t:ty),*) => {$(
        impl Dispatch<$t, ()> for State {
            fn event(_: &mut Self, _: &$t, _: <$t as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}
ignore!(ExtForeignToplevelImageCaptureSourceManagerV1, ExtImageCaptureSourceV1, ExtImageCopyCaptureManagerV1, wl_shm::WlShm, wl_shm_pool::WlShmPool, wl_buffer::WlBuffer, ZwpLinuxDmabufV1);

#[cfg(test)]
mod tests {
    use super::*;

    fn on_card() -> OnCard {
        let fd = std::fs::File::open("/dev/null").unwrap().into();
        OnCard { ticket: 1, fd, fourcc: 0, modifier: 0, planes: Vec::new(), size: (1, 1), read: AtomicU64::new(0), readers: AtomicU64::new(0), skipped: AtomicU64::new(0) }
    }

    #[test]
    fn a_buffer_on_the_card_is_never_read_and_filled_at_once() {
        let on = on_card();
        // Read twice at once: not filled until both are over.
        assert!(on.claim() && on.claim());
        assert!(!on.close());
        on.release();
        assert!(!on.close());
        on.release();
        // Being filled: not read, until the copy is in.
        assert!(on.close());
        assert!(on.close(), "asked again after a failed copy");
        assert!(!on.claim());
        on.open();
        assert!(on.claim());
        // Opened while read (the buffer let go of): the read keeps its count.
        on.open();
        assert!(!on.close());
        on.release();
        assert!(on.close());
    }
}
