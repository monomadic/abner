//! In-process libav decode, adapted from switchblade's `SeekablePlayer`
//! (the settled, soaked design: resident demuxer + VideoToolbox session per
//! stream, bounded backpressure queue, seek = demuxer jump + decoder flush,
//! content-relative time, drop-wakes-the-parked-reader).
//!
//! One deliberate difference: abner plays N streams in lockstep, so the
//! per-player wall-clock pacing (due-stamped frames) is gone. The app owns
//! one master clock `t`; each player just queues `(pts, rgba)` frames and
//! the app pops everything `pts <= t` each frame (`take_upto`, newest
//! wins). That makes A/B sync exact by construction — no per-stream anchors
//! to drift apart — and pause/framestep trivial (stop advancing `t`;
//! backpressure stalls every decoder within a few frames for free).
//!
//! Decode is at NATIVE resolution: this is a comparison tool, pixels are
//! the product. Hardware-decoded frames download via
//! `av_hwframe_transfer_data` and convert to RGBA in software (the plain
//! `-hwaccel` parity path from switchblade); there is no scale chain.

use rsmpeg::ffi;
use std::collections::VecDeque;
use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

pub type Notify = Arc<dyn Fn() + Send + Sync>;

/// Read-ahead depth: enough to ride out a slow frame, small enough that a
/// paused (undrained) player stalls its decoder almost immediately.
pub const QUEUE_DEPTH: usize = 4;

/// Recycled buffers kept per player: queue depth + one being copied + one
/// at the display. A 4K RGBA frame is ~33MB — allocator churn per frame
/// taxed both threads in switchblade until the pool (its P1.5).
const POOL_CAP: usize = QUEUE_DEPTH + 2;

pub struct Player {
    shared: Arc<Shared>,
    pub w: u32,
    pub h: u32,
}

struct Shared {
    /// (pts seconds, rgba) — bounded decode queue, pts ascending.
    frames: Mutex<VecDeque<(f64, Vec<u8>)>>,
    /// Signalled when the consumer pops (or a seek/drop needs the reader).
    space: Condvar,
    /// Raised on drop: a stalled reader parks on `space` with a full queue
    /// and nothing else can reach it there — without this wake the thread
    /// leaks, pinning its frame buffers.
    closed: AtomicBool,
    /// Latest seek request (seconds, exact, forward-biased) — newest wins.
    cmd: Mutex<Option<(f64, bool, bool)>>,
    /// f64 bits: pts of the frame most recently taken (on screen), or the
    /// seek target while one is in flight.
    position: AtomicU64,
    failed: AtomicBool,
    /// Fired when a frame lands in a previously empty queue — wakes an
    /// idle render loop (paused app waiting on a framestep's frame).
    notify: Mutex<Option<Notify>>,
    /// Recycled frame buffers. Lock order: may be taken while `frames` is
    /// held, never the reverse.
    pool: Mutex<Vec<Vec<u8>>>,
}

impl Shared {
    fn take_buf(&self, len: usize) -> Vec<u8> {
        let mut buf = self.pool.lock().unwrap().pop().unwrap_or_default();
        buf.resize(len, 0);
        buf
    }

    fn recycle_buf(&self, buf: Vec<u8>) {
        let mut pool = self.pool.lock().unwrap();
        if pool.len() < POOL_CAP {
            pool.push(buf);
        }
    }
}

impl Player {
    /// Spawn a reader decoding `path` to `w × h` RGBA (the display dims —
    /// rotation applied, so pass probe's display size). Returns
    /// immediately; open/decode errors surface as a stream that never
    /// produces frames (`failed()`).
    pub fn spawn(path: &Path, w: u32, h: u32, use_vt: bool, rotation: Option<f64>) -> Option<Self> {
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
        let shared = Arc::new(Shared {
            frames: Mutex::new(VecDeque::new()),
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
            if let Err(e) = unsafe { reader(&reader_shared, &cfg) }
                && !reader_shared.closed.load(Ordering::Relaxed)
            {
                log::warn!("player: {} — {e}", cfg.path.display());
                reader_shared.failed.store(true, Ordering::Relaxed);
            }
        });
        Some(Self { shared, w, h })
    }

    /// Jump playback. `exact` decodes forward from the preceding keyframe
    /// and discards frames before the target (GOP-bound); otherwise the
    /// landing keyframe is delivered immediately. Queued frames are stale
    /// the moment this is called — the last shown frame stays on screen
    /// until the new position delivers.
    pub fn seek(&self, target_s: f64, exact: bool) {
        self.seek_biased(target_s, exact, false);
    }

    /// `seek` for a gesture that has a DIRECTION (switchblade's
    /// `SeekBias`): `forward` lands a keyframe seek on the first keyframe
    /// at or AFTER the target, so a click ahead of the picture never moves
    /// it backward (a keyframe-before landing undershoots by up to a GOP).
    /// Falls back to the keyframe before when none follows — near the
    /// tail that is a smaller wrong than seeking past EOF.
    pub fn seek_biased(&self, target_s: f64, exact: bool, forward: bool) {
        let t = target_s.max(0.0);
        *self.shared.cmd.lock().unwrap() = Some((t, exact, forward));
        for (_, buf) in self.shared.frames.lock().unwrap().drain(..) {
            self.shared.recycle_buf(buf);
        }
        self.shared.position.store(t.to_bits(), Ordering::Relaxed);
        self.shared.space.notify_all();
    }

