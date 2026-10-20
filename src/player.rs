//! In-process libav decode, adapted from switchblade's `SeekablePlayer`
//! (the settled, soaked design: resident demuxer + VideoToolbox session per
//! stream, seek = demuxer jump + decoder flush, content-relative time,
//! drop-wakes-the-parked-reader).
//!
//! Two deliberate differences. abner plays N streams in lockstep, so the
//! per-player wall-clock pacing (due-stamped frames) is gone: the app owns
//! one master clock `t` and pops everything `pts <= t` each frame
//! (`take_upto`, newest wins), which makes A/B sync exact by construction
//! and pause/framestep trivial (stop advancing `t`).
//!
//! And abner is frame-precise: every seek from the pointer is EXACT, so
//! the bounded read-ahead queue became a **frame cache** (2026-10-14). The
//! reader retains every frame near the playhead, including nearby
//! pre-target frames of an exact seek, in a
//! pts-keyed cache bounded by a byte budget (`playback.frame_cache_mb`),
//! evicting the frame farthest from the current position. With nearby frames
//! resident, a scrub inside that span never touches the decoder
//! (`seek` serves it on the caller's thread); with budget left the reader
//! keeps decoding ahead, so the NEXT GOP is warm by the time the playhead
//! gets there. The cache IS the queue: `take_upto`/`take_next` read from
//! it, and backpressure is the budget around the position. On a cold
//! long-GOP jump, distant pre-target frames are decoded for reference
//! continuity but not converted to RGBA; the target and a nearby rewind
//! window are still cached.
//!
//! Decode is at NATIVE resolution: this is a comparison tool, pixels are
//! the product. Hardware-decoded frames download via
//! `av_hwframe_transfer_data` and convert to RGBA in software (the plain
//! `-hwaccel` parity path from switchblade); there is no scale chain.

use rsmpeg::ffi;
use std::collections::BTreeMap;
use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

pub type Notify = Arc<dyn Fn() + Send + Sync>;

/// A delivered frame: the cache keeps its own reference, so handing one
/// out is a refcount, not a copy — and the consumer just drops it.
pub type Rgba = Arc<Vec<u8>>;

/// Cache budget for players that only ever show a few frames (the launch
/// backdrop, thumbnails, the split preview).
pub const SMALL_CACHE: usize = 48 << 20;

/// Recycled buffers kept per player for playback churn: an evicted frame
/// nobody else holds goes back here instead of to the allocator. A 4K RGBA
/// frame is ~33MB — allocator churn per frame taxed both threads in
/// switchblade until the pool (its P1.5).
const POOL_CAP: usize = 4;

/// Two pts closer than this are the same frame (µs).
const SAME_FRAME_US: i64 = 1000;

pub struct Player {
    shared: Arc<Shared>,
    pub w: u32,
    pub h: u32,
}

/// Decoded frames by content-relative pts in µs, plus the bookkeeping the
/// delivery rules need: which spans are contiguous (so "the first cached
/// frame at or after t" is known to be the TRUE next frame and not the
/// far side of a gap), where the consumer is, and what it is waiting for.
struct Cache {
    frames: BTreeMap<i64, Rgba>,
    /// Contiguous decoded spans `[first, last]`, sorted, non-overlapping.
    /// A span grows as the reader decodes on; a seek starts a new one at
    /// its landing keyframe; two merge when decoding runs into cached
    /// frames. Eviction only ever trims an end.
    runs: Vec<(i64, i64)>,
    bytes: usize,
    budget: usize,
    /// pts of the last frame handed out, or the seek target while one is
    /// in flight. `i64::MIN` before the first frame.
    position: i64,
    /// An exact seek's target: the next frame out is the first cached one
    /// at or after it, once the span containing that frame is known to
    /// reach back to the target.
    pending: Option<i64>,
    /// First frame at/after `pending` reached by decoding from its seek
    /// point. This proves the exact landing when older RGBA frames were
    /// deliberately not materialized during a cold seek.
    pending_verified: Option<i64>,
    /// A keyframe seek in flight: the reader's next frame becomes the
    /// position and the pending target both.
    landing: bool,
    /// The last key the reader inserted, so the next one extends its run
    /// — or starts a fresh one after a seek (`broken`).
    last_key: i64,
    broken: bool,
    /// A background seek is filling a gap after the displayed frame.
    gap_refill: bool,
}

impl Cache {
    fn run_of(&self, key: i64) -> Option<usize> {
        self.runs.iter().position(|&(a, b)| a <= key && key <= b)
    }

    /// Whether `key` is deliverable for the pending target `t`: it sits in
    /// a span that starts at or before `t`, so nothing undecoded lies
    /// between the target and it.
    fn covers(&self, key: i64, t: i64) -> bool {
        self.run_of(key).is_some_and(|i| self.runs[i].0 <= t + SAME_FRAME_US)
    }

    /// First deliverable key: for a pending target, the first cached
    /// frame at or after it (if its span reaches back to the target);
    /// otherwise the first frame past the position.
    fn next_key(&self) -> Option<i64> {
        match self.pending {
            Some(_) if self.pending_verified.is_some() => self.pending_verified
                .filter(|k| self.frames.contains_key(k)),
            Some(t) => self
                .frames
                .range(t - SAME_FRAME_US..)
                .next()
                .map(|(k, _)| *k)
                .filter(|k| self.covers(*k, t)),
            None => self.frames.range(self.position.saturating_add(1)..).next().map(|(k, _)| *k),
        }
    }

    /// Newest deliverable key ≤ `t_us`.
    fn upto_key(&self, t_us: i64) -> Option<i64> {
        match self.pending {
            Some(_) if self.pending_verified.is_some() => {
                let first = self.pending_verified?;
                if t_us < first { return None; }
                self.frames.range(first..=t_us).rev()
                    .map(|(k, _)| *k)
                    .find(|&k| self.run_of(k) == self.run_of(first))
            }
            Some(t) => {
                let lo = t - SAME_FRAME_US;
                if t_us < lo {
                    return None;
                }
                // A later, disjoint cached run must not hide a valid frame
                // in the run containing the seek target.
                self.frames.range(lo..=t_us).rev()
                    .map(|(k, _)| *k)
                    .find(|&k| self.covers(k, t))
            }
            None => {
                let lo = self.position.saturating_add(1);
                if t_us < lo {
                    return None;
                }
                self.frames.range(lo..=t_us).rev()
                    .map(|(k, _)| *k)
                    .find(|&k| self.position == i64::MIN || self.covers(k, self.position))
            }
        }
    }

    fn deliverable(&self) -> usize {
        match self.next_key() {
            Some(k) => self.frames.range(k..).count(),
            None => 0,
        }
    }

    fn took(&mut self, key: i64) {
        self.position = key;
        self.pending = None;
        self.pending_verified = None;
        self.landing = false;
        self.gap_refill = false;
    }

    fn anchor(&self) -> i64 {
        if self.position == i64::MIN { self.pending.unwrap_or(0) } else { self.position }
    }

    /// The cached frame farthest from the position — always a span end.
    fn farthest(&self) -> Option<i64> {
        let pos = self.anchor();
        let first = self.runs.first().map(|r| r.0)?;
        let last = self.runs.last().map(|r| r.1)?;
        Some(if (pos - first).abs() >= (last - pos).abs() { first } else { last })
    }

