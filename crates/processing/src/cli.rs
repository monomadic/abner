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
        Arc::new(|p| eprintln!("{}: {}", p.phase, p.completed)),
    );
    drop(signals);
    println!(
        "{}",
        serde_json::to_string_pretty(&result?).map_err(|e| e.to_string())?
    );
    Ok(())
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
