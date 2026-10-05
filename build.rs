use std::process::Command;

/// `git <args>` trimmed, or None when git is missing or the tree is not a repository.
fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// UTC `YYYY-MM-DD HH:MM` from the clock, without a date crate (Hinnant's civil-from-days).
fn utc_now() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02}", rem / 3600, rem % 3600 / 60)
}

/// The label the launch window prints: `0.1.0 · 4fd1bdf+dirty · 2026-10-06 14:22`.
///
/// Cargo only reruns a build script when something it was told to watch
/// changed, and uncommitted edits touch neither HEAD nor any ref — so the
/// source tree and the git index are watched too, or a dirty build would
/// carry the stamp of the last clean one.
fn stamp_build() {
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=abner.default.toml");
    for path in ["HEAD", "index"] {
        if let Some(p) = git(&["rev-parse", "--git-path", path]) {
            println!("cargo:rerun-if-changed={p}");
        }
    }
    let hash = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "nogit".into());
    let dirty = git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let label = format!("{version} · {hash}{} · {}", if dirty { "+dirty" } else { "" }, utc_now());
    println!("cargo:rustc-env=ABNER_BUILD={label}");
}

fn main() {
    stamp_build();
    // Compile the Objective-C shim. macOS only; nothing to build elsewhere.
    //
    //  - open_shim.m: the Open With application delegate (see src/open.rs).
    #[cfg(target_os = "macos")]
    {
        println!("cargo:rerun-if-changed=src/open_shim.m");
        cc::Build::new()
            .file("src/open_shim.m")
            .flag("-fobjc-arc")
            .compile("ab_shims");
        // objc2-app-kit already links AppKit, but be explicit so the shim's
        // symbols resolve regardless of link order.
        println!("cargo:rustc-link-lib=framework=AppKit");
    }
}
