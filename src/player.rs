//! In-process decode on FilmCraft, adapted from switchblade's `SeekablePlayer` (its design:
//! resident decoder per stream, seek in place, content-relative time, drop wakes the parked
//! reader). The libav version this replaced is in git history up to the `filmcraft-player`
//! merge; what it was for still holds:
//!
//! abner plays N streams in lockstep, so there is no per-player wall-clock pacing: the app owns
//! one master clock `t` and pops everything `pts <= t` each frame (`take_upto`, newest wins),
//! which makes A/B sync exact by construction and pause/framestep trivial (stop advancing `t`).
//!
//! And abner is frame-precise: every seek from the pointer is EXACT, so the queue is a **frame
//! cache**. The reader retains every frame near the playhead, including a rewind window of
//! frames before an exact target, in a pts-keyed cache bounded by a byte budget
//! (`playback.frame_cache_mb`), evicting the frame farthest from the current position. With
//! nearby frames resident, a scrub inside that span never touches the decoder (`seek` serves it
//! on the caller's thread); with budget left the reader keeps decoding ahead, so the NEXT GOP is
//! warm by the time the playhead gets there. The cache IS the queue: `take_upto`/`take_next`
//! read from it, and backpressure is the budget around the position.
//!
//! Underneath, FilmCraft's `MediaSource` is pull-based: the reader asks for frame `i`, then
//! `i + 1`, and FilmCraft's GOP cache decodes forward without re-seeking; a jump decodes forward
//! from the preceding sync sample (the frames before the rewind window are decoded for reference
//! and never converted). Keyframe landings use the container's own sync-sample table
//! (`fc::SyncTable`); a container without an index gets exact landings. Hardware: VideoToolbox
//! for H.264, HEVC, VP9 and AV1 on Apple silicon, chosen by FilmCraft per stream, with its
//! software decoders as the fallback; the `use_vt` argument is advisory now.
//!
//! Decode is at NATIVE resolution: this is a comparison tool, pixels are the product. Frames are
//! converted to RGBA in one fixed-point pass (`fc::fit_rgba_into`); a player asked for another
//! size (thumbnails) decimates and resamples.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use crate::fc::{self, Fit};

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
        self.run_of(key)
            .is_some_and(|i| self.runs[i].0 <= t + SAME_FRAME_US)
    }

    /// First deliverable key: for a pending target, the first cached
    /// frame at or after it (if its span reaches back to the target);
    /// otherwise the first frame past the position.
    fn next_key(&self) -> Option<i64> {
        match self.pending {
            Some(_) if self.pending_verified.is_some() => self
                .pending_verified
                .filter(|k| self.frames.contains_key(k)),
            Some(t) => self
                .frames
                .range(t - SAME_FRAME_US..)
                .next()
                .map(|(k, _)| *k)
                .filter(|k| self.covers(*k, t)),
            None => self
                .frames
                .range(self.position.saturating_add(1)..)
                .next()
                .map(|(k, _)| *k),
        }
    }

    /// Newest deliverable key ≤ `t_us`.
    fn upto_key(&self, t_us: i64) -> Option<i64> {
        match self.pending {
            Some(_) if self.pending_verified.is_some() => {
                let first = self.pending_verified?;
                if t_us < first {
                    return None;
                }
                self.frames
                    .range(first..=t_us)
                    .rev()
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
                self.frames
                    .range(lo..=t_us)
                    .rev()
                    .map(|(k, _)| *k)
                    .find(|&k| self.covers(k, t))
            }
            None => {
                let lo = self.position.saturating_add(1);
                if t_us < lo {
                    return None;
                }
                self.frames
                    .range(lo..=t_us)
                    .rev()
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
        if self.position == i64::MIN {
            self.pending.unwrap_or(0)
        } else {
            self.position
        }
    }

    /// The cached frame farthest from the position — always a span end.
    fn farthest(&self) -> Option<i64> {
        let pos = self.anchor();
        let first = self.runs.first().map(|r| r.0)?;
        let last = self.runs.last().map(|r| r.1)?;
        Some(if (pos - first).abs() >= (last - pos).abs() {
            first
        } else {
            last
        })
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
        let cur = (!self.broken)
            .then(|| self.runs.iter().position(|r| r.1 == self.last_key))
            .flatten();
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
    /// Raised with every command so a decode-forward in progress inside FilmCraft gives up.
    cancel: Arc<AtomicBool>,
    /// f64 bits: pts of the frame most recently taken (on screen), or the
    /// seek target while one is in flight.
    position: AtomicU64,
    failed: AtomicBool,
    /// The reader drained the file's tail (no command outstanding): see `exhausted`.
    eof: AtomicBool,
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
        self.position
            .store((key as f64 * 1e-6).to_bits(), Ordering::Relaxed);
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
        let _ = (use_vt, rotation); // FilmCraft decides the decoder and applies the rotation
        let (w, h) = (w.max(2), h.max(2));
        let cfg = ReaderCfg {
            path: path.to_path_buf(),
            w,
            h,
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
            cancel: Arc::new(AtomicBool::new(false)),
            position: AtomicU64::new(0f64.to_bits()),
            failed: AtomicBool::new(false),
            eof: AtomicBool::new(false),
            notify: Mutex::new(None),
            pool: Mutex::new(Vec::new()),
        });
        let reader_shared = shared.clone();
        thread::Builder::new()
            .name("abner-decode".into())
            .spawn(move || {
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    reader(&reader_shared, &cfg)
                }));
                let failed = match r {
                    Ok(Ok(())) => false,
                    Ok(Err(e)) => {
                        if !reader_shared.closed.load(Ordering::Relaxed) {
                            log::warn!("player: {} — {e}", cfg.path.display());
                        }
                        true
                    }
                    Err(_) => {
                        log::error!("player: decoder panicked on {}", cfg.path.display());
                        true
                    }
                };
                if failed && !reader_shared.closed.load(Ordering::Relaxed) {
                    reader_shared.failed.store(true, Ordering::Relaxed);
                }
            })
            .ok()?;
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
            && c.frames
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
            self.shared.eof.store(false, Ordering::Release);
            c.pending = exact.then_some(key);
            c.landing = !exact;
            *self.shared.cmd.lock().unwrap() = Some((t, exact, forward));
            self.shared.cancel.store(true, Ordering::Relaxed);
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
                if c.pending.is_none()
                    && !c.gap_refill
                    && c.position != i64::MIN
                    && us(t) > c.position
                    && c.frames
                        .range(c.position.saturating_add(1)..=us(t))
                        .next()
                        .is_some()
                {
                    let target = c.position.saturating_add(SAME_FRAME_US + 1);
                    c.pending = Some(target);
                    c.pending_verified = None;
                    c.gap_refill = true;
                    *self.shared.cmd.lock().unwrap() = Some((target as f64 * 1e-6, true, false));
                    self.shared.cancel.store(true, Ordering::Relaxed);
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

    /// Seconds into the clip: the pts of the frame currently on screen, or
    /// the seek target while one is in flight.
    /// The cached frame at exactly `pts` (a pts this player delivered —
    /// `take_upto`/`take_next` leave frames resident, so the frame on
    /// screen is normally still here). None once eviction took it.
    pub fn frame_at(&self, pts: f64) -> Option<Rgba> {
        self.shared.cache.lock().unwrap().frames.get(&us(pts)).cloned()
    }

    #[allow(dead_code)]
    pub fn position(&self) -> f64 {
        f64::from_bits(self.shared.position.load(Ordering::Relaxed))
    }

    /// True only after the decoder has drained its tail and no frames remain.
    pub fn exhausted(&self) -> bool {
        let no_seek = self.shared.cmd.lock().unwrap().is_none();
        no_seek && self.shared.eof.load(Ordering::Acquire) && self.buffered() == 0
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
        self.shared.cancel.store(true, Ordering::Relaxed);
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
    w: u32,
    h: u32,
}

enum Flow {
    Continue,
    Stop,
}

/// The reader thread: decode into the cache until dropped, following the commands.
fn reader(shared: &Arc<Shared>, cfg: &ReaderCfg) -> Result<(), String> {
    let src = fc::open(&cfg.path)?;
    let (frame_s, frames) = fc::timing(&src).ok_or_else(|| "no video stream".to_string())?;
    let sync = fc::SyncTable::open(&cfg.path);
    let to_index = |t: f64| ((t.max(0.0) / frame_s).round() as u64).min(frames.saturating_sub(1));
    let key_of = |index: u64| us(index as f64 * frame_s);
    // The keyframe index at or before `index` (the index itself without a sync table).
    let keyframe_before = |index: u64| match &sync {
        Some(s) => to_index(s.before(index as f64 * frame_s)),
        None => index,
    };
    let frame_bytes = cfg.w as usize * cfg.h as usize * 4;
    let mut hw_noted = false;
    let hw0 = fc::hw_frames();
    let mut index: u64 = 0;
    let mut last_index: Option<u64> = None;
    let mut at_end = false;
    let mut active_target: Option<i64> = None;
    let mut seen_before_target = false;
    loop {
        if shared.closed.load(Ordering::Relaxed) {
            return Ok(());
        }
        if let Some((target, exact, forward)) = shared.cmd.lock().unwrap().take() {
            shared.eof.store(false, Ordering::Release);
            let t_idx = to_index(target);
            if exact {
                active_target = Some(key_of(t_idx));
                // Already decoding towards it inside the same GOP: carry on (the old reader's
                // `extend`), nothing before the target is lost.
                let extend = !at_end
                    && last_index
                        .is_some_and(|l| t_idx > l && keyframe_before(t_idx) == keyframe_before(l));
                if extend {
                    seen_before_target = true;
                    continue;
                }
                // A rewind window of frames before the target, bounded by the budget (as
                // `rewind_us` was): the keyframe decode is paid once, the frames nearest the
                // target are what a scrub back wants.
                let budget = shared.cache.lock().unwrap().budget;
                let rewind = (budget / frame_bytes.max(1)).saturating_sub(2).min(30) as u64;
                let start = keyframe_before(t_idx).max(t_idx.saturating_sub(rewind));
                seen_before_target = start < t_idx || t_idx == 0;
                index = start;
            } else {
                active_target = None;
                seen_before_target = false;
                index = match &sync {
                    Some(s) if forward => {
                        to_index(s.at_or_after(target).unwrap_or_else(|| s.before(target)))
                    }
                    Some(s) => to_index(s.before(target)),
                    None => t_idx,
                };
            }
            shared.cache.lock().unwrap().broken = true;
            last_index = None;
            at_end = false;
            continue;
        }
        if at_end {
            // Nothing more to decode until a command: park (the cache keeps what it has).
            shared.eof.store(true, Ordering::Release);
            let c = shared.cache.lock().unwrap();
            let _c = shared
                .space
                .wait_timeout(c, Duration::from_millis(10))
                .unwrap()
                .0;
            continue;
        }
        let key = key_of(index);
        // Already cached (a seek landed inside a resident span): bookkeeping only.
        {
            let mut c = shared.cache.lock().unwrap();
            if c.frames.contains_key(&key) {
                c.note_run(key);
                verify_target(&mut c, key, active_target, seen_before_target);
                let landed = land(shared, &mut c, key);
                drop(c);
                if landed {
                    wake(shared);
                }
                last_index = Some(index);
                index += 1;
                if index >= frames {
                    at_end = true;
                }
                continue;
            }
        }
        shared.cancel.store(false, Ordering::Relaxed);
        let frame = match fc::frame_at(&src, index, frame_s, &shared.cancel) {
            Ok(f) => f,
            Err(true) => continue, // cancelled by a command or a drop
            Err(false) => {
                if index + 1 >= frames || index > 0 {
                    // The tail of the file is shorter than its duration says, or a damaged
                    // sample: stop here, what is cached stays deliverable.
                    at_end = true;
                    continue;
                }
                return Err(format!("frame {index} failed to decode"));
            }
        };
        if !hw_noted {
            hw_noted = true;
            log::info!(
                "player: {} decoding in {}",
                cfg.path.display(),
                if fc::hw_frames() > hw0 {
                    "hardware (VideoToolbox)"
                } else {
                    "software"
                }
            );
        }
        if active_target.is_some_and(|t| key < t - SAME_FRAME_US) {
            seen_before_target = true;
        }
        let mut buf = shared.take_buf(frame_bytes);
        fc::fit_rgba_into(&frame, Fit::Exact { w: cfg.w, h: cfg.h }, &mut buf);
        drop(frame);
        if buf.len() != frame_bytes {
            buf.resize(frame_bytes, 0);
        }
        match push(shared, key, buf, active_target, seen_before_target) {
            Flow::Stop => return Ok(()),
            Flow::Continue => {}
        }
        last_index = Some(index);
        index += 1;
        if index >= frames {
            at_end = true;
        }
    }
}

/// Put a converted frame in the cache, evicting the frames farthest from the position to make
/// room, or parking while the budget is full of nearer ones (the libav reader's `push_rgba`).
fn push(
    shared: &Arc<Shared>,
    key: i64,
    buf: Vec<u8>,
    active_target: Option<i64>,
    seen_before: bool,
) -> Flow {
    let need = buf.len();
    let mut c = shared.cache.lock().unwrap();
    loop {
        if shared.closed.load(Ordering::Relaxed) {
            return Flow::Stop;
        }
        if c.bytes + need <= c.budget {
            break;
        }
        let pos = c.anchor();
        if let Some(k) = c.farthest().filter(|k| (k - pos).abs() > (key - pos).abs()) {
            if let Some(old) = c.remove(k) {
                shared.recycle_buf(old);
            }
            continue;
        }
        if shared.cmd.lock().unwrap().is_some() {
            c.broken = true;
            return Flow::Continue;
        }
        c = shared.space.wait(c).unwrap();
    }
    let dry = c.deliverable() == 0;
    c.bytes += buf.len();
    c.frames.insert(key, Arc::new(buf));
    c.note_run(key);
    verify_target(&mut c, key, active_target, seen_before);
    if c.landing {
        c.landing = false;
        c.position = key;
        c.pending = Some(key);
        shared.set_position(key);
    }
    let now_live = dry && c.deliverable() > 0;
    drop(c);
    if now_live {
        wake(shared);
    }
    Flow::Continue
}

fn land(shared: &Shared, c: &mut Cache, key: i64) -> bool {
    let dry = c.deliverable() == 0;
    if c.landing {
        c.landing = false;
        c.position = key;
        c.pending = Some(key);
        shared.set_position(key);
    }
    dry && c.deliverable() > 0
}

fn verify_target(c: &mut Cache, key: i64, active_target: Option<i64>, seen_before: bool) {
    if active_target.is_some_and(|t| {
        c.pending == Some(t)
            && key >= t - SAME_FRAME_US
            && c.pending_verified.is_none()
            && (seen_before || key <= t + SAME_FRAME_US || t == 0)
    }) {
        c.pending_verified = Some(key);
    }
}

fn wake(shared: &Shared) {
    if let Some(f) = &*shared.notify.lock().unwrap() {
        f();
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
                .args([
                    "-c:v",
                    "libx264",
                    "-preset",
                    "ultrafast",
                    "-pix_fmt",
                    "yuv420p",
                    "-g",
                    "30",
                ])
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
            frames: BTreeMap::new(),
            runs: vec![(100_000, 120_000), (200_000, 220_000)],
            bytes: 0,
            budget: 1024,
            position: 105_000,
            pending: Some(105_000),
            pending_verified: None,
            landing: false,
            last_key: 220_000,
            broken: false,
            gap_refill: false,
        };
        for key in [100_000, 110_000, 120_000, 200_000, 210_000, 220_000] {
            c.frames.insert(key, Arc::new(vec![0]));
        }
        assert_eq!(c.upto_key(220_000), Some(120_000));
        c.took(120_000);
        assert_eq!(c.position, 120_000);
        assert_eq!(
            c.upto_key(220_000),
            None,
            "playback cannot jump over a cache gap"
        );
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
            .args([
                "-y",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=duration=4:size=3840x2160:rate=24",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-threads",
                "4",
                "-pix_fmt",
                "yuv420p",
                "-g",
                "96",
            ])
            .arg(&clip)
            .status()
            .unwrap()
            .success();
        assert!(ok, "4K fixture encode failed");
        let n: usize = std::env::var("ABNER_BENCH_PLAYERS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3);
        let players: Vec<_> = (0..n)
            .map(|_| Player::spawn(&clip, 3840, 2160, true, None, 256 << 20).unwrap())
            .collect();
        let cold = Instant::now();
        for p in &players {
            p.seek(3.5, true);
        }
        for p in &players {
            assert!(
                wait_buffered(p, 1, Duration::from_secs(30)),
                "cold 4K seek stalled"
            );
        }
        let cold_ms = cold.elapsed().as_millis();
        let landed: Vec<_> = players.iter().map(|p| p.take_next().unwrap().0).collect();
        assert!(
            landed
                .iter()
                .all(|&pts| (pts - landed[0]).abs() < 1.0 / 24.0)
        );
        let cached = Instant::now();
        for p in &players {
            p.seek(3.42, true);
        }
        let cached_pts: Vec<_> = players
            .iter()
            .map(|p| p.take_next().expect("cache hit").0)
            .collect();
        assert!(
            cached_pts.iter().all(|&pts| pts < landed[0]),
            "cache seek should move to an earlier frame"
        );
        let cached_us = cached.elapsed().as_micros();
        let cache_mib: f64 =
            players.iter().map(|p| p.cached_bytes() as f64).sum::<f64>() / 1048576.0;
        eprintln!(
            "4K x3: cold={cold_ms}ms, cached={cached_us}us, cached_frames={cached_pts:?}, cache={cache_mib:.1}MiB"
        );
        drop(players);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn master_clock_take_upto_pops_in_pts_order() {
        let Some(clip) = test_clip("clock.mp4", 4) else {
            return;
        };
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
        assert!(
            p.cached_bytes() <= SMALL_CACHE,
            "cache over budget: {}",
            p.cached_bytes()
        );
        assert!(!p.failed());
    }

    /// An exact seek forward inside the GOP being decoded continues from
    /// where the reader is instead of restarting at the keyframe, and a
    /// seek backward inside it still restarts; both land on the target.
    #[test]
    fn exact_seeks_inside_one_gop_land_forward_and_back() {
        let Some(clip) = test_clip("gop.mp4", 4) else {
            return;
        };
        let p = Player::spawn(&clip, 320, 180, true, None, SMALL_CACHE).expect("spawn");
        // GOP is 30 frames at 30 fps: 1.0..2.0 is one GOP.
        p.seek(1.1, true);
        assert!(
            wait_buffered(&p, 1, Duration::from_secs(5)),
            "no frame after seek"
        );
        let (pts, buf) = p.take_next().unwrap();
        assert!((1.08..=1.14).contains(&pts), "landed {pts}");
        drop(buf);
        p.seek(1.7, true); // forward, same GOP: extended, not re-seeked
        assert!(
            wait_buffered(&p, 1, Duration::from_secs(5)),
            "no frame after forward seek"
        );
        let (pts, buf) = p.take_next().unwrap();
        assert!((1.68..=1.74).contains(&pts), "forward landed {pts}");
        drop(buf);
        p.seek(1.3, true); // backward, same GOP: must restart from the keyframe
        assert!(
            wait_buffered(&p, 1, Duration::from_secs(5)),
            "no frame after backward seek"
        );
        let (pts, buf) = p.take_next().unwrap();
        assert!((1.28..=1.34).contains(&pts), "backward landed {pts}");
        drop(buf);
        p.seek(3.5, true); // forward across GOPs: a real seek
        assert!(
            wait_buffered(&p, 1, Duration::from_secs(5)),
            "no frame after far seek"
        );
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
        let Some(clip) = test_clip("cache.mp4", 4) else {
            return;
        };
        let p = Player::spawn(&clip, 320, 180, true, None, 64 << 20).expect("spawn");
        p.seek(1.5, true);
        assert!(
            wait_buffered(&p, 1, Duration::from_secs(5)),
            "no frame after seek"
        );
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
            assert!(
                (target - 1e-3..=target + 1.0 / 30.0).contains(&pts),
                "{target}: landed {pts}"
            );
        }
        assert!(!p.failed());
    }

    #[test]
    fn cold_long_gop_seek_skips_distant_rgba_but_lands_exact() {
        let Some(source) = test_clip("long-gop-source.mp4", 4) else {
            return;
        };
        let clip = source.with_file_name("long-gop.mp4");
        let ok = Command::new("ffmpeg")
            .args(["-y", "-v", "error", "-i"])
            .arg(&source)
            .args([
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-g",
                "120",
                "-sc_threshold",
                "0",
            ])
            .arg(&clip)
            .status()
            .unwrap()
            .success();
        assert!(ok, "long-GOP fixture encode failed");
        let frame = 320 * 180 * 4;
        let p = Player::spawn(&clip, 320, 180, false, None, frame * 12).unwrap();
        p.seek(3.5, true);
        assert!(
            wait_buffered(&p, 1, Duration::from_secs(5)),
            "cold seek did not land"
        );
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
        let Some(clip) = test_clip("budget.mp4", 4) else {
            return;
        };
        let frame = 320 * 180 * 4;
        let p = Player::spawn(&clip, 320, 180, true, None, frame * 12).expect("spawn");
        assert!(wait_buffered(&p, 6, Duration::from_secs(5)), "no frames");
        thread::sleep(Duration::from_millis(300));
        assert!(
            p.cached_bytes() <= frame * 12,
            "over budget: {}",
            p.cached_bytes()
        );
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
        assert!(
            p.cached_bytes() <= frame * 12,
            "over budget: {}",
            p.cached_bytes()
        );
        assert!(!p.failed());
    }

    /// VP9 and AV1 decode (hardware where the chip has it, software
    /// otherwise) and exact-seek like everything else. Skipped quietly
    /// when the encoders aren't built into the PATH ffmpeg.
    #[test]
    fn vp9_and_av1_decode_and_seek() {
        let Some(h264) = test_clip("codecs-src.mp4", 3) else {
            return;
        };
        let dir = h264.parent().unwrap();
        for (name, args) in [
            (
                "codecs.webm",
                ["-c:v", "libvpx-vp9", "-b:v", "1M", "-g", "30"],
            ),
            (
                "codecs-av1.mp4",
                ["-c:v", "libsvtav1", "-crf", "40", "-g", "30"],
            ),
        ] {
            let clip = dir.join(name);
            if !clip.exists() {
                let ok = Command::new("ffmpeg")
                    .args(["-y", "-v", "error", "-i"])
                    .arg(&h264)
                    .args(args)
                    .arg(&clip)
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                if !ok {
                    eprintln!("skipping {name}: encoder unavailable");
                    continue;
                }
            }
            let codec = if name.ends_with(".webm") {
                "vp9"
            } else {
                "av1"
            };
            let p = Player::spawn(
                &clip,
                320,
                180,
                crate::probe::vt_accel(codec),
                None,
                SMALL_CACHE,
            )
            .expect("spawn");
            assert!(
                wait_buffered(&p, 1, Duration::from_secs(10)),
                "{name}: no frames"
            );
            p.seek(1.5, true);
            assert!(
                wait_buffered(&p, 1, Duration::from_secs(10)),
                "{name}: no frame after seek"
            );
            let (pts, _) = p.take_next().unwrap();
            assert!(
                (1.45..=1.6).contains(&pts),
                "{name}: exact seek landed {pts}"
            );
            assert!(!p.failed(), "{name}: player failed");
        }
    }

    #[test]
    fn exact_seek_lands_and_take_next_adopts() {
        let Some(clip) = test_clip("seek.mp4", 4) else {
            return;
        };
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
        assert!(
            (1.9..=2.2).contains(&pts),
            "exact seek should land ~2.0, got {pts}"
        );
        assert!(!p.failed());
    }

    /// The whole point of the app: two players drained against one master
    /// clock must always show the same frame (within one frame period).
    #[test]
    fn two_players_stay_in_sync_on_the_master_clock() {
        let Some(clip) = test_clip("sync.mp4", 4) else {
            return;
        };
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
        let Some(clip) = test_clip("drop.mp4", 4) else {
            return;
        };
        let p = Player::spawn(&clip, 320, 180, true, None, SMALL_CACHE).expect("spawn");
        assert!(
            wait_buffered(&p, 8, Duration::from_secs(10)),
            "cache never filled"
        );
        let shared = Arc::downgrade(&p.shared);
        drop(p);
        let deadline = Instant::now() + Duration::from_secs(3);
        while shared.upgrade().is_some() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            shared.upgrade().is_none(),
            "reader thread leaked after drop"
        );
    }

    /// A reader wedged inside libav I/O (not parked on the condvar) must
    /// still die on drop. A FIFO that dribbles zeros keeps
    /// `avformat_open_input` probing forever — libav's probe never gives
    /// up while bytes keep arriving — so without the interrupt callback
    /// the thread outlives this test's deadline by minutes.
    /// A source that cannot be read (a FIFO is not a file; a path that is not there) fails
    /// promptly instead of hanging a reader, and a drop still releases it.
    #[test]
    fn unreadable_source_fails_instead_of_hanging() {
        let dir = std::env::temp_dir().join("abner_player_test");
        let _ = std::fs::create_dir_all(&dir);
        let fifo = dir.join("stall.fifo");
        let _ = std::fs::remove_file(&fifo);
        let made = Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        let path = if made {
            fifo.clone()
        } else {
            dir.join("missing.mp4")
        };
        let p = Player::spawn(&path, 320, 180, false, None, SMALL_CACHE).expect("spawn");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !p.failed() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            p.failed(),
            "an unreadable source is reported, not waited on"
        );
        let shared = Arc::downgrade(&p.shared);
        drop(p);
        let deadline = Instant::now() + Duration::from_secs(3);
        while shared.upgrade().is_some() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(shared.upgrade().is_none(), "reader released after the drop");
        let _ = std::fs::remove_file(&fifo);
    }
}