    fn remove(&mut self, key: i64) -> Option<Rgba> {
        let f = self.frames.remove(&key)?;
        self.bytes -= f.len();
        if let Some(i) = self.run_of(key) {
            let (a, b) = self.runs[i];
            if a == b {
                self.runs.remove(i);
            } else if key == a {
                self.runs[i].0 = self.frames.range(key..).next().map_or(b, |(k, _)| *k);
            } else if key == b {
                self.runs[i].1 = self.frames.range(..key).next_back().map_or(a, |(k, _)| *k);
            } else {
                // Interior removal never happens (eviction takes ends);
                // keep the invariant anyway.
                let left = self.frames.range(..key).next_back().map(|(k, _)| *k);
                let right = self.frames.range(key..).next().map(|(k, _)| *k);
                self.runs[i].1 = left.unwrap_or(a);
                if let Some(r) = right {
                    self.runs.insert(i + 1, (r, b));
                }
            }
        }
        Some(f)
    }

    /// Record that the reader just produced `key` (cached or already
    /// present): extend or start a span, merge on contact.
    fn note_run(&mut self, key: i64) {
        let cur = (!self.broken).then(|| self.runs.iter().position(|r| r.1 == self.last_key)).flatten();
        match (self.run_of(key), cur) {
            // Decoding ran into cached territory: the current span joins it.
            (Some(j), Some(i)) if i != j => {
                let (lo, hi) = (i.min(j), i.max(j));
                let merged = (self.runs[lo].0, self.runs[hi].1);
                self.runs.remove(hi);
                self.runs[lo] = merged;
            }
            (Some(_), _) => {}
            (None, Some(i)) => {
                self.runs[i].1 = key;
                if let Some(next) = self.runs.get(i + 1).copied()
                    && next.0 <= key
                {
                    self.runs[i].1 = next.1.max(key);
                    self.runs.remove(i + 1);
                }
            }
            (None, None) => {
                let at = self.runs.partition_point(|r| r.0 < key);
                self.runs.insert(at, (key, key));
            }
        }
        self.last_key = key;
        self.broken = false;
    }
}

struct Shared {
    cache: Mutex<Cache>,
    /// Signalled when the position moves, a seek lands in the cache, or
    /// the player closes — anything that changes what the parked reader
    /// may evict.
    space: Condvar,
    /// Raised on drop: a reader parked on `space` with a full cache has
    /// nothing else to reach it there — without this wake the thread
    /// leaks, pinning its frames.
    closed: AtomicBool,
    /// Latest seek request the READER must perform (seconds, exact,
    /// forward-biased) — newest wins. Seeks the cache can serve never get
    /// here.
    cmd: Mutex<Option<(f64, bool, bool)>>,
    /// f64 bits: pts of the frame most recently taken (on screen), or the
    /// seek target while one is in flight.
    position: AtomicU64,
    failed: AtomicBool,
    /// Fired when a frame becomes deliverable with none before it — wakes
    /// an idle render loop (paused app waiting on a framestep's frame).
    notify: Mutex<Option<Notify>>,
    /// Recycled frame buffers. Lock order: may be taken while `cache` is
    /// held, never the reverse.
    pool: Mutex<Vec<Vec<u8>>>,
}

impl Shared {
    fn take_buf(&self, len: usize) -> Vec<u8> {
        let mut buf = self.pool.lock().unwrap().pop().unwrap_or_default();
        buf.resize(len, 0);
        buf
    }

    fn recycle_buf(&self, buf: Rgba) {
        if let Ok(buf) = Arc::try_unwrap(buf) {
            let mut pool = self.pool.lock().unwrap();
            if pool.len() < POOL_CAP {
                pool.push(buf);
            }
        }
    }

    fn set_position(&self, key: i64) {
        self.position.store((key as f64 * 1e-6).to_bits(), Ordering::Relaxed);
    }
}

fn us(s: f64) -> i64 {
    (s * 1e6).round() as i64
}

impl Player {
    /// Spawn a reader decoding `path` to `w × h` RGBA (the display dims —
    /// rotation applied, so pass probe's display size) into a frame cache
    /// of at most `cache_bytes` (floored at a few frames). Returns
    /// immediately; open/decode errors surface as a stream that never
    /// produces frames (`failed()`).
    pub fn spawn(
        path: &Path,
        w: u32,
        h: u32,
        use_vt: bool,
        rotation: Option<f64>,
        cache_bytes: usize,
    ) -> Option<Self> {
        let (w, h) = (w.max(2), h.max(2));
        // Rotation is explicit: libavfilter does not autorotate like the
        // ffmpeg CLI. The transpose directions mirror switchblade's
        // PSNR-verified mapping.
        let sw_pre = match rotation.map(|r| {
            let q = (r / 90.0).round();
            if (r - q * 90.0).abs() > 1.0 { -1 } else { (q as i64).rem_euclid(4) }
        }) {
            Some(1) => "transpose=2,",
            Some(3) => "transpose=1,",
            Some(2) => "hflip,vflip,",
            _ => "",
        };
        // The scale is normally identity (native res) but pins the output
        // dims so decoder cropping surprises can't break the upload path.
        let sw_chain = format!("{sw_pre}scale={w}:{h}:flags=bilinear,format=rgba");
        let cfg = ReaderCfg {
            path: path.to_path_buf(),
            cpath: c_path(path)?,
            w,
            h,
            use_vt,
            sw_chain: CString::new(sw_chain).ok()?,
        };
        let frame_bytes = (w as usize) * (h as usize) * 4;
        let shared = Arc::new(Shared {
            cache: Mutex::new(Cache {
                frames: BTreeMap::new(),
                runs: Vec::new(),
                bytes: 0,
                budget: cache_bytes.max(frame_bytes * 6),
                position: i64::MIN,
                pending: None,
                pending_verified: None,
                landing: false,
                last_key: i64::MIN,
                broken: true,
                gap_refill: false,
            }),
            space: Condvar::new(),
            closed: AtomicBool::new(false),
            cmd: Mutex::new(None),
            position: AtomicU64::new(0f64.to_bits()),
            failed: AtomicBool::new(false),
            notify: Mutex::new(None),
            pool: Mutex::new(Vec::new()),
        });
        let reader_shared = shared.clone();
        thread::spawn(move || {
            // All libav state is created, used and freed on this thread.
            // A drop aborts blocked libav I/O via the interrupt callback,
            // and that abort surfaces here as an ordinary error — a
            // closed player shutting down is not a stream failure.
            let mut result = unsafe { reader(&reader_shared, &cfg) };
            // VT can claim AV1 hardware support yet reject the actual
            // stream. If it produced nothing, reopen with libdav1d.
            if result.as_ref().is_err_and(|e| e.starts_with("hardware AV1:"))
                && !reader_shared.closed.load(Ordering::Relaxed)
                && reader_shared.cache.lock().unwrap().frames.is_empty()
            {
                log::warn!("player: {} — hardware decode failed before first frame; retrying in software", cfg.path.display());
                let mut software = cfg.clone();
                software.use_vt = false;
                result = unsafe { reader(&reader_shared, &software) };
            }
            if let Err(e) = result && !reader_shared.closed.load(Ordering::Relaxed) {
                log::warn!("player: {} — {e}", cfg.path.display());
                reader_shared.failed.store(true, Ordering::Relaxed);
            }
        });
        Some(Self { shared, w, h })
    }

    /// Jump playback. `exact` lands on the first frame at or after the
    /// target — served from the cache at once when that span is resident,
    /// else decoded forward from the preceding keyframe; otherwise the
    /// landing keyframe is delivered. The last shown frame stays on screen
    /// until the new position delivers.
    pub fn seek(&self, target_s: f64, exact: bool) {
        self.seek_biased(target_s, exact, false);
    }

