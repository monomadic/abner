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
    /// Latest seek request (seconds, exact) — newest wins.
    cmd: Mutex<Option<(f64, bool)>>,
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
            if let Err(e) = unsafe { reader(&reader_shared, &cfg) } {
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
        let t = target_s.max(0.0);
        *self.shared.cmd.lock().unwrap() = Some((t, exact));
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

    #[allow(dead_code)] // used by tests; kept public API
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

unsafe fn reader(shared: &Shared, cfg: &ReaderCfg) -> Result<(), String> {
    quiet_libav_once();
    unsafe {
        let mut fmt_raw: *mut ffi::AVFormatContext = ptr::null_mut();
        if ffi::avformat_open_input(&mut fmt_raw, cfg.cpath.as_ptr(), ptr::null(), ptr::null_mut())
            < 0
        {
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
        };

        let seek_to = |target: f64| -> bool {
            // `target` is content-relative; seek in absolute stream time.
            let ts = ((target + start_off) / tb) as i64;
            let ok = ffi::avformat_seek_file(
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
            if let Some((target, exact)) = shared.cmd.lock().unwrap().take() {
                seek_to(target);
                pump.skip_until = exact.then_some(target);
                continue;
            }
            let r = ffi::av_read_frame(fmt.0, pkt.0);
            if r == ffi::AVERROR_EOF {
                // Drain the decoder's tail, then park: the app owns the
                // clock and will send a seek when it wraps.
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
            if let Some(t) = self.skip_until {
                if pts != ffi::AV_NOPTS_VALUE && pts_s < t - 1e-3 {
                    ffi::av_frame_unref(frame);
                    return Flow::Continue; // pre-target GOP frame
                }
                self.skip_until = None;
            }
            let is_hw = (*frame).format == ffi::AV_PIX_FMT_VIDEOTOOLBOX;
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
}
