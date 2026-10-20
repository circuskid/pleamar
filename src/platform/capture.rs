//! `capture`: what is on a monitor, asked for by the logic. Two questions,
//! both answered at once (`sys.ask`), both with `services: "capture"`:
//!
//! - `capture.pixel(x, y[, output])`: the colour at a point, `"#rrggbb"`. A
//!   colour picker. Without `output`, `x` and `y` are on the desktop, in the
//!   compositor's logical units (the monitor that holds the point is found);
//!   with it, inside that monitor (`screen.name`), as a full-screen surface's
//!   pointer reads them.
//! - `capture.freeze(output)`: the whole monitor, as it is now, kept in memory
//!   and named `capture:DP-1?3`, which `image … = from` paints like a live
//!   window's frame. What a region picker draws under its rectangle, so that
//!   what is picked does not move while it is picked. A monitor's last freeze
//!   is kept until the next one of that monitor replaces it.
//!
//! Spoken in the standard protocols, on a connection of its own (a monitor
//! that goes away halfway through a copy can at worst end this connection,
//! never the one the scene draws on): wlr-screencopy, which wlroots
//! compositors, Hyprland, niri and pleamar-wm ship, and ext-image-copy-capture
//! with its output source where the compositor has it. A pixel is asked of
//! wlr-screencopy first, as a small region around the point, which is made upright
//! here; the pixel inside it is the one the point is on, from the monitor's
//! scale. A freeze is asked of ext-image-copy-capture first. Either one
//! alone is enough. xdg-output says where each monitor is and how big, in
//! logical units.
//!
//! Surfaces with `captures: hidden` are left out by the compositor that
//! honours it (pleamar-wm does, for every capture): nothing to do here.