#[cfg(test)]
mod parity_tests {
    use super::*;
    use std::process::Command;
    use std::time::{Duration, Instant};

    /// The RGBA the player hands out for a frame is ffmpeg's RGBA conversion of the same frame,
    /// within the rounding of two different fixed-point converters: the pictures themselves
    /// are bit-exact (codec-bench), so what this guards is `fc::fit_rgba_into`'s colour maths
    /// (matrix, range, chroma siting) against a second implementation.
    #[test]
    fn delivered_rgba_matches_ffmpegs_conversion() {
        if Command::new("ffmpeg").arg("-version").output().is_err() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = std::env::temp_dir().join("abner_player_test");
        let _ = std::fs::create_dir_all(&dir);
        let clip = dir.join("parity.mp4");
        let ok = Command::new("ffmpeg")
            .args(["-y", "-v", "error", "-f", "lavfi", "-i", "testsrc2=duration=2:size=320x180:rate=30,noise=alls=8:allf=t"])
            .args(["-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p", "-g", "30", "-colorspace", "bt709", "-color_primaries", "bt709", "-color_trc", "bt709"])
            .arg(&clip)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "fixture");
        // frame 10 from ffmpeg, as RGBA
        let out = Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(&clip)
            .args(["-vf", "select=eq(n\\,10)", "-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "rgba", "-"])
            .output()
            .unwrap();
        assert_eq!(out.stdout.len(), 320 * 180 * 4, "one RGBA frame from ffmpeg");
        let p = Player::spawn(&clip, 320, 180, true, None, SMALL_CACHE).expect("spawn");
        p.seek(10.0 / 30.0, true);
        let deadline = Instant::now() + Duration::from_secs(10);
        let ours = loop {
            if let Some((pts, f)) = p.take_next() {
                assert!((pts - 10.0 / 30.0).abs() < 0.002, "landed on {pts}");
                break f;
            }
            assert!(Instant::now() < deadline, "no frame");
            thread::sleep(Duration::from_millis(5));
        };
        let diffs: Vec<u32> = ours.iter().zip(&out.stdout).map(|(a, b)| a.abs_diff(*b) as u32).collect();
        let mean = diffs.iter().sum::<u32>() as f64 / diffs.len() as f64;
        let max = *diffs.iter().max().unwrap();
        let over4 = diffs.iter().filter(|&&d| d > 4).count();
        eprintln!("rgba vs ffmpeg: mean {mean:.3}, max {max}, >4: {over4} of {}", diffs.len());
        assert!(mean < 1.0, "mean difference {mean:.3}");
        assert!(over4 * 1000 < diffs.len(), "{over4} samples differ by more than 4 levels");
    }
}
