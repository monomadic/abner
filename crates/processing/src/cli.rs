//! Reusable frontend. A launcher supplies its registry; no plugin is named here.
use crate::{
    Stage,
    media::{self, Job},
};
use abner_processor_api::{Cancellation, Registry, Result};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

const HELP: &str = "processor-cli list | describe ID | run [OPTIONS]
  --input FILE         SDR Rec.709 CFR MP4/MOV
  --output NEW.mov     ProRes HQ, high-precision RGB input; no overwrite
  --processor ID       one processor; --settings FILE supplies its JSON settings
  --chain FILE         JSON array of {processor, settings}, instead of --processor
  --no-audio           omit audio; otherwise copy packets and verify preservation
  --memory-mib N       returned-frame batch budget (default 512; excludes models)
Unsupported formats are rejected, never silently converted or dropped.";

pub fn run(registry: &Registry) -> Result<()> {
    run_args(registry, std::env::args().skip(1))
}
/// CLI signal handling belongs here; the library's media runner never installs it.
pub fn run_args(registry: &Registry, args: impl IntoIterator<Item = String>) -> Result<()> {
    let mut args = args.into_iter();
    let command = args.next().unwrap_or_else(|| "--help".into());
    match command.as_str() {
        "--help" | "-h" => {
            println!("{HELP}");
            return Ok(());
        }
        "list" => {
            if args.next().is_some() {
                return Err("list takes no arguments".into());
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&registry.descriptors()).map_err(|e| e.to_string())?
            );
            return Ok(());
        }
        "describe" => {
            let id = args.next().ok_or("describe needs a processor ID")?;
            if args.next().is_some() {
                return Err("describe takes one ID".into());
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&registry.get(&id)?.descriptor())
                    .map_err(|e| e.to_string())?
            );
            return Ok(());
        }
        "run" => {}
        _ => return Err(HELP.into()),
    }
    let mut opts = BTreeMap::new();
    let mut audio = true;
    while let Some(k) = args.next() {
        if k == "--no-audio" {
            audio = false;
            continue;
        }
        if ![
            "--input",
            "--output",
            "--processor",
            "--settings",
            "--chain",
            "--memory-mib",
        ]
        .contains(&k.as_str())
        {
            return Err(format!("unknown option {k}"));
        }
        let value = args
            .next()
            .ok_or_else(|| format!("missing value for {k}"))?;
        if opts.insert(k.clone(), value).is_some() {
            return Err(format!("duplicate {k}"));
        }
    }
    let read = |path: &str| -> Result<serde_json::Value> {
        serde_json::from_slice(&std::fs::read(path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())
    };
    let stages = if let Some(path) = opts.get("--chain") {
        if opts.contains_key("--processor") || opts.contains_key("--settings") {
            return Err("--chain cannot be combined with --processor/--settings".into());
        }
        serde_json::from_value::<Vec<Stage>>(read(path)?).map_err(|e| e.to_string())?
    } else {
        vec![Stage {
            processor: opts
                .get("--processor")
                .ok_or("--processor or --chain is required")?
                .clone(),
            settings: opts
                .get("--settings")
                .map(|p| read(p))
                .transpose()?
                .unwrap_or_else(|| serde_json::json!({})),
        }]
    };
    let memory: usize = opts
        .get("--memory-mib")
        .map(|s| s.parse::<usize>().map_err(|e| e.to_string()))
        .transpose()?
        .unwrap_or(512);
    let job = Job {
        input: PathBuf::from(opts.get("--input").ok_or("--input is required")?),
        output: PathBuf::from(opts.get("--output").ok_or("--output is required")?),
        stages,
        copy_audio: audio,
        memory_bytes: memory
            .checked_mul(1024 * 1024)
            .ok_or("memory budget overflow")?,
    };
    let cancellation = Cancellation::default();
    let signals = Signals::new(cancellation.clone());
    let result = media::run(
        &job,
        registry,
        &cancellation,
        Arc::new({
            let started = std::time::Instant::now();
            move |p| eprintln!("{}", progress_line(&p, started.elapsed().as_secs_f64()))
        }),
    );
    drop(signals);
    println!(
        "{}",
        serde_json::to_string_pretty(&result?).map_err(|e| e.to_string())?
    );
    Ok(())
}
/// `90` → `1m 30s`; under a minute, whole seconds.
fn remaining(seconds: f64) -> String {
    let s = seconds.ceil().max(0.) as u64;
    if s >= 3600 {
        format!("{}h {:02}m", s / 3600, s / 60 % 60)
    } else if s >= 60 {
        format!("{}m {:02}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}
/// One stderr line per report. A stage's report shows its own fraction; the
/// job-level one adds the whole clip's percentage and the time left at the
/// pace so far, `elapsed` seconds into the job.
fn progress_line(p: &abner_processor_api::Progress, elapsed: f64) -> String {
    let Some(total) = p.total.filter(|t| *t > 0) else {
        return format!("{}: {}", p.phase, p.completed);
    };
    let done = p.completed.min(total);
    let mut line = format!("{}: {done}/{total}", p.phase);
    if p.phase == media::JOB_PHASE {
        line.push_str(&format!(" · {}%", done * 100 / total));
        if done > 0 && done < total {
            let left = elapsed / done as f64 * (total - done) as f64;
            line.push_str(&format!(" · ~{} left", remaining(left)));
        }
    }
    line
}
static SIGNALLED: AtomicBool = AtomicBool::new(false);
#[cfg(unix)]
extern "C" fn signal(_: libc::c_int) {
    SIGNALLED.store(true, Ordering::Relaxed);
}
struct Signals {
    done: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    #[cfg(unix)]
    old: [libc::sighandler_t; 2],
}
impl Signals {
    fn new(cancel: Cancellation) -> Self {
        SIGNALLED.store(false, Ordering::Relaxed);
        #[cfg(unix)]
        let old = unsafe {
            [
                libc::signal(libc::SIGINT, signal as *const () as libc::sighandler_t),
                libc::signal(libc::SIGTERM, signal as *const () as libc::sighandler_t),
            ]
        };
        let done = Arc::new(AtomicBool::new(false));
        let finished = done.clone();
        let thread = std::thread::spawn(move || {
            while !finished.load(Ordering::Relaxed) {
                if SIGNALLED.load(Ordering::Relaxed) {
                    cancel.cancel();
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        });
        Self {
            done,
            thread: Some(thread),
            #[cfg(unix)]
            old,
        }
    }
}
impl Drop for Signals {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        #[cfg(unix)]
        unsafe {
            libc::signal(libc::SIGINT, self.old[0]);
            libc::signal(libc::SIGTERM, self.old[1]);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use abner_processor_api::Progress;
    #[test]
    fn job_report_adds_whole_clip_percentage_and_time_left() {
        let line = |phase: &str, completed, total, elapsed| {
            progress_line(
                &Progress {
                    phase: phase.into(),
                    completed,
                    total,
                },
                elapsed,
            )
        };
        assert_eq!(line("Tiles", 3, None, 9.), "Tiles: 3");
        assert_eq!(line("Tiles", 3, Some(0), 9.), "Tiles: 3");
        // A stage's fraction is about its own work, so it gets no clip estimate.
        assert_eq!(line("Tiles", 3, Some(12), 9.), "Tiles: 3/12");
        assert_eq!(
            line(media::JOB_PHASE, 30, Some(120), 600.),
            "Encoded frames: 30/120 · 25% · ~30m 00s left"
        );
        assert_eq!(
            line(media::JOB_PHASE, 119, Some(120), 119.),
            "Encoded frames: 119/120 · 99% · ~1s left"
        );
        // Nothing to extrapolate from before the first frame or after the last.
        assert_eq!(line(media::JOB_PHASE, 0, Some(120), 50.), "Encoded frames: 0/120 · 0%");
        assert_eq!(line(media::JOB_PHASE, 130, Some(120), 50.), "Encoded frames: 120/120 · 100%");
        assert_eq!(remaining(7300.), "2h 01m");
    }
}