    /// `seek` for a keyframe seek with a direction: `forward` lands on the
    /// first keyframe at or AFTER the target (falling back to the one
    /// before when none follows). Exact seeks ignore the bias.
    pub fn seek_biased(&self, target_s: f64, exact: bool, forward: bool) {
        let t = target_s.max(0.0);
        let key = us(t);
        let mut c = self.shared.cache.lock().unwrap();
        c.position = key;
        c.gap_refill = false;
        c.pending_verified = None;
        self.shared.set_position(key);
        let cached = exact
            && c
                .frames
                .range(key - SAME_FRAME_US..)
                .next()
                .map(|(k, _)| *k)
                .is_some_and(|k| c.covers(k, key));
        if cached {
            // Served from the cache: the reader carries on wherever it is
            // (decoding ahead), and a request it hasn't picked up yet is
            // superseded.
            c.pending = Some(key);
            c.landing = false;
            *self.shared.cmd.lock().unwrap() = None;
        } else {
            c.pending = exact.then_some(key);
            c.landing = !exact;
            *self.shared.cmd.lock().unwrap() = Some((t, exact, forward));
        }
        drop(c);
        self.shared.space.notify_all();
    }

    /// Pop the newest cached frame with `pts <= t` past the position —
    /// master-clock pacing (frames the clock ran past are skipped, so a
    /// slow decoder degrades to a lower frame rate, never to lag).
    pub fn take_upto(&self, t: f64) -> Option<(f64, Rgba)> {
        let mut c = self.shared.cache.lock().unwrap();
        let key = match c.upto_key(us(t)) {
            Some(key) => key,
            None => {
                // A cache hit can leave the reader decoding elsewhere.
                // Refill a gap before playback crosses into a later run.
                if c.pending.is_none() && !c.gap_refill && c.position != i64::MIN
                    && us(t) > c.position
                    && c.frames.range(c.position.saturating_add(1)..=us(t)).next().is_some()
                {
                    let target = c.position.saturating_add(SAME_FRAME_US + 1);
                    c.pending = Some(target);
                    c.pending_verified = None;
                    c.gap_refill = true;
                    *self.shared.cmd.lock().unwrap() = Some((target as f64 * 1e-6, true, false));
                    self.shared.space.notify_all();
                }
                return None;
            }
        };
        let f = c.frames.get(&key)?.clone();
        c.took(key);
        self.shared.set_position(key);
        drop(c);
        self.shared.space.notify_all();
        Some((key as f64 * 1e-6, f))
    }

    /// Pop the next frame regardless of the clock — used right after an
    /// exact seek while paused, when the app adopts the delivered frame's
    /// true pts as the master time (float-safe framestepping).
    pub fn take_next(&self) -> Option<(f64, Rgba)> {
        let mut c = self.shared.cache.lock().unwrap();
        let key = c.next_key()?;
        let f = c.frames.get(&key)?.clone();
        c.took(key);
        self.shared.set_position(key);
        drop(c);
        self.shared.space.notify_all();
        Some((key as f64 * 1e-6, f))
    }

    /// The cached frame at exactly `pts` (a pts this player delivered —
    /// `take_upto`/`take_next` leave frames resident, so the frame on
    /// screen is normally still here). None once eviction took it.
    pub fn frame_at(&self, pts: f64) -> Option<Rgba> {
        self.shared.cache.lock().unwrap().frames.get(&us(pts)).cloned()
    }

    /// Seconds into the clip: the pts of the frame currently on screen, or
    /// the seek target while one is in flight.
    #[allow(dead_code)]
    pub fn position(&self) -> f64 {
        f64::from_bits(self.shared.position.load(Ordering::Relaxed))
    }

    pub fn failed(&self) -> bool {
        self.shared.failed.load(Ordering::Relaxed)
    }

    /// Frames decoded and deliverable from the position on. After `seek`
    /// this is non-zero only once the new position can deliver, so
    /// `buffered() > 0` means the seek landed even when the clock won't
    /// pop the frame yet.
    pub fn buffered(&self) -> usize {
        self.shared.cache.lock().unwrap().deliverable()
    }

    /// Bytes of decoded frames resident in the cache.
    #[allow(dead_code)] // tests; kept public API
    pub fn cached_bytes(&self) -> usize {
        self.shared.cache.lock().unwrap().bytes
    }

    /// Install the wake fired when a frame becomes deliverable.
    pub fn set_notify(&self, f: Notify) {
        *self.shared.notify.lock().unwrap() = Some(f);
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.shared.closed.store(true, Ordering::Relaxed);
        // Serialize with the reader's park loop before notifying: the
        // reader checks `closed` under the `cache` lock and then waits on
        // `space`. A store+notify that lands between its check and its
        // wait is a LOST wakeup — nothing ever notifies again and the
        // reader parks forever, pinning its buffers (switchblade's
        // documented leak). Holding the lock for an instant forces the
        // reader to be either before its check (sees `closed`) or already
        // parked (gets the notify).
        drop(self.shared.cache.lock());
        self.shared.space.notify_all();
    }
}

#[derive(Clone)]
struct ReaderCfg {
    path: PathBuf,
    cpath: CString,
    w: u32,
    h: u32,
    use_vt: bool,
    sw_chain: CString,
}

#[cfg(unix)]
fn c_path(p: &Path) -> Option<CString> {
    use std::os::unix::ffi::OsStrExt;
    CString::new(p.as_os_str().as_bytes()).ok()
}
#[cfg(not(unix))]
fn c_path(p: &Path) -> Option<CString> {
    CString::new(p.to_string_lossy().as_bytes()).ok()
}

enum Flow {
    Continue,
    /// Player dropped — unwind and free everything.
    Stop,
}

unsafe extern "C" fn get_hw_format(
    _ctx: *mut ffi::AVCodecContext,
    fmts: *const ffi::AVPixelFormat,
) -> ffi::AVPixelFormat {
    unsafe {
        let mut p = fmts;
        while *p != ffi::AV_PIX_FMT_NONE {
            if *p == ffi::AV_PIX_FMT_VIDEOTOOLBOX {
                return *p;
            }
            p = p.add(1);
        }
        *fmts
    }
}

// RAII for the libav objects the reader owns, so every early return frees.
struct FmtCtx(*mut ffi::AVFormatContext);
impl Drop for FmtCtx {
    fn drop(&mut self) {
        unsafe { ffi::avformat_close_input(&mut self.0) }
    }
}
struct DecCtx(*mut ffi::AVCodecContext);
impl Drop for DecCtx {
    fn drop(&mut self) {
        unsafe { ffi::avcodec_free_context(&mut self.0) }
    }
}
struct HwDev(*mut ffi::AVBufferRef);
impl Drop for HwDev {
    fn drop(&mut self) {
        unsafe { ffi::av_buffer_unref(&mut self.0) }
    }
}
struct FramePtr(*mut ffi::AVFrame);
impl Drop for FramePtr {
    fn drop(&mut self) {
        unsafe { ffi::av_frame_free(&mut self.0) }
    }
}
struct PktPtr(*mut ffi::AVPacket);
impl Drop for PktPtr {
    fn drop(&mut self) {
        unsafe { ffi::av_packet_free(&mut self.0) }
    }
}
struct Graph {
    graph: *mut ffi::AVFilterGraph,
    src: *mut ffi::AVFilterContext,
    sink: *mut ffi::AVFilterContext,
    /// pts unit of frames coming off the sink.
    tb: f64,
}
impl Drop for Graph {
    fn drop(&mut self) {
        unsafe { ffi::avfilter_graph_free(&mut self.graph) }
    }
}