use super::thumbnails::{Frame, Shm};
use super::SysValue;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use wayland_client::protocol::{wl_buffer, wl_callback, wl_output, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_image_capture_source_v1::ExtImageCaptureSourceV1, ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1::{self as copy_frame, ExtImageCopyCaptureFrameV1},
    ext_image_copy_capture_manager_v1::{self as copy_manager, ExtImageCopyCaptureManagerV1},
    ext_image_copy_capture_session_v1::{self as copy_session, ExtImageCopyCaptureSessionV1},
};
use wayland_protocols::xdg::xdg_output::zv1::client::{
    zxdg_output_manager_v1::ZxdgOutputManagerV1,
    zxdg_output_v1::{self, ZxdgOutputV1},
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

/// How long the compositor has to answer one copy.
const WAIT: Duration = Duration::from_secs(2);
/// How long it has to answer a plain round trip.
const SYNC_WAIT: Duration = Duration::from_millis(500);

/// Every monitor's last freeze, by its name.
static FROZEN: Mutex<Option<HashMap<String, Arc<Frame>>>> = Mutex::new(None);
/// Each freeze's version: one counter for all, so a name is never reused.
static VERSION: AtomicU64 = AtomicU64::new(0);
/// The connection, made the first time it is asked, and again if it ends.
static LINK: Mutex<Option<Link>> = Mutex::new(None);

/// The frame a `capture:DP-1` names (without its version), if it was frozen.
pub fn frame(name: &str) -> Option<Arc<Frame>> {
    let output = name.strip_prefix("capture:")?;
    FROZEN.lock().unwrap().as_ref()?.get(output).cloned()
}

/// `sys.ask("capture.pixel", …)` and `sys.ask("capture.freeze", …)`.
pub fn query(name: &str, args: &[SysValue]) -> Result<SysValue, String> {
    match (name, args) {
        ("capture.pixel", [SysValue::Num(x), SysValue::Num(y)]) => with_link(|l| l.pixel(*x, *y, None)).map(SysValue::Text),
        ("capture.pixel", [SysValue::Num(x), SysValue::Num(y), SysValue::Text(o)]) => with_link(|l| l.pixel(*x, *y, Some(o))).map(SysValue::Text),
        ("capture.pixel", _) => Err("`capture.pixel` takes a point, and a monitor if the point is inside it: sys.ask(\"capture.pixel\", x, y[, screen.name])".into()),
        ("capture.freeze", [SysValue::Text(o)]) => with_link(|l| l.freeze(o)).map(SysValue::Text),
        ("capture.freeze", _) => Err("`capture.freeze` takes the monitor to keep: sys.ask(\"capture.freeze\", screen.name)".into()),
        _ => Err(format!("this system cannot answer '{name}' yet")),
    }
}

fn with_link<T>(ask: impl FnOnce(&mut Link) -> Result<T, Asked>) -> Result<T, String> {
    let mut held = LINK.lock().unwrap();
    if held.is_none() {
        *held = Some(Link::connect()?);
    }
    match ask(held.as_mut().unwrap()) {
        Ok(v) => Ok(v),
        Err(Asked::No(why)) => Err(why),
        // The connection is not to be trusted any more: a new one next time.
        Err(Asked::Lost(why)) => {
            *held = None;
            Err(why)
        }
    }
}

/// Why a question was not answered: the answer is no, or the connection is gone.
enum Asked {
    No(String),
    Lost(String),
}

impl From<String> for Asked {
    fn from(why: String) -> Asked {
        Asked::No(why)
    }
}

struct Link {
    connection: Connection,
    queue: EventQueue<State>,
    qh: QueueHandle<State>,
    state: State,
}

#[derive(Default)]
struct State {
    outputs: Vec<Output>,
    shm: Option<wl_shm::WlShm>,
    wlr: Option<ZwlrScreencopyManagerV1>,
    sources: Option<ExtOutputImageCaptureSourceManagerV1>,
    copies: Option<ExtImageCopyCaptureManagerV1>,
    xdg: Option<ZxdgOutputManagerV1>,
    /// Counts monitors coming and going, so that a turn with the compositor
    /// can tell whether one came or went during it.
    changes: u64,
    /// The turns asked for and the last one answered, so that a late answer
    /// to a turn that timed out cannot pass for the next one.
    asked: u64,
    synced: u64,
    shot: Shot,
}

/// A monitor, as the compositor describes it.
struct Output {
    global: u32,
    wl: wl_output::WlOutput,
    xdg: Option<ZxdgOutputV1>,
    name: String,
    /// Where and how big, in logical units.
    at: (i32, i32),
    size: (i32, i32),
    transform: u32,
    /// The current mode, in pixels, before the transform.
    mode: Option<(i32, i32)>,
    /// `wl_output.scale`: whole numbers only, used where there is no mode.
    scale: i32,
}

impl Output {
    /// Pixels a logical unit: the mode over the logical size, to the nearest
    /// 1/120 (the steps a fractional scale is given in).
    fn scale(&self) -> f64 {
        let turned = self.transform & 1 == 1;
        match self.mode {
            Some((w, h)) if self.size.0 > 0 => snap(if turned { h } else { w } as f64 / self.size.0 as f64),
            _ => self.scale.max(1) as f64,
        }
    }

    /// Everything the questions need has been said.
    fn described(&self) -> bool {
        !self.name.is_empty() && self.size.0 > 0 && self.size.1 > 0
    }
}

/// The copy under way, as far as it has got.
#[derive(Default)]
struct Shot {
    /// The layouts the compositor offers: format, size, bytes a row.
    offers: Vec<(wl_shm::Format, (u32, u32), u32)>,
    /// wlr: every layout said (`buffer_done`, or the first one on version 1 and 2).
    described: bool,
    /// ext: the size and the formats, until `done` makes them offers.
    size: Option<(u32, u32)>,
    formats: Vec<wl_shm::Format>,
    y_invert: bool,
    transform: Option<u32>,
    ready: bool,
    failed: Option<String>,
}

impl Link {
    fn connect() -> Result<Link, String> {
        let connection = Connection::connect_to_env().map_err(|_| "capture needs a Wayland compositor".to_string())?;
        let queue = connection.new_event_queue::<State>();
        let qh = queue.handle();
        connection.display().get_registry(&qh, ());
        let mut link = Link { connection, queue, qh, state: State::default() };
        // Bounded like every other wait: a compositor that never answers is an error, not a hang.
        link.settle().map_err(|(Asked::No(why) | Asked::Lost(why))| why)?;
        let s = &link.state;
        if s.shm.is_none() || (s.wlr.is_none() && (s.sources.is_none() || s.copies.is_none())) {
            return Err("this compositor does not let a program copy the screen (neither wlr-screencopy nor ext-image-copy-capture)".into());
        }
        Ok(link)
    }

    /// Answers until `done` says so, or `within` passes.
    fn wait(&mut self, within: Duration, done: impl Fn(&State) -> bool) -> Result<(), Asked> {
        let until = Instant::now() + within;
        let lost = |e: &dyn std::fmt::Display| Asked::Lost(format!("the compositor's connection ended: {e}"));
        loop {
            self.queue.dispatch_pending(&mut self.state).map_err(|e| lost(&e))?;
            if done(&self.state) {
                return Ok(());
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                // Slow is not broken: the connection stays.
                return Err(Asked::No("the compositor did not answer in time".into()));
            }
            self.connection.flush().map_err(|e| lost(&e))?;
            let Some(guard) = self.queue.prepare_read() else { continue };
            {
                use std::os::fd::AsRawFd;
                let mut fds = [libc::pollfd { fd: guard.connection_fd().as_raw_fd(), events: libc::POLLIN, revents: 0 }];
                // SAFETY: a pollfd of our own, for as long as the call.
                unsafe { libc::poll(fds.as_mut_ptr(), 1, left.as_millis().clamp(1, i32::MAX as u128) as i32) };
            }
            match guard.read() {
                Ok(_) => {}
                Err(wayland_client::backend::WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(lost(&e)),
            }
        }
    }

    /// One turn with the compositor.
    fn sync(&mut self) -> Result<(), Asked> {
        self.state.asked += 1;
        let turn = self.state.asked;
        self.connection.display().sync(&self.qh, turn);
        self.wait(SYNC_WAIT, move |s| s.synced >= turn)
    }

    /// The monitors as they are now, so that one unplugged a moment ago is not
    /// asked for. A monitor announced during a turn is only asked about after
    /// it (its name, where it is), so turns go on until none came or went and
    /// each one has said what it is.
    fn settle(&mut self) -> Result<(), Asked> {
        for _ in 0..4 {
            let before = self.state.changes;
            if let Some(xdg) = &self.state.xdg {
                for o in self.state.outputs.iter_mut().filter(|o| o.xdg.is_none()) {
                    o.xdg = Some(xdg.get_xdg_output(&o.wl, &self.qh, o.global));
                }
            }
            self.sync()?;
            if self.state.changes == before && self.state.outputs.iter().all(Output::described) {
                break;
            }
        }
        Ok(())
    }

    fn output(&self, name: &str) -> Result<&Output, Asked> {
        self.state.outputs.iter().find(|o| o.name == name).ok_or_else(|| {
            let known: Vec<_> = self.state.outputs.iter().map(|o| o.name.as_str()).collect();
            Asked::No(format!("there is no monitor '{name}' (there are: {})", known.join(", ")))
        })
    }

    fn pixel(&mut self, x: f64, y: f64, output: Option<&str>) -> Result<String, Asked> {
        self.settle()?;
        // The point inside its monitor, in logical units, fraction kept: on a
        // scaled monitor one unit is several pixels, and they can differ.
        let (o, fx, fy) = match output {
            Some(name) => (self.output(name)?, x, y),
            None => {
                let rects: Vec<_> = self.state.outputs.iter().map(|o| (o.at, o.size)).collect();
                let k = holding(&rects, (x.floor() as i32, y.floor() as i32)).ok_or_else(|| Asked::No(format!("no monitor holds the point {x}, {y}")))?;
                let o = &self.state.outputs[k];
                (o, x - o.at.0 as f64, y - o.at.1 as f64)
            }
        };
        let (lx, ly) = (fx.floor() as i32, fy.floor() as i32);
        if lx < 0 || ly < 0 || lx >= o.size.0 || ly >= o.size.1 {
            return Err(Asked::No(format!("{x}, {y} is outside the monitor '{}' ({} × {})", o.name, o.size.0, o.size.1)));
        }
        let (wl, size, scale, transform) = (o.wl.clone(), o.size, o.scale(), o.transform);
        // The monitor's real length in pixels on each upright axis: the logical
        // size is rounded, so size × scale can be half a pixel more or less.
        let pixels = match o.mode {
            Some((w, h)) if transform & 1 == 1 => (h as i64, w as i64),
            Some((w, h)) => (w as i64, h as i64),
            None => ((size.0 as f64 * scale).round() as i64, (size.1 as f64 * scale).round() as i64),
        };
        if let Some(wlr) = self.state.wlr.clone() {
            // A region around the point, laid on the scale's pixel grid so that
            // however the compositor rounds it, it starts on a known pixel.
            let (rx, ry) = reversed(transform);
            let (ax, ay) = (Axis::around(fx, size.0, pixels.0, scale, rx), Axis::around(fy, size.1, pixels.1, scale, ry));
            let frame = wlr.capture_output_region(0, &wl, ax.start, ay.start, ax.len, ay.len, &self.qh, ());
            let (px, w, h, _) = self.copy_wlr(frame, &wl)?;
            return Ok(hex(&px[(ay.index(h) * w + ax.index(w)) as usize * 4..]));
        }
        // The whole monitor, upright, in its pixels, which may be more than its units.
        let (px, w, h, _) = self.copy_ext(&wl)?;
        let (ax, ay) = (Axis::whole(fx, scale), Axis::whole(fy, scale));
        Ok(hex(&px[(ay.index(h) * w + ax.index(w)) as usize * 4..]))
    }

    fn freeze(&mut self, name: &str) -> Result<String, Asked> {
        self.settle()?;
        let wl = self.output(name)?.wl.clone();
        let (pixels, width, height, _) = match (&self.state.sources, &self.state.copies) {
            (Some(_), Some(_)) => self.copy_ext(&wl)?,
            _ => {
                let frame = self.state.wlr.as_ref().unwrap().capture_output(0, &wl, &self.qh, ());
                self.copy_wlr(frame, &wl)?
            }
        };
        let version = VERSION.fetch_add(1, Ordering::Relaxed) + 1;
        let frame = Arc::new(Frame { version, width, height, pixels, opaque: true, card: None });
        FROZEN.lock().unwrap().get_or_insert_with(HashMap::new).insert(name.to_owned(), frame);
        Ok(format!("capture:{name}?{version}"))
    }

    /// A wlr-screencopy frame, copied and made upright: blue, green, red,
    /// alpha; and the transform it was made upright from.
    fn copy_wlr(&mut self, frame: ZwlrScreencopyFrameV1, wl: &wl_output::WlOutput) -> Result<Copy, Asked> {
        self.state.shot = Shot::default();
        let got = self.wait(WAIT, |s| s.shot.described || s.shot.failed.is_some());
        let shm = got.and_then(|_| self.buffer());
        let shm = match shm {
            Ok(shm) => shm,
            Err(e) => {
                frame.destroy();
                return Err(e);
            }
        };
        frame.copy(&shm.buffer);
        let got = self.wait(WAIT, |s| s.shot.ready || s.shot.failed.is_some());
        frame.destroy();
        got?;
        if let Some(why) = self.state.shot.failed.take() {
            return Err(Asked::No(why));
        }
        // wlr-screencopy does not say it: the monitor's own, as grim does.
        let transform = self.transform_of(wl);
        Ok(self.finish(&shm, transform))
    }

    fn transform_of(&self, wl: &wl_output::WlOutput) -> u32 {
        self.state.outputs.iter().find(|o| &o.wl == wl).map_or(0, |o| o.transform)
    }

    /// An ext-image-copy-capture copy of the whole monitor, made upright.
    fn copy_ext(&mut self, wl: &wl_output::WlOutput) -> Result<Copy, Asked> {
        let (Some(sources), Some(copies)) = (self.state.sources.clone(), self.state.copies.clone()) else {
            return Err(Asked::No("this compositor copies no whole monitor".into()));
        };
        self.state.shot = Shot::default();
        let source = sources.create_source(wl, &self.qh, ());
        let session = copies.create_session(&source, copy_manager::Options::empty(), &self.qh, ());
        let ended = |source: ExtImageCaptureSourceV1, session: ExtImageCopyCaptureSessionV1| {
            session.destroy();
            source.destroy();
        };
        let shm = self.wait(WAIT, |s| s.shot.described || s.shot.failed.is_some()).and_then(|_| self.buffer());
        let shm = match shm {
            Ok(shm) => shm,
            Err(e) => {
                ended(source, session);
                return Err(e);
            }
        };
        let frame = session.create_frame(&self.qh, ());
        frame.attach_buffer(&shm.buffer);
        frame.damage_buffer(0, 0, shm.size.0 as i32, shm.size.1 as i32);
        frame.capture();
        let got = self.wait(WAIT, |s| s.shot.ready || s.shot.failed.is_some());
        frame.destroy();
        ended(source, session);
        got?;
        if let Some(why) = self.state.shot.failed.take() {
            return Err(Asked::No(why));
        }
        // The spec has the frame say it before `ready`; one that does not is
        // taken to be laid out as the monitor is, as wlr-screencopy is.
        let transform = self.state.shot.transform.unwrap_or_else(|| self.transform_of(wl));
        Ok(self.finish(&shm, transform))
    }

    /// A buffer in a layout the compositor offered and this reads.
    fn buffer(&mut self) -> Result<Shm, Asked> {
        if let Some(why) = self.state.shot.failed.take() {
            return Err(Asked::No(why));
        }
        let shm = self.state.shm.clone().ok_or_else(|| Asked::No("no shared memory".into()))?;
        let Some(&(format, size, stride)) = self.state.shot.offers.iter().find(|(f, ..)| reads(*f)) else {
            return Err(Asked::No("the compositor offers the screen in no layout of 8 bits a colour".into()));
        };
        Shm::new(&shm, size, stride, format, &self.qh).ok_or_else(|| Asked::No("no memory for the copy".into()))
    }

    fn finish(&mut self, shm: &Shm, transform: u32) -> Copy {
        let (w, h) = shm.size;
        let mut px = packed(shm.bytes(), w, h, shm.stride);
        if matches!(shm.format, wl_shm::Format::Abgr8888 | wl_shm::Format::Xbgr8888) {
            for p in px.chunks_exact_mut(4) {
                p.swap(0, 2);
            }
        }
        if self.state.shot.y_invert {
            px = flip_rows(&px, w, h);
        }
        let (px, w, h) = upright(px, w, h, transform);
        (px, w, h, transform)
    }
}

/// Upright pixels, their width and height, and the transform undone.
type Copy = (Vec<u8>, u32, u32, u32);

/// A scale to the nearest 1/120.
fn snap(scale: f64) -> f64 {
    ((scale * 120.0).round() / 120.0).max(1.0 / 120.0)
}

fn gcd(a: i64, b: i64) -> i64 {
    if b == 0 { a.abs() } else { gcd(b, a % b) }
}

/// Which way each upright axis (x, y) runs in the copy as the compositor lays
/// it out: `true` where it runs backwards. Read off `upright` itself.
fn reversed(transform: u32) -> (bool, bool) {
    let marked: Vec<u8> = (0..2u8).flat_map(|y| (0..2u8).flat_map(move |x| [x, y, 0, 0])).collect();
    let (px, w, _) = upright(marked, 2, 2, transform);
    let at = |x: usize, y: usize| (px[(y * w as usize + x) * 4], px[(y * w as usize + x) * 4 + 1]);
    let falls = |a: (u8, u8), b: (u8, u8)| b.0 < a.0 || b.1 < a.1;
    (falls(at(0, 0), at(1, 0)), falls(at(0, 0), at(0, 1)))
}

/// One axis of a copy: the logical span asked for, and where in the copy the
/// point's pixel is. Counted in the compositor's layout (from the far end on a
/// reversed axis), where its pixels start at 0.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Axis {
    /// The span asked for, in upright logical units.
    start: i32,
    len: i32,
    /// The point's pixel, from the span's first pixel, in the compositor's layout.
    offset: i64,
    reversed: bool,
}

impl Axis {
    /// The whole monitor, already upright: the point's pixel is where it
    /// falls from the start, whatever the copy's real length.
    fn whole(at: f64, scale: f64) -> Axis {
        Axis { start: 0, len: 0, offset: (at * scale).floor() as i64, reversed: false }
    }

    /// A span around the point that starts and ends on whole pixels at this
    /// scale, so that a compositor rounding its ends to pixels (wlroots rounds,
    /// pleamar-wm truncates) cannot move them: at 1.5 an even unit, at 1.25 a
    /// fourth one. On a reversed axis the span is laid out from the far end, as
    /// the compositor lays it, and the point's pixel is counted from the
    /// monitor's real end (`pixels`), not from `whole` × `scale`.
    fn around(at: f64, whole: i32, pixels: i64, scale: f64, reversed: bool) -> Axis {
        let upright = (at * scale).floor() as i64;
        let pixel = if reversed { pixels - 1 - upright } else { upright };
        // In 120ths, as a fractional scale is given: dividing by 1.1 in floating
        // point puts pixel 33 in unit 29.999….
        let k = ((scale * 120.0).round() as i64).max(1);
        let unit = (pixel * 120).div_euclid(k);
        let step = 120 / gcd(k, 120).max(1);
        let from = (unit.div_euclid(step) * step).max(0);
        let to = ((unit + step).div_euclid(step) * step).min(whole as i64).max(from + 1);
        let first = from * k / 120;
        let start = if reversed { whole as i64 - to } else { from };
        Axis { start: start as i32, len: (to - from) as i32, offset: pixel - first, reversed }
    }

    /// The pixel's place in an upright copy `n` pixels long.
    fn index(&self, n: u32) -> u32 {
        let i = self.offset.clamp(0, n.max(1) as i64 - 1) as u32;
        if self.reversed { n - 1 - i } else { i }
    }
}

/// The layouts read here: 8 bits a colour, in either order.
fn reads(f: wl_shm::Format) -> bool {
    matches!(f, wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888 | wl_shm::Format::Abgr8888 | wl_shm::Format::Xbgr8888)
}

/// The copy's rows without the padding a compositor may put after each:
/// `w * 4` bytes a row, `h` rows.
fn packed(bytes: &[u8], w: u32, h: u32, stride: u32) -> Vec<u8> {
    let row = w as usize * 4;
    if stride as usize == row {
        return bytes[..row * h as usize].to_vec();
    }
    bytes.chunks(stride as usize).take(h as usize).flat_map(|r| &r[..row]).copied().collect()
}

/// `"#rrggbb"` of the pixel at the start of `px` (blue, green, red, alpha).
fn hex(px: &[u8]) -> String {
    format!("#{:02x}{:02x}{:02x}", px[2], px[1], px[0])
}

/// Which of the monitors (where, how big) holds the point.
fn holding(rects: &[((i32, i32), (i32, i32))], (x, y): (i32, i32)) -> Option<usize> {
    rects.iter().position(|&((ox, oy), (w, h))| x >= ox && y >= oy && x < ox + w && y < oy + h)
}

fn flip_rows(px: &[u8], w: u32, h: u32) -> Vec<u8> {
    let row = w as usize * 4;
    px.chunks_exact(row).take(h as usize).rev().flatten().copied().collect()
}

/// The copy as it is seen. A copy comes the way the monitor's pixels are laid
/// out: with the monitor's transform (`wl_output.transform`, 0 to 7, turned
/// counter-clockwise, then 4 to 7 flipped too) already applied to what is
/// seen, as a client's buffer with that `buffer_transform`. Undone here.
fn upright(px: Vec<u8>, w: u32, h: u32, transform: u32) -> (Vec<u8>, u32, u32) {
    // Undone: a quarter turn the other way; a flip is its own undoing.
    let undo = match transform {
        1 => 3,
        3 => 1,
        t => t,
    };
    if undo == 0 || undo > 7 {
        return (px, w, h);
    }
    let (w, h) = (w as usize, h as usize);
    let turned = matches!(undo, 1 | 3 | 5 | 7);
    let (dw, dh) = if turned { (h, w) } else { (w, h) };
    let mut out = vec![0u8; px.len()];
    for y in 0..h {
        for x in 0..w {
            // Flipped first, left to right, then turned.
            let fx = if undo >= 4 { w - 1 - x } else { x };
            let (dx, dy) = match undo & 3 {
                0 => (fx, y),
                1 => (y, w - 1 - fx),
                2 => (w - 1 - fx, h - 1 - y),
                _ => (h - 1 - y, fx),
            };
            let (s, d) = ((y * w + x) * 4, (dy * dw + dx) * 4);
            out[d..d + 4].copy_from_slice(&px[s..s + 4]);
        }
    }
    (out, dw as u32, dh as u32)
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(e: &mut Self, registry: &wl_registry::WlRegistry, ev: wl_registry::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        match ev {
            wl_registry::Event::Global { name, interface, version } => match interface.as_str() {
                "wl_output" => {
                    let wl: wl_output::WlOutput = registry.bind(name, version.min(4), qh, name);
                    let xdg = e.xdg.as_ref().map(|m| m.get_xdg_output(&wl, qh, name));
                    e.outputs.push(Output { global: name, wl, xdg, name: String::new(), at: (0, 0), size: (0, 0), transform: 0, mode: None, scale: 1 });
                    e.changes += 1;
                }
                "zxdg_output_manager_v1" => e.xdg = Some(registry.bind(name, version.min(3), qh, ())),
                "wl_shm" => e.shm = Some(registry.bind(name, 1, qh, ())),
                "zwlr_screencopy_manager_v1" => e.wlr = Some(registry.bind(name, version.min(3), qh, ())),
                "ext_output_image_capture_source_manager_v1" => e.sources = Some(registry.bind(name, 1, qh, ())),
                "ext_image_copy_capture_manager_v1" => e.copies = Some(registry.bind(name, 1, qh, ())),
                _ => {}
            },
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(k) = e.outputs.iter().position(|o| o.global == name) {
                    let o = e.outputs.remove(k);
                    e.changes += 1;
                    // Its last freeze goes with it: a whole monitor of memory.
                    if let Some(frozen) = FROZEN.lock().unwrap().as_mut() {
                        frozen.remove(&o.name);
                    }
                    if let Some(x) = o.xdg {
                        x.destroy();
                    }
                    if o.wl.version() >= 3 {
                        o.wl.release();
                    }
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, u32> for State {
    fn event(e: &mut Self, wl: &wl_output::WlOutput, ev: wl_output::Event, _: &u32, _: &Connection, _: &QueueHandle<Self>) {
        let Some(o) = e.outputs.iter_mut().find(|o| &o.wl == wl) else { return };
        match ev {
            wl_output::Event::Geometry { transform: WEnum::Value(t), .. } => o.transform = t as u32,
            wl_output::Event::Name { name } => o.name = name,
            wl_output::Event::Mode { flags: WEnum::Value(f), width, height, .. } if f.contains(wl_output::Mode::Current) => o.mode = Some((width, height)),
            wl_output::Event::Scale { factor } => o.scale = factor,
            _ => {}
        }
    }
}

impl Dispatch<ZxdgOutputV1, u32> for State {
    fn event(e: &mut Self, _: &ZxdgOutputV1, ev: zxdg_output_v1::Event, global: &u32, _: &Connection, _: &QueueHandle<Self>) {
        let Some(o) = e.outputs.iter_mut().find(|o| o.global == *global) else { return };
        match ev {
            zxdg_output_v1::Event::LogicalPosition { x, y } => o.at = (x, y),
            zxdg_output_v1::Event::LogicalSize { width, height } => o.size = (width, height),
            // Before version 4 of wl_output, the only place it is said.
            zxdg_output_v1::Event::Name { name } if o.name.is_empty() => o.name = name,
            _ => {}
        }
    }
}

impl Dispatch<wl_callback::WlCallback, u64> for State {
    fn event(e: &mut Self, _: &wl_callback::WlCallback, ev: wl_callback::Event, turn: &u64, _: &Connection, _: &QueueHandle<Self>) {
        if let wl_callback::Event::Done { .. } = ev {
            e.synced = e.synced.max(*turn);
        }
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for State {
    fn event(e: &mut Self, frame: &ZwlrScreencopyFrameV1, ev: zwlr_screencopy_frame_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        use zwlr_screencopy_frame_v1::Event as E;
        let shot = &mut e.shot;
        match ev {
            E::Buffer { format: WEnum::Value(format), width, height, stride } => {
                // The buffer is made with the stride asked for, padding and all;
                // `packed` takes the padding off.
                if stride >= width * 4 {
                    shot.offers.push((format, (width, height), stride));
                }
                // Before version 3 there is no `buffer_done`: this is all.
                if frame.version() < 3 {
                    shot.described = true;
                }
            }
            E::BufferDone => shot.described = true,
            E::Flags { flags: WEnum::Value(f) } => shot.y_invert = f.contains(zwlr_screencopy_frame_v1::Flags::YInvert),
            E::Ready { .. } => shot.ready = true,
            E::Failed => shot.failed = Some("the compositor would not copy the screen".into()),
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, ()> for State {
    fn event(e: &mut Self, _: &ExtImageCopyCaptureSessionV1, ev: copy_session::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        let shot = &mut e.shot;
        match ev {
            copy_session::Event::BufferSize { width, height } => shot.size = Some((width, height)),
            copy_session::Event::ShmFormat { format: WEnum::Value(f) } => shot.formats.push(f),
            copy_session::Event::Done => {
                if let Some(size) = shot.size {
                    // ext lets the client say how wide a row is: no padding.
                    shot.offers = shot.formats.iter().map(|&f| (f, size, size.0 * 4)).collect();
                }
                shot.described = true;
            }
            copy_session::Event::Stopped => {
                if !shot.ready {
                    shot.failed = Some("the compositor stopped copying the monitor".into());
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, ()> for State {
    fn event(e: &mut Self, _: &ExtImageCopyCaptureFrameV1, ev: copy_frame::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        let shot = &mut e.shot;
        match ev {
            copy_frame::Event::Transform { transform: WEnum::Value(t) } => shot.transform = Some(t as u32),
            copy_frame::Event::Ready => shot.ready = true,
            copy_frame::Event::Failed { reason } => {
                use copy_frame::FailureReason as F;
                let why = match reason {
                    WEnum::Value(F::BufferConstraints) => "the copy's buffer no longer matches the monitor (it changed size or layout)",
                    WEnum::Value(F::Stopped) => "the monitor stopped being copied (it went away)",
                    _ => "an error the compositor did not explain",
                };
                shot.failed = Some(format!("the compositor would not copy the monitor: {why}"));
            }
            _ => {}
        }
    }
}

macro_rules! ignore {
    ($($t:ty),*) => {$(
        impl Dispatch<$t, ()> for State {
            fn event(_: &mut Self, _: &$t, _: <$t as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}
ignore!(
    ZxdgOutputManagerV1,
    ZwlrScreencopyManagerV1,
    ExtOutputImageCaptureSourceManagerV1,
    ExtImageCaptureSourceV1,
    ExtImageCopyCaptureManagerV1,
    wl_shm::WlShm,
    wl_shm_pool::WlShmPool,
    wl_buffer::WlBuffer
);

#[cfg(test)]
mod tests {
    use super::*;

    /// A `w` × `h` copy whose every pixel says where it is: blue = x, green = y.
    fn marked(w: u32, h: u32) -> Vec<u8> {
        (0..h).flat_map(|y| (0..w).flat_map(move |x| [x as u8, y as u8, 0, 255])).collect()
    }

    fn at(px: &[u8], w: u32, x: u32, y: u32) -> (u8, u8) {
        let i = ((y * w + x) * 4) as usize;
        (px[i], px[i + 1])
    }

    #[test]
    fn a_pixel_is_red_green_blue_in_that_order() {
        assert_eq!(hex(&[0x10, 0x80, 0xff, 0xff]), "#ff8010");
    }

    #[test]
    fn the_point_is_found_on_its_monitor() {
        let two = [((0, 0), (1920, 1080)), ((1920, 0), (1280, 1024))];
        assert_eq!(holding(&two, (0, 0)), Some(0));
        assert_eq!(holding(&two, (1919, 1079)), Some(0));
        assert_eq!(holding(&two, (1920, 5)), Some(1));
        assert_eq!(holding(&two, (2000, 1050)), None, "below the smaller one");
        assert_eq!(holding(&two, (-1, 0)), None);
    }

    #[test]
    fn a_copy_upside_down_is_turned_over() {
        let px = flip_rows(&marked(3, 2), 3, 2);
        assert_eq!(at(&px, 3, 0, 0), (0, 1));
        assert_eq!(at(&px, 3, 2, 1), (2, 0));
    }

    #[test]
    fn an_upright_monitor_is_left_alone() {
        let (px, w, h) = upright(marked(3, 2), 3, 2, 0);
        assert_eq!((w, h), (3, 2));
        assert_eq!(px, marked(3, 2));
    }

    #[test]
    fn a_turned_monitor_is_turned_back() {
        // What a monitor turned 90 lays out: what is seen, turned a quarter
        // counter-clockwise. Turned back, the top left of the copy (seen at the
        // top right) goes to the top right again.
        let (px, w, h) = upright(marked(3, 2), 3, 2, 1);
        assert_eq!((w, h), (2, 3));
        assert_eq!(at(&px, w, 1, 0), (0, 0));
        assert_eq!(at(&px, w, 0, 0), (0, 1));
        assert_eq!(at(&px, w, 0, 2), (2, 1));
        // 270 and 90 undo each other; twice 180 is nothing.
        let (back, w2, h2) = upright(upright(marked(3, 2), 3, 2, 1).0, 2, 3, 3);
        assert_eq!((w2, h2), (3, 2));
        assert_eq!(back, marked(3, 2));
        let (half, ..) = upright(marked(3, 2), 3, 2, 2);
        assert_eq!(upright(half, 3, 2, 2).0, marked(3, 2));
    }

    #[test]
    fn a_flipped_monitor_is_flipped_back() {
        for t in 4..8 {
            let (once, w, h) = upright(marked(3, 2), 3, 2, t);
            assert_eq!(upright(once, w, h, t).0, marked(3, 2), "transform {t} is its own undoing");
        }
        let (px, ..) = upright(marked(3, 2), 3, 2, 4);
        assert_eq!(at(&px, 3, 0, 0), (2, 0));
    }

    #[test]
    fn the_pixel_is_the_one_the_point_is_on_at_scale_2() {
        // One unit at scale 2 is 2 × 2 pixels, and an edge can run through it.
        let a = Axis::around(0.1, 100, 200, 2.0, false);
        assert_eq!((a.start, a.len, a.index(2)), (0, 1, 0));
        assert_eq!(Axis::around(0.75, 100, 200, 2.0, false).index(2), 1);
        assert_eq!(Axis::around(40.75, 100, 200, 2.0, false).index(2), 1);
    }

    #[test]
    fn the_pixel_is_the_one_the_point_is_on_at_scale_one_and_a_half() {
        // At 1.5, unit 0 is pixel 0 and half of pixel 1: 0.6 is on pixel 0
        // (0.9), 0.7 on pixel 1 (1.05). The span is two units, 3 pixels.
        let a = Axis::around(0.6, 100, 150, 1.5, false);
        assert_eq!((a.start, a.len, a.index(3)), (0, 2, 0));
        assert_eq!(Axis::around(0.7, 100, 150, 1.5, false).index(3), 1);
        // Unit 3 is in the span from 2 (pixel 3) to 4: 3.2 is pixel 4.
        let a = Axis::around(3.2, 100, 150, 1.5, false);
        assert_eq!((a.start, a.len, a.index(3)), (2, 2, 1));
    }

    /// What a compositor sends for a region: the pixels it covers, in its own
    /// layout, with its ends rounded or cut to whole pixels.
    fn region(start: i32, len: i32, whole: i32, scale: f64, reversed: bool, round: bool) -> Vec<i64> {
        // wlroots turns the box with the logical size, then scales it.
        let to_px = |v: f64| if round { v.round() as i64 } else { v as i64 };
        let from = if reversed { whole - start - len } else { start } as f64;
        let first = to_px(from * scale);
        let n = if round { to_px((from + len as f64) * scale) - first } else { to_px(len as f64 * scale) };
        let pixels: Vec<i64> = (first..first + n).collect();
        // Made upright: a reversed axis comes back the other way round.
        if reversed { pixels.into_iter().rev().collect() } else { pixels }
    }

    #[test]
    fn the_pixel_is_right_however_the_compositor_rounds() {
        for scale in [1.0, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0, 121.0 / 120.0] {
            for reversed in [false, true] {
                for round in [false, true] {
                    for k in 0..400 {
                        let at = k as f64 * 0.2937 + 0.013;
                        // 120 units: a whole number of pixels at every scale here.
                        let a = Axis::around(at, 120, (120.0 * scale) as i64, scale, reversed);
                        let got = region(a.start, a.len, 120, scale, reversed, round);
                        let seen = got[a.index(got.len() as u32) as usize];
                        // The pixel the point is on, in the compositor's layout.
                        let c = if reversed { 120.0 - at } else { at };
                        let want = if reversed { (c * scale).ceil() as i64 - 1 } else { (c * scale).floor() as i64 };
                        assert_eq!(seen, want, "scale {scale}, reversed {reversed}, rounded {round}, at {at}");
                    }
                }
            }
        }
    }

    #[test]
    fn the_pixel_is_right_where_the_logical_size_was_rounded() {
        // 2560 pixels at 1.5 are 1706.67 units, said as 1707: size × scale is
        // 2560.5, half a pixel past the real end. Near both ends, either way
        // round and however the compositor rounds, the point's pixel is the
        // one counted from the real start, or the real end when reversed.
        for reversed in [false, true] {
            for round in [false, true] {
                for at in [0.1, 0.6, 0.75, 1.4, 853.3, 1705.2, 1706.1, 1706.6] {
                    let a = Axis::around(at, 1707, 2560, 1.5, reversed);
                    let got = region(a.start, a.len, 1707, 1.5, reversed, round);
                    let up = (at * 1.5_f64).floor() as i64;
                    let want = if reversed { 2559 - up } else { up };
                    assert_eq!(got[a.index(got.len() as u32) as usize], want, "reversed {reversed}, rounded {round}, at {at}");
                }
            }
        }
    }

    #[test]
    fn a_pixel_on_a_grid_line_is_in_the_cell_after_it() {
        // Pixels that start exactly on a grid line, where a division in
        // floating point lands just short of it (33 / 1.1 = 29.999…).
        for (scale, at, pixel) in [(1.1, 30.1, 33), (1.1, 60.05, 66), (1.25, 4.0, 5), (1.25, 8.1, 10), (1.75, 4.0, 7), (1.75, 8.2, 14)] {
            for reversed in [false, true] {
                for round in [false, true] {
                    let pixels = (120.0 * scale) as i64;
                    let a = Axis::around(at, 120, pixels, scale, reversed);
                    let got = region(a.start, a.len, 120, scale, reversed, round);
                    let want = if reversed { pixels - 1 - pixel } else { pixel };
                    assert_eq!(got[a.index(got.len() as u32) as usize], want, "scale {scale}, at {at}, reversed {reversed}, rounded {round}");
                }
            }
        }
    }

    #[test]
    fn the_whole_monitor_is_read_at_its_scale() {
        // 10 units at 1.5 are 15 pixels: 0.6 is pixel 0, 0.7 pixel 1, 9.9 pixel 14.
        assert_eq!(Axis::whole(0.6, 1.5).index(15), 0);
        assert_eq!(Axis::whole(0.7, 1.5).index(15), 1);
        assert_eq!(Axis::whole(9.9, 1.5).index(15), 14);
        // The copy is upright, so a rounded logical size changes nothing: 1707
        // units for 2560 pixels, and 0.75 is pixel 1 (1.125), not 0.
        assert_eq!(Axis::whole(0.75, 1.5).index(2560), 1);
        assert_eq!(Axis::whole(1706.6, 1.5).index(2560), 2559);
    }

    #[test]
    fn a_turned_monitor_runs_its_axes_backwards_where_it_should() {
        assert_eq!(reversed(0), (false, false));
        assert_eq!(reversed(2), (true, true));
        assert_eq!(reversed(4), (true, false));
        // A quarter turn: one axis runs backwards, the other does not.
        assert_eq!(reversed(1), (true, false));
        assert_eq!(reversed(3), (false, true));
    }

    #[test]
    fn a_fractional_scale_is_snapped_to_its_step() {
        // 2560 pixels over 1707 units is 1.4997…: 1.5.
        assert_eq!(snap(2560.0 / 1707.0), 1.5);
        assert_eq!(snap(1.0), 1.0);
        assert_eq!(snap(2880.0 / 2304.0), 1.25);
    }

    #[test]
    fn padded_rows_are_packed() {
        // Two rows of 3 pixels, each row 16 bytes where 12 are pixels.
        let mut bytes = Vec::new();
        for y in 0..2u8 {
            for x in 0..3u8 {
                bytes.extend([x, y, 0, 255]);
            }
            bytes.extend([0xee; 4]);
        }
        assert_eq!(packed(&bytes, 3, 2, 16), marked(3, 2));
        assert_eq!(packed(&marked(3, 2), 3, 2, 12), marked(3, 2));
    }

    #[test]
    fn a_frozen_monitor_is_found_by_its_name() {
        let f = Arc::new(Frame { version: 7, width: 1, height: 1, pixels: vec![0; 4], opaque: true, card: None });
        FROZEN.lock().unwrap().get_or_insert_with(HashMap::new).insert("TEST-9".into(), f);
        assert_eq!(frame("capture:TEST-9").map(|f| f.version), Some(7));
        assert!(frame("capture:TEST-8").is_none());
        assert!(frame("thumbnails:9").is_none());
    }
}