    /// Pop every queued frame with `pts <= t`, returning the newest —
    /// master-clock pacing (frames the clock ran past are dropped, so a
    /// slow decoder degrades to a lower frame rate, never to lag).
    pub fn take_upto(&self, t: f64) -> Option<(f64, Vec<u8>)> {
        let mut q = self.shared.frames.lock().unwrap();
        let mut out: Option<(f64, Vec<u8>)> = None;
        while q.front().is_some_and(|(pts, _)| *pts <= t) {
            let (pts, buf) = q.pop_front().unwrap();
            self.shared.position.store(pts.to_bits(), Ordering::Relaxed);
            if let Some((_, prev)) = out.replace((pts, buf)) {
                self.shared.recycle_buf(prev);
            }
        }
        if out.is_some() {
            self.shared.space.notify_one();
        }
        out
    }

    /// Pop the next queued frame regardless of the clock — used right
    /// after an exact seek while paused, when the app adopts the delivered
    /// frame's true pts as the master time (float-safe framestepping).
    pub fn take_next(&self) -> Option<(f64, Vec<u8>)> {
        let r = self.shared.frames.lock().unwrap().pop_front();
        if let Some((pts, _)) = &r {
            self.shared.position.store(pts.to_bits(), Ordering::Relaxed);
            self.shared.space.notify_one();
        }
        r
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

    /// Frames decoded and waiting. After `seek` this is non-zero only once
    /// the new position has delivered (the seek drains the queue and the
    /// reader refuses stale pushes under the same lock), so `buffered() > 0`
    /// means the seek landed even when the clock won't pop the frame yet.
    pub fn buffered(&self) -> usize {
        self.shared.frames.lock().unwrap().len()
    }

    /// Install the wake fired when a frame lands in an empty queue.
    pub fn set_notify(&self, f: Notify) {
        *self.shared.notify.lock().unwrap() = Some(f);
    }

    /// Hand a presented frame's buffer back for reuse.
    pub fn recycle(&self, buf: Vec<u8>) {
        self.shared.recycle_buf(buf);
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.shared.closed.store(true, Ordering::Relaxed);
        // Serialize with the reader's park loop before notifying: the
        // reader checks `closed` under the `frames` lock and then waits on
        // `space`. A store+notify that lands between its check and its
        // wait is a LOST wakeup — nothing ever notifies again and the
        // reader parks forever, pinning its buffers (switchblade's
        // documented leak). Holding the lock for an instant forces the
        // reader to be either before its check (sees `closed`) or already
        // parked (gets the notify). `seek()` gets this ordering for free
        // via its drain.
        drop(self.shared.frames.lock());
        self.shared.space.notify_all();
    }
}

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
    /// Exact-seek refinement: drop decoded frames until this pts.
    skip_until: Option<f64>,
    /// Absolute stream pts of the last frame the decoder produced
    /// (shown or discarded), `i64::MIN` right after a seek. An exact seek
    /// forward of this that stays inside the current GOP just moves
    /// `skip_until` instead of re-seeking the demuxer and flushing the
    /// decoder — a scrub inside one GOP otherwise restarts the whole GOP
    /// decode on every pointer event and never lands.
    last_pts: i64,
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
        if cfg.use_vt && (*(*stream).codecpar).codec_id == ffi::AV_CODEC_ID_AV1 {
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
            return Err("decoder open failed".into());
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
            skip_until: None,
            last_pts: i64::MIN,
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
                // A seek that fails (transient SMB fault, stale handle)
                // must not be silent: the reader would carry on from
                // wherever it was while the other streams land on the
                // target, and A/B on the same frame — the product — would
                // be quietly false. Failing the player puts it in the HUD.
                //
                // An exact target AHEAD of the last decoded frame, in the
                // same GOP, needs no seek at all: decoding on from here
                // reaches it with every reference intact, so only the
                // discard threshold moves. Forward decode is always
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
                    pump.skip_until = Some(target);
                    continue;
                }
                if !seek_to(target, forward) {
                    return Err(format!("seek to {target:.3}s failed"));
                }
                at_eof = false;
                pump.last_pts = i64::MIN;
                pump.skip_until = exact.then_some(target);
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
                    return Err("decode error".into());
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
            if let Some(t) = self.skip_until {
                if pts != ffi::AV_NOPTS_VALUE && pts_s < t - 1e-3 {
                    ffi::av_frame_unref(frame);
                    return Flow::Continue; // pre-target GOP frame
                }
                self.skip_until = None;
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

    /// Copy the filtered rgba frame out and queue it.
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
            // Bounded read-ahead: park until the consumer makes room — or
            // the player is dropped, or a seek makes this frame stale.
            // The wait runs BEFORE the RGBA copy so a paused player never
            // retains an extra pre-copied frame.
            {
                let mut q = self.shared.frames.lock().unwrap();
                loop {
                    if self.shared.closed.load(Ordering::Relaxed) {
                        return Flow::Stop;
                    }
                    if self.shared.cmd.lock().unwrap().is_some() {
                        return Flow::Continue; // stale: the seek owns what's next
                    }
                    if q.len() < QUEUE_DEPTH {
                        break;
                    }
                    q = self.shared.space.wait(q).unwrap();
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
            let mut q = self.shared.frames.lock().unwrap();
            if self.shared.closed.load(Ordering::Relaxed) {
                return Flow::Stop;
            }
            if self.shared.cmd.lock().unwrap().is_some() {
                return Flow::Continue; // seek landed during the copy: stale
            }
            let was_dry = q.is_empty();
            q.push_back((pts_s, buf));
            drop(q);
            if was_dry && let Some(f) = &*self.shared.notify.lock().unwrap() {
                f();
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
    fn master_clock_take_upto_pops_in_pts_order() {
        let Some(clip) = test_clip("clock.mp4", 4) else { return };
        let p = Player::spawn(&clip, 320, 180, true, None).expect("spawn");
        assert!(wait_buffered(&p, 1, Duration::from_secs(5)), "no frames");
        // Clock before the first frame: nothing pops.
        assert!(p.take_upto(-1.0).is_none());
        // Clock way ahead: everything queued pops, newest wins.
        let (pts, buf) = p.take_upto(100.0).expect("frame");
        assert!(pts >= 0.0);
        assert_eq!(buf.len(), 320 * 180 * 4);
        p.recycle(buf);
        // The queue stays bounded while unwatched (backpressure).
        assert!(wait_buffered(&p, QUEUE_DEPTH, Duration::from_secs(5)));
        thread::sleep(Duration::from_millis(50));
        assert!(p.buffered() <= QUEUE_DEPTH);
        assert!(!p.failed());
    }

    /// An exact seek forward inside the GOP being decoded continues from
    /// where the reader is instead of restarting at the keyframe, and a
    /// seek backward inside it still restarts; both land on the target.
    #[test]
    fn exact_seeks_inside_one_gop_land_forward_and_back() {
        let Some(clip) = test_clip("gop.mp4", 4) else { return };
        let p = Player::spawn(&clip, 320, 180, true, None).expect("spawn");
        // GOP is 30 frames at 30 fps: 1.0..2.0 is one GOP.
        p.seek(1.1, true);
        assert!(wait_buffered(&p, 1, Duration::from_secs(5)), "no frame after seek");
        let (pts, buf) = p.take_next().unwrap();
        assert!((1.08..=1.14).contains(&pts), "landed {pts}");
        p.recycle(buf);
        p.seek(1.7, true); // forward, same GOP: extended, not re-seeked
        assert!(wait_buffered(&p, 1, Duration::from_secs(5)), "no frame after forward seek");
        let (pts, buf) = p.take_next().unwrap();
        assert!((1.68..=1.74).contains(&pts), "forward landed {pts}");
        p.recycle(buf);
        p.seek(1.3, true); // backward, same GOP: must restart from the keyframe
        assert!(wait_buffered(&p, 1, Duration::from_secs(5)), "no frame after backward seek");
        let (pts, buf) = p.take_next().unwrap();
        assert!((1.28..=1.34).contains(&pts), "backward landed {pts}");
        p.recycle(buf);
        p.seek(3.5, true); // forward across GOPs: a real seek
        assert!(wait_buffered(&p, 1, Duration::from_secs(5)), "no frame after far seek");
        let (pts, buf) = p.take_next().unwrap();
        assert!((3.48..=3.54).contains(&pts), "far landed {pts}");
        p.recycle(buf);
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
            let p = Player::spawn(&clip, 320, 180, crate::probe::vt_accel(codec), None).expect("spawn");
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
        let p = Player::spawn(&clip, 320, 180, true, None).expect("spawn");
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
        let pa = Player::spawn(&clip, 320, 180, true, None).expect("spawn a");
        let pb = Player::spawn(&clip, 320, 180, true, None).expect("spawn b");
        assert!(wait_buffered(&pa, 1, Duration::from_secs(5)));
        assert!(wait_buffered(&pb, 1, Duration::from_secs(5)));
        let (mut sa, mut sb) = (f64::NAN, f64::NAN);
        let mut t = 0.0;
        let mut worst: f64 = 0.0;
        while t < 3.0 {
            t += 0.010;
            if let Some((pts, buf)) = pa.take_upto(t) {
                sa = pts;
                pa.recycle(buf);
            }
            if let Some((pts, buf)) = pb.take_upto(t) {
                sb = pts;
                pb.recycle(buf);
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
        let p = Player::spawn(&clip, 320, 180, true, None).expect("spawn");
        assert!(wait_buffered(&p, QUEUE_DEPTH, Duration::from_secs(10)), "queue never filled");
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
        let p = Player::spawn(&fifo, 320, 180, false, None).expect("spawn");
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