struct Pump<'a> {
    shared: &'a Shared,
    cfg: &'a ReaderCfg,
    /// Stream timebase: seconds per pts unit (and the rational itself,
    /// for declaring the buffersrc input).
    tb: f64,
    tb_q: ffi::AVRational,
    /// Absolute stream start_time in seconds (edit-list offset). Applied
    /// on seek, subtracted from every frame pts, so the whole player
    /// speaks content-relative time — files with a non-zero start_time
    /// (phone/camera/QuickTime footage) otherwise land A and B on
    /// different frames, the exact jolt an A/B tool exists to avoid.
    start_off: f64,
    graph: Option<Graph>,
    /// Destination of `av_hwframe_transfer_data` (VT frames download at
    /// native res, then convert in software).
    transfer: FramePtr,
    filtered: FramePtr,
    /// Absolute stream pts of the last frame the decoder produced,
    /// `i64::MIN` right after a seek. An exact seek ahead of this inside
    /// the current GOP needs no demuxer seek and no flush: decoding on
    /// reaches it with every reference intact (and caches the frames on
    /// the way), where a re-seek would restart the GOP.
    last_pts: i64,
    /// Exact target currently being decoded, distinct from a newer seek
    /// the caller may have queued while this decoder was working.
    active_target: Option<i64>,
    /// True once decode has shown that no frame between the preceding
    /// keyframe and the exact target was omitted by the demux seek.
    active_seen_before_target: bool,
}

/// Clamp libav's stderr chatter (the benign "No accelerated colorspace
/// conversion found from yuv420p to rgba" swscale note, once per thread
/// context) to ERROR; honor RUST_LOG=debug for decode diagnosis.
fn quiet_libav_once() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let level = if log::log_enabled!(log::Level::Debug) {
            ffi::AV_LOG_WARNING
        } else {
            ffi::AV_LOG_ERROR
        };
        unsafe { ffi::av_log_set_level(level as i32) };
    });
}

/// libav's `AVIOInterruptCB`: a non-zero return aborts whatever blocking
/// I/O the reader is inside. Without it a reader wedged in
/// `avformat_open_input`'s probe loop or `av_read_frame` (stale network
/// mount, unplugged drive, a dribbling source) ignores `closed` until the
/// OS-level I/O timeout — minutes — and every dropped player stuck there
/// keeps its buffers and its VideoToolbox session for the duration. This
/// is how a drop reaches a reader that is stuck in libav rather than
/// parked on the condvar. `opaque` points at `Shared::closed`, kept alive
/// for the whole reader lifetime by the thread's own Arc.
unsafe extern "C" fn check_closed(opaque: *mut std::ffi::c_void) -> std::os::raw::c_int {
    unsafe { (*(opaque as *const AtomicBool)).load(Ordering::Relaxed) as i32 }
}

unsafe fn reader(shared: &Shared, cfg: &ReaderCfg) -> Result<(), String> {
    quiet_libav_once();
    unsafe {
        // Pre-allocate the context so the interrupt callback is installed
        // BEFORE `avformat_open_input` — the open itself (and the probe
        // reads inside it) is one of the places a stale mount wedges.
        let mut fmt_raw: *mut ffi::AVFormatContext = ffi::avformat_alloc_context();
        if fmt_raw.is_null() {
            return Err("format alloc failed".into());
        }
        (*fmt_raw).interrupt_callback = ffi::AVIOInterruptCB {
            callback: Some(check_closed),
            opaque: &shared.closed as *const AtomicBool as *mut std::ffi::c_void,
        };
        if ffi::avformat_open_input(&mut fmt_raw, cfg.cpath.as_ptr(), ptr::null(), ptr::null_mut())
            < 0
        {
            // avformat_open_input frees a caller-supplied context on failure.
            return Err("open failed".into());
        }
        let fmt = FmtCtx(fmt_raw);
        if ffi::avformat_find_stream_info(fmt.0, ptr::null_mut()) < 0 {
            return Err("no stream info".into());
        }
        let mut codec: *const ffi::AVCodec = ptr::null();
        let sidx = ffi::av_find_best_stream(fmt.0, ffi::AVMEDIA_TYPE_VIDEO, -1, -1, &mut codec, 0);
        if sidx < 0 {
            return Err("no video stream".into());
        }
        let stream = *(*fmt.0).streams.add(sidx as usize);
        // libav's default AV1 decoder is libdav1d (software); the
        // VideoToolbox hwaccel hangs off the NATIVE `av1` decoder, which
        // has no software path at all. So it is only chosen when the caller
        // already established hardware AV1 support (`probe::vt_accel`).
        let native_av1 = cfg.use_vt && (*(*stream).codecpar).codec_id == ffi::AV_CODEC_ID_AV1;
        if native_av1 {
            let native = ffi::avcodec_find_decoder_by_name(c"av1".as_ptr());
            if !native.is_null() {
                codec = native;
            }
        }
        let stream_tb = (*stream).time_base;
        let tb = stream_tb.num as f64 / stream_tb.den as f64;
        let start_off = {
            let st = (*stream).start_time;
            if st == ffi::AV_NOPTS_VALUE { 0.0 } else { st as f64 * tb }
        };

        let dec = DecCtx(ffi::avcodec_alloc_context3(codec));
        if ffi::avcodec_parameters_to_context(dec.0, (*stream).codecpar) < 0 {
            return Err("codec params failed".into());
        }
        (*dec.0).thread_count = 0; // auto
        // Device creation failing is fine: frames arrive software.
        let mut _hw_dev = HwDev(ptr::null_mut());
        if cfg.use_vt
            && ffi::av_hwdevice_ctx_create(
                &mut _hw_dev.0,
                ffi::AV_HWDEVICE_TYPE_VIDEOTOOLBOX,
                ptr::null(),
                ptr::null_mut(),
                0,
            ) == 0
        {
            (*dec.0).hw_device_ctx = ffi::av_buffer_ref(_hw_dev.0);
            (*dec.0).get_format = Some(get_hw_format);
            // Frame threading delays output by one packet per thread after
            // every flush, and with the hardware doing the decode the
            // threads do nothing else — on a 12-core machine that was ~12
            // packets of pure latency on every seek. One thread: the first
            // frame after a seek comes out as soon as it is decoded.
            (*dec.0).thread_count = 1;
        }
        if ffi::avcodec_open2(dec.0, codec, ptr::null_mut()) < 0 {
            return Err(if native_av1 { "hardware AV1: decoder open failed" } else { "decoder open failed" }.into());
        }

        let pkt = PktPtr(ffi::av_packet_alloc());
        let frame = FramePtr(ffi::av_frame_alloc());
        let mut pump = Pump {
            shared,
            cfg,
            tb,
            tb_q: stream_tb,
            start_off,
            graph: None,
            transfer: FramePtr(ffi::av_frame_alloc()),
            filtered: FramePtr(ffi::av_frame_alloc()),
            last_pts: i64::MIN,
            active_target: None,
            active_seen_before_target: false,
        };
        let mut at_eof = false;

        // Index entry of the keyframe at or before an absolute stream ts,
        // -1 when the container keeps no usable index.
        let key_entry = |ts: i64| -> i32 {
            ffi::av_index_search_timestamp(stream, ts, ffi::AVSEEK_FLAG_BACKWARD as i32)
        };
        let seek_to = |target: f64, forward: bool| -> bool {
            // `target` is content-relative; seek in absolute stream time.
            let ts = ((target + start_off) / tb) as i64;
            let ok = (forward && ffi::avformat_seek_file(fmt.0, sidx, ts, ts, i64::MAX, 0) >= 0)
                || ffi::avformat_seek_file(
                    fmt.0,
                    sidx,
                    i64::MIN,
                    ts,
                    ts,
                    ffi::AVSEEK_FLAG_BACKWARD as i32,
                ) >= 0;
            ffi::avcodec_flush_buffers(dec.0);
            ok
        };

        loop {
            if shared.closed.load(Ordering::Relaxed) {
                return Ok(());
            }
            if let Some((target, exact, forward)) = shared.cmd.lock().unwrap().take() {
                pump.active_target = exact.then(|| us(target));
                pump.active_seen_before_target = false;
                // A seek that fails (transient SMB fault, stale handle)
                // must not be silent: the reader would carry on from
                // wherever it was while the other streams land on the
                // target, and A/B on the same frame — the product — would
                // be quietly false. Failing the player puts it in the HUD.
                //
                // An exact target AHEAD of the last decoded frame, in the
                // same GOP, needs no seek at all: decoding on from here
                // reaches it with every reference intact, and the cache
                // picks the right frame out. Forward decode is always
                // correct; the same-keyframe test is purely about not
                // decoding across GOPs when a jump would land closer.
                let ts = ((target + start_off) / tb) as i64;
                let extend = exact
                    && !at_eof
                    && pump.last_pts != i64::MIN
                    && ts > pump.last_pts
                    && key_entry(ts) >= 0
                    && key_entry(ts) == key_entry(pump.last_pts);
                if extend {
                    pump.active_seen_before_target = true;
                    continue;
                }
                if !seek_to(target, forward) {
                    return Err(format!("seek to {target:.3}s failed"));
                }
                at_eof = false;
                pump.last_pts = i64::MIN;
                // The frames from here on start a new cached span.
                shared.cache.lock().unwrap().broken = true;
                continue;
            }
            let r = ffi::av_read_frame(fmt.0, pkt.0);
            if r == ffi::AVERROR_EOF {
                // Drain the decoder's tail, then park: the app owns the
                // clock and will send a seek when it wraps.
                at_eof = true;
                let _ = ffi::avcodec_send_packet(dec.0, ptr::null());
                loop {
                    let rr = ffi::avcodec_receive_frame(dec.0, frame.0);
                    if rr < 0 {
                        break;
                    }
                    if let Flow::Stop = pump.frame_in(frame.0) {
                        return Ok(());
                    }
                }
                if native_av1 && shared.cache.lock().unwrap().frames.is_empty() {
                    return Err("hardware AV1: no decoded frames".into());
                }
                loop {
                    if shared.closed.load(Ordering::Relaxed) {
                        return Ok(());
                    }
                    if shared.cmd.lock().unwrap().is_some() {
                        break;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                continue;
            }
            if r < 0 {
                return Err(format!("read error {r}"));
            }
            if (*pkt.0).stream_index != sidx {
                ffi::av_packet_unref(pkt.0);
                continue;
            }
            let sr = ffi::avcodec_send_packet(dec.0, pkt.0);
            ffi::av_packet_unref(pkt.0);
            if sr < 0 && sr != ffi::AVERROR(ffi::EAGAIN) {
                // Corrupt packet: skip it, keep the stream alive.
                continue;
            }
            loop {
                let rr = ffi::avcodec_receive_frame(dec.0, frame.0);
                if rr == ffi::AVERROR(ffi::EAGAIN) || rr == ffi::AVERROR_EOF {
                    break;
                }
                if rr < 0 {
                    return Err(if native_av1 { "hardware AV1: decode error" } else { "decode error" }.into());
                }
                if let Flow::Stop = pump.frame_in(frame.0) {
                    return Ok(());
                }
            }
        }
    }
}

impl Pump<'_> {
    /// One decoded frame in: exact-seek discard, hw download, lazy graph
    /// build, filter, push.
    unsafe fn frame_in(&mut self, frame: *mut ffi::AVFrame) -> Flow {
        unsafe {
            let pts = (*frame).best_effort_timestamp;
            let pts_s = pts as f64 * self.tb - self.start_off;
            if pts != ffi::AV_NOPTS_VALUE {
                self.last_pts = pts;
            }
            if self.active_target.is_some_and(|t| us(pts_s) < t - SAME_FRAME_US) {
                self.active_seen_before_target = true;
            }
            // Already cached (decoding ran back into resident frames after
            // a seek): join the spans, skip the download and convert.
            {
                let mut c = self.shared.cache.lock().unwrap();
                let key = us(pts_s);
                if c.frames.contains_key(&key) {
                    c.note_run(key);
                    self.verify_target(&mut c, key);
                    let landed = self.land(&mut c, key);
                    drop(c);
                    if landed {
                        self.wake();
                    }
                    ffi::av_frame_unref(frame);
                    return Flow::Continue;
                }
                // Decoding the GOP is necessary for an exact target, but
                // turning every distant pre-target frame into full RGBA is
                // not. Keep a budget-sized rewind neighborhood instead.
                if self.active_target.is_some_and(|t| c.pending == Some(t)
                    && key < t.saturating_sub(self.rewind_us(c.budget)))
                {
                    c.broken = true;
                    ffi::av_frame_unref(frame);
                    return Flow::Continue;
                }
            }
            let is_hw = (*frame).format == ffi::AV_PIX_FMT_VIDEOTOOLBOX;
            if self.graph.is_none() {
                // Once per stream: the only way to tell from outside which
                // path a clip ended up on (VT device creation failing, or
                // a profile VT declines, both fall back silently).
                log::info!(
                    "player: {} decoding in {}",
                    self.cfg.path.display(),
                    if is_hw { "hardware (VideoToolbox)" } else { "software" }
                );
            }
            let feed = if is_hw {
                ffi::av_frame_unref(self.transfer.0);
                if ffi::av_hwframe_transfer_data(self.transfer.0, frame, 0) < 0 {
                    ffi::av_frame_unref(frame);
                    return Flow::Continue;
                }
                let _ = ffi::av_frame_copy_props(self.transfer.0, frame);
                ffi::av_frame_unref(frame);
                self.transfer.0
            } else {
                frame
            };
            if self.graph.is_none() {
                match build_graph(feed, &self.cfg.sw_chain, self.tb_q) {
                    Ok(g) => self.graph = Some(g),
                    Err(e) => {
                        log::warn!(
                            "player: filter graph failed ({e}): {}",
                            self.cfg.path.display()
                        );
                        ffi::av_frame_unref(feed);
                        self.shared.failed.store(true, Ordering::Relaxed);
                        return Flow::Stop;
                    }
                }
            }
            let (g_src, g_sink, g_tb) = {
                let g = self.graph.as_ref().unwrap();
                (g.src, g.sink, g.tb)
            };
            if ffi::av_buffersrc_add_frame(g_src, feed) < 0 {
                ffi::av_frame_unref(feed);
                return Flow::Continue;
            }
            loop {
                let r = ffi::av_buffersink_get_frame(g_sink, self.filtered.0);
                if r < 0 {
                    return Flow::Continue; // EAGAIN/EOF: need more input
                }
                let out_pts = (*self.filtered.0).pts as f64 * g_tb - self.start_off;
                let flow = self.push_rgba(out_pts);
                ffi::av_frame_unref(self.filtered.0);
                if let Flow::Stop = flow {
                    return Flow::Stop;
                }
            }
        }
    }

    /// A keyframe seek landing: the first frame after it becomes the
    /// position and the pending target, so delivery picks it. True when
    /// a frame just became deliverable with none before it.
    fn land(&self, c: &mut Cache, key: i64) -> bool {
        let dry = c.deliverable() == 0;
        if c.landing {
            c.landing = false;
            c.position = key;
            c.pending = Some(key);
            self.shared.set_position(key);
        }
        dry && c.deliverable() > 0
    }

    fn rewind_us(&self, budget: usize) -> i64 {
        let frame = self.cfg.w as usize * self.cfg.h as usize * 4;
        let frames = (budget / frame).saturating_sub(2).min(30);
        frames as i64 * 33_333
    }

    fn verify_target(&self, c: &mut Cache, key: i64) {
        if self.active_target.is_some_and(|t| c.pending == Some(t)
            && key >= t - SAME_FRAME_US && c.pending_verified.is_none()
            && (self.active_seen_before_target || key <= t + SAME_FRAME_US || t == 0))
        {
            c.pending_verified = Some(key);
        }
    }

    fn wake(&self) {
        if let Some(f) = &*self.shared.notify.lock().unwrap() {
            f();
        }
    }

    /// Copy the filtered rgba frame out and cache it.
    unsafe fn push_rgba(&mut self, pts_s: f64) -> Flow {
        unsafe {
            let f = self.filtered.0;
            let (w, h) = (self.cfg.w as usize, self.cfg.h as usize);
            if (*f).format != ffi::AV_PIX_FMT_RGBA
                || (*f).width as usize != w
                || (*f).height as usize != h
            {
                return Flow::Continue; // negotiation surprise: drop, don't crash
            }
            let key = us(pts_s);
            let need = w * h * 4;
            // Room: within budget, or something farther from the position
            // than this frame to evict. Otherwise park — until the
            // position moves, a seek arrives, or the player closes. The
            // wait runs BEFORE the RGBA copy so a parked reader holds no
            // extra pre-copied frame.
            {
                let mut c = self.shared.cache.lock().unwrap();
                loop {
                    if self.shared.closed.load(Ordering::Relaxed) {
                        return Flow::Stop;
                    }
                    if c.bytes + need <= c.budget {
                        break;
                    }
                    let pos = c.anchor();
                    if let Some(k) = c.farthest().filter(|k| (k - pos).abs() > (key - pos).abs()) {
                        if let Some(old) = c.remove(k) {
                            self.shared.recycle_buf(old);
                        }
                        continue;
                    }
                    if self.shared.cmd.lock().unwrap().is_some() {
                        // The seek owns what's next; this frame is beyond
                        // the budget's horizon anyway.
                        c.broken = true;
                        return Flow::Continue;
                    }
                    c = self.shared.space.wait(c).unwrap();
                }
            }
            let stride = (*f).linesize[0] as usize;
            let row = w * 4;
            let mut buf = self.shared.take_buf(row * h);
            let src = (*f).data[0];
            if stride == row {
                ptr::copy_nonoverlapping(src, buf.as_mut_ptr(), row * h);
            } else {
                for y in 0..h {
                    ptr::copy_nonoverlapping(src.add(y * stride), buf[y * row..].as_mut_ptr(), row);
                }
            }
            let mut c = self.shared.cache.lock().unwrap();
            if self.shared.closed.load(Ordering::Relaxed) {
                return Flow::Stop;
            }
            let dry = c.deliverable() == 0;
            c.bytes += buf.len();
            c.frames.insert(key, Arc::new(buf));
            c.note_run(key);
            self.verify_target(&mut c, key);
            if c.landing {
                c.landing = false;
                c.position = key;
                c.pending = Some(key);
                self.shared.set_position(key);
            }
            let now_live = dry && c.deliverable() > 0;
            drop(c);
            if now_live {
                self.wake();
            }
            Flow::Continue
        }
    }
}

/// buffersrc → parsed chain → buffersink, built from the first frame's
/// actual properties (format, dims, colorspace/range tags).
unsafe fn build_graph(
    frame: *const ffi::AVFrame,
    chain: &CString,
    tb: ffi::AVRational,
) -> Result<Graph, String> {
    unsafe {
        let mut g = Graph {
            graph: ffi::avfilter_graph_alloc(),
            src: ptr::null_mut(),
            sink: ptr::null_mut(),
            tb: 0.0,
        };
        if g.graph.is_null() {
            return Err("graph alloc".into());
        }
        let src_def = ffi::avfilter_get_by_name(c"buffer".as_ptr());
        let sink_def = ffi::avfilter_get_by_name(c"buffersink".as_ptr());
        g.src = ffi::avfilter_graph_alloc_filter(g.graph, src_def, c"in".as_ptr());
        if g.src.is_null() {
            return Err("buffersrc alloc".into());
        }
        let par = ffi::av_buffersrc_parameters_alloc();
        (*par).format = (*frame).format;
        (*par).width = (*frame).width;
        (*par).height = (*frame).height;
        // Colorspace/range too: buffersrc defaults them to unspecified and
        // real camera output tags bt709/tv — the mismatch renegotiates the
        // graph on the fly.
        (*par).color_space = (*frame).colorspace;
        (*par).color_range = (*frame).color_range;
        (*par).time_base = tb;
        if !(*frame).hw_frames_ctx.is_null() {
            (*par).hw_frames_ctx = (*frame).hw_frames_ctx;
        }
        let pr = ffi::av_buffersrc_parameters_set(g.src, par);
        ffi::av_free(par as *mut _);
        if pr < 0 {
            return Err("buffersrc params".into());
        }
        if ffi::avfilter_init_str(g.src, ptr::null()) < 0 {
            return Err("buffersrc init".into());
        }
        if ffi::avfilter_graph_create_filter(
            &mut g.sink,
            sink_def,
            c"out".as_ptr(),
            ptr::null(),
            ptr::null_mut(),
            g.graph,
        ) < 0
        {
            return Err("buffersink".into());
        }
        let outputs = ffi::avfilter_inout_alloc();
        let inputs = ffi::avfilter_inout_alloc();
        (*outputs).name = ffi::av_strdup(c"in".as_ptr());
        (*outputs).filter_ctx = g.src;
        (*outputs).pad_idx = 0;
        (*outputs).next = ptr::null_mut();
        (*inputs).name = ffi::av_strdup(c"out".as_ptr());
        (*inputs).filter_ctx = g.sink;
        (*inputs).pad_idx = 0;
        (*inputs).next = ptr::null_mut();
        let mut inputs = inputs;
        let mut outputs = outputs;
        let pr = ffi::avfilter_graph_parse_ptr(
            g.graph,
            chain.as_ptr(),
            &mut inputs,
            &mut outputs,
            ptr::null_mut(),
        );
        ffi::avfilter_inout_free(&mut inputs);
        ffi::avfilter_inout_free(&mut outputs);
        if pr < 0 {
            return Err("graph parse".into());
        }
        if ffi::avfilter_graph_config(g.graph, ptr::null_mut()) < 0 {
            return Err("graph config".into());
        }
        let sink_tb = ffi::av_buffersink_get_time_base(g.sink);
        g.tb = sink_tb.num as f64 / sink_tb.den as f64;
        Ok(g)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::time::Instant;

    /// Generate (once) a small h264 test clip. Returns None when ffmpeg
    /// isn't on PATH (tests skip quietly).
    fn test_clip(name: &str, secs: u32) -> Option<PathBuf> {
        let ok = Command::new("ffmpeg").arg("-version").output().is_ok();
        if !ok {
            eprintln!("skipping: ffmpeg not on PATH");
            return None;
        }
        let dir = std::env::temp_dir().join("abner_player_test");
        let _ = std::fs::create_dir_all(&dir);
        let clip = dir.join(name);
        if !clip.exists() {
            let ok = Command::new("ffmpeg")
                .args(["-y", "-v", "error", "-f", "lavfi", "-i"])
                .arg(format!("testsrc2=duration={secs}:size=320x180:rate=30"))
                .args(["-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p", "-g", "30"])
                .arg(&clip)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            assert!(ok, "failed to generate test clip");
        }
        Some(clip)
    }

    fn wait_buffered(p: &Player, n: usize, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if p.buffered() >= n {
                return true;
            }
            thread::sleep(Duration::from_millis(5));
        }
        false
    }

    #[test]
    fn pending_seek_uses_its_covered_run_even_with_a_later_cached_run() {
        let mut c = Cache {
            frames: BTreeMap::new(), runs: vec![(100_000, 120_000), (200_000, 220_000)],
            bytes: 0, budget: 1024, position: 105_000, pending: Some(105_000), pending_verified: None,
            landing: false, last_key: 220_000, broken: false, gap_refill: false,
        };
        for key in [100_000, 110_000, 120_000, 200_000, 210_000, 220_000] {
            c.frames.insert(key, Arc::new(vec![0]));
        }
        assert_eq!(c.upto_key(220_000), Some(120_000));
        c.took(120_000);
        assert_eq!(c.position, 120_000);
        assert_eq!(c.upto_key(220_000), None, "playback cannot jump over a cache gap");
    }

    /// Manual performance probe: run with
    /// `cargo test benchmark_three_4k_players -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn benchmark_three_4k_players() {
        let dir = std::env::temp_dir().join(format!("abner-bench-4k-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = dir.join("source.mp4");
        let ok = Command::new("ffmpeg")
            .args(["-y", "-v", "error", "-f", "lavfi", "-i",
                "testsrc2=duration=4:size=3840x2160:rate=24",
                "-c:v", "libx264", "-preset", "ultrafast", "-threads", "4",
                "-pix_fmt", "yuv420p", "-g", "96"])
            .arg(&clip).status().unwrap().success();
        assert!(ok, "4K fixture encode failed");
        let players: Vec<_> = (0..3).map(|_| {
            Player::spawn(&clip, 3840, 2160, true, None, 256 << 20).unwrap()
        }).collect();
        let cold = Instant::now();
        for p in &players { p.seek(3.5, true); }
        for p in &players {
            assert!(wait_buffered(p, 1, Duration::from_secs(30)), "cold 4K seek stalled");
        }
        let cold_ms = cold.elapsed().as_millis();
        let landed: Vec<_> = players.iter().map(|p| p.take_next().unwrap().0).collect();
        assert!(landed.iter().all(|&pts| (pts - landed[0]).abs() < 1.0 / 24.0));
        let cached = Instant::now();
        for p in &players { p.seek(3.42, true); }
        let cached_pts: Vec<_> = players.iter().map(|p| p.take_next().expect("cache hit").0).collect();
        assert!(cached_pts.iter().all(|&pts| pts < landed[0]), "cache seek should move to an earlier frame");
        let cached_us = cached.elapsed().as_micros();
        let cache_mib: f64 = players.iter().map(|p| p.cached_bytes() as f64).sum::<f64>() / 1048576.0;
        eprintln!("4K x3: cold={cold_ms}ms, cached={cached_us}us, cached_frames={cached_pts:?}, cache={cache_mib:.1}MiB");
        drop(players);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn master_clock_take_upto_pops_in_pts_order() {
        let Some(clip) = test_clip("clock.mp4", 4) else { return };
        let p = Player::spawn(&clip, 320, 180, true, None, SMALL_CACHE).expect("spawn");
        assert!(wait_buffered(&p, 1, Duration::from_secs(5)), "no frames");
        // Clock before the first frame: nothing pops.
        assert!(p.take_upto(-1.0).is_none());
        // Clock way ahead: everything queued pops, newest wins.
        let (pts, buf) = p.take_upto(100.0).expect("frame");
        assert!(pts >= 0.0);
        assert_eq!(buf.len(), 320 * 180 * 4);
        // The cache stays within budget while unwatched (backpressure):
        // the reader reads ahead to the budget, then parks.
        assert!(wait_buffered(&p, 8, Duration::from_secs(5)));
        thread::sleep(Duration::from_millis(100));
        assert!(p.cached_bytes() <= SMALL_CACHE, "cache over budget: {}", p.cached_bytes());
        assert!(!p.failed());
    }

    /// An exact seek forward inside the GOP being decoded continues from
    /// where the reader is instead of restarting at the keyframe, and a
    /// seek backward inside it still restarts; both land on the target.
    #[test]
    fn exact_seeks_inside_one_gop_land_forward_and_back() {
        let Some(clip) = test_clip("gop.mp4", 4) else { return };
        let p = Player::spawn(&clip, 320, 180, true, None, SMALL_CACHE).expect("spawn");
        // GOP is 30 frames at 30 fps: 1.0..2.0 is one GOP.
        p.seek(1.1, true);
        assert!(wait_buffered(&p, 1, Duration::from_secs(5)), "no frame after seek");
        let (pts, buf) = p.take_next().unwrap();
        assert!((1.08..=1.14).contains(&pts), "landed {pts}");
        drop(buf);
        p.seek(1.7, true); // forward, same GOP: extended, not re-seeked
        assert!(wait_buffered(&p, 1, Duration::from_secs(5)), "no frame after forward seek");
        let (pts, buf) = p.take_next().unwrap();
        assert!((1.68..=1.74).contains(&pts), "forward landed {pts}");
        drop(buf);
        p.seek(1.3, true); // backward, same GOP: must restart from the keyframe
        assert!(wait_buffered(&p, 1, Duration::from_secs(5)), "no frame after backward seek");
        let (pts, buf) = p.take_next().unwrap();
        assert!((1.28..=1.34).contains(&pts), "backward landed {pts}");
        drop(buf);
        p.seek(3.5, true); // forward across GOPs: a real seek
        assert!(wait_buffered(&p, 1, Duration::from_secs(5)), "no frame after far seek");
        let (pts, buf) = p.take_next().unwrap();
        assert!((3.48..=3.54).contains(&pts), "far landed {pts}");
        drop(buf);
        assert!(!p.failed());
    }

    /// Nearby decoded frames stay resident after an exact seek, including
    /// the pre-target rewind window, so nearby seeks deliver instantly on
    /// the caller's thread while the reader decodes ahead.
    #[test]
    fn exact_seeks_inside_a_cached_gop_are_served_without_the_decoder() {
        let Some(clip) = test_clip("cache.mp4", 4) else { return };
        let p = Player::spawn(&clip, 320, 180, true, None, 64 << 20).expect("spawn");
        p.seek(1.5, true);
        assert!(wait_buffered(&p, 1, Duration::from_secs(5)), "no frame after seek");
        let (pts, _) = p.take_next().unwrap();
        assert!((1.48..=1.54).contains(&pts), "landed {pts}");
        // Let the read-ahead fill the budget (64MB = 290 frames at 320x180,
        // more than the 120 in the clip).
        thread::sleep(Duration::from_millis(800));
        // The nearby pre-target frames and the next GOP are resident.
        for target in [1.0, 1.2, 1.47, 2.3, 2.9, 1.05] {
            p.seek(target, true);
            let got = p.take_next();
            let (pts, _) = got.unwrap_or_else(|| panic!("{target}: not served from cache"));
            assert!((target - 1e-3..=target + 1.0 / 30.0).contains(&pts), "{target}: landed {pts}");
        }
        assert!(!p.failed());
    }

    #[test]
    fn cold_long_gop_seek_skips_distant_rgba_but_lands_exact() {
        let Some(source) = test_clip("long-gop-source.mp4", 4) else { return };
        let clip = source.with_file_name("long-gop.mp4");
        let ok = Command::new("ffmpeg")
            .args(["-y", "-v", "error", "-i"])
            .arg(&source)
            .args(["-c:v", "libx264", "-preset", "ultrafast", "-g", "120", "-sc_threshold", "0"])
            .arg(&clip).status().unwrap().success();
        assert!(ok, "long-GOP fixture encode failed");
        let frame = 320 * 180 * 4;
        let p = Player::spawn(&clip, 320, 180, false, None, frame * 12).unwrap();
        p.seek(3.5, true);
        assert!(wait_buffered(&p, 1, Duration::from_secs(5)), "cold seek did not land");
        let (pts, _) = p.take_next().unwrap();
        assert!((3.48..=3.54).contains(&pts), "landed {pts}");
        p.seek(3.4, true);
        let (back, _) = p.take_next().expect("nearby pre-target frame is cached");
        assert!((3.38..=3.44).contains(&back), "backward landed {back}");
        assert!(p.cached_bytes() <= frame * 12);
        assert!(!p.failed());
    }

    /// The budget holds: with room for ~12 frames the cache keeps the
    /// frames nearest the position and the reader parks instead of
    /// growing.
    #[test]
    fn cache_keeps_the_frames_nearest_the_position_within_budget() {
        let Some(clip) = test_clip("budget.mp4", 4) else { return };
        let frame = 320 * 180 * 4;
        let p = Player::spawn(&clip, 320, 180, true, None, frame * 12).expect("spawn");
        assert!(wait_buffered(&p, 6, Duration::from_secs(5)), "no frames");
        thread::sleep(Duration::from_millis(300));
        assert!(p.cached_bytes() <= frame * 12, "over budget: {}", p.cached_bytes());
        // Playing through keeps delivering in order within the budget.
        let mut t = 0.0;
        let mut last = -1.0;
        while t < 2.0 {
            t += 1.0 / 30.0;
            if let Some((pts, _)) = p.take_upto(t) {
                assert!(pts > last, "out of order: {pts} after {last}");
                last = pts;
            }
            thread::sleep(Duration::from_millis(3));
        }
        assert!(last > 1.8, "playback stalled at {last}");
        assert!(p.cached_bytes() <= frame * 12, "over budget: {}", p.cached_bytes());
        assert!(!p.failed());
    }

    /// VP9 and AV1 decode (hardware where the chip has it, software
    /// otherwise) and exact-seek like everything else. Skipped quietly
    /// when the encoders aren't built into the PATH ffmpeg.
    #[test]
    fn vp9_and_av1_decode_and_seek() {
        let Some(h264) = test_clip("codecs-src.mp4", 3) else { return };
        let dir = h264.parent().unwrap();
        for (name, args) in [
            ("codecs.webm", ["-c:v", "libvpx-vp9", "-b:v", "1M", "-g", "30"]),
            ("codecs-av1.mp4", ["-c:v", "libsvtav1", "-crf", "40", "-g", "30"]),
        ] {
            let clip = dir.join(name);
            if !clip.exists() {
                let ok = Command::new("ffmpeg")
                    .args(["-y", "-v", "error", "-i"]).arg(&h264).args(args).arg(&clip)
                    .status().map(|s| s.success()).unwrap_or(false);
                if !ok {
                    eprintln!("skipping {name}: encoder unavailable");
                    continue;
                }
            }
            let codec = if name.ends_with(".webm") { "vp9" } else { "av1" };
            let p = Player::spawn(&clip, 320, 180, crate::probe::vt_accel(codec), None, SMALL_CACHE).expect("spawn");
            assert!(wait_buffered(&p, 1, Duration::from_secs(10)), "{name}: no frames");
            p.seek(1.5, true);
            assert!(wait_buffered(&p, 1, Duration::from_secs(10)), "{name}: no frame after seek");
            let (pts, _) = p.take_next().unwrap();
            assert!((1.45..=1.6).contains(&pts), "{name}: exact seek landed {pts}");
            assert!(!p.failed(), "{name}: player failed");
        }
    }

    #[test]
    fn exact_seek_lands_and_take_next_adopts() {
        let Some(clip) = test_clip("seek.mp4", 4) else { return };
        let p = Player::spawn(&clip, 320, 180, true, None, SMALL_CACHE).expect("spawn");
        assert!(wait_buffered(&p, 1, Duration::from_secs(5)), "no frames");
        p.seek(2.0, true);
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut got = None;
        while Instant::now() < deadline && got.is_none() {
            got = p.take_next();
            thread::sleep(Duration::from_millis(2));
        }
        let (pts, _) = got.expect("frame after exact seek");
        assert!((1.9..=2.2).contains(&pts), "exact seek should land ~2.0, got {pts}");
        assert!(!p.failed());
    }

    /// The whole point of the app: two players drained against one master
    /// clock must always show the same frame (within one frame period).
    #[test]
    fn two_players_stay_in_sync_on_the_master_clock() {
        let Some(clip) = test_clip("sync.mp4", 4) else { return };
        let pa = Player::spawn(&clip, 320, 180, true, None, SMALL_CACHE).expect("spawn a");
        let pb = Player::spawn(&clip, 320, 180, true, None, SMALL_CACHE).expect("spawn b");
        assert!(wait_buffered(&pa, 1, Duration::from_secs(5)));
        assert!(wait_buffered(&pb, 1, Duration::from_secs(5)));
        let (mut sa, mut sb) = (f64::NAN, f64::NAN);
        let mut t = 0.0;
        let mut worst: f64 = 0.0;
        while t < 3.0 {
            t += 0.010;
            if let Some((pts, _)) = pa.take_upto(t) {
                sa = pts;
            }
            if let Some((pts, _)) = pb.take_upto(t) {
                sb = pts;
            }
            if t > 0.5 && sa.is_finite() && sb.is_finite() {
                worst = worst.max((sa - sb).abs());
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert!(
            worst < 1.0 / 30.0 + 1e-6,
            "streams drifted: worst pts gap {worst:.4}s"
        );
    }

    #[test]
    fn dropped_player_releases_its_reader() {
        let Some(clip) = test_clip("drop.mp4", 4) else { return };
        let p = Player::spawn(&clip, 320, 180, true, None, SMALL_CACHE).expect("spawn");
        assert!(wait_buffered(&p, 8, Duration::from_secs(10)), "cache never filled");
        let shared = Arc::downgrade(&p.shared);
        drop(p);
        let deadline = Instant::now() + Duration::from_secs(3);
        while shared.upgrade().is_some() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(shared.upgrade().is_none(), "reader thread leaked after drop");
    }

    /// A reader wedged inside libav I/O (not parked on the condvar) must
    /// still die on drop. A FIFO that dribbles zeros keeps
    /// `avformat_open_input` probing forever — libav's probe never gives
    /// up while bytes keep arriving — so without the interrupt callback
    /// the thread outlives this test's deadline by minutes.
    #[test]
    fn dropped_player_interrupts_a_reader_blocked_in_libav_io() {
        let dir = std::env::temp_dir().join("abner_player_test");
        let _ = std::fs::create_dir_all(&dir);
        let fifo = dir.join("stall.fifo");
        let _ = std::fs::remove_file(&fifo);
        let made = Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !made {
            eprintln!("skipping: mkfifo unavailable");
            return;
        }
        // Dribble zeros forever; exits on EPIPE once the aborted reader
        // frees its format context and the read end closes.
        let wpath = fifo.clone();
        let writer = thread::spawn(move || {
            use std::io::Write;
            let Ok(mut f) = std::fs::OpenOptions::new().write(true).open(&wpath) else {
                return;
            };
            let chunk = [0u8; 256];
            while f.write_all(&chunk).is_ok() {
                thread::sleep(Duration::from_millis(20));
            }
        });
        let p = Player::spawn(&fifo, 320, 180, false, None, SMALL_CACHE).expect("spawn");
        // Let the reader get properly stuck inside the open's probe loop.
        thread::sleep(Duration::from_millis(300));
        let shared = Arc::downgrade(&p.shared);
        drop(p);
        let deadline = Instant::now() + Duration::from_secs(5);
        while shared.upgrade().is_some() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(shared.upgrade().is_none(), "reader thread stayed wedged in libav I/O after drop");
        let _ = writer.join();
        let _ = std::fs::remove_file(&fifo);
    }
}
