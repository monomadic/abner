//! `cargo build-with-plugins [cargo build flags]`
//!
//! Cargo resolves dependencies from manifests before anything runs, so a
//! directory of crates cannot be discovered at build time. This helper does
//! the discovery instead: every `plugins/<crate>/Cargo.toml` becomes a path
//! dependency of a GENERATED launcher package under `plugins/.launcher`, whose
//! `main` registers each crate's `factories()` and calls `abner::run`. The
//! launcher is then built with a real `cargo build`, with whatever flags
//! followed the command, into the repository's `target/` directory, so the
//! abner library compiles once for both the plain and the composed build.
//!
//! Local plugin entries are gitignored, and the launcher is its own package (an empty
//! `[workspace]` table) with its own lockfile, so a run leaves the committed
//! tree, including the root `Cargo.lock`, untouched. Nothing here knows any
//! plugin by name; the convention a plugin crate meets is one function:
//!
//! ```ignore
//! pub fn factories() -> Vec<Box<dyn abner_processor_api::Factory>>
//! ```
//!
//! A plugin folder may be a symbolic link to a crate kept in its own project;
//! the launcher then depends on the link's target, so the crate's relative
//! path dependencies resolve where it really lives.
//!
//! `ABNER_PLUGINS` overrides the plugin directory.

use anyhow::{Context, Result, bail, ensure};
use std::path::{Path, PathBuf};
use std::process::Command;

const LAUNCHER_NAME: &str = "abner-with-plugins";
const SDK_GIT: &str = "https://github.com/monomadic/abner";

fn main() -> Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .context("tools/build-with-plugins must sit two levels under the repository root")?
        .to_path_buf();
    let configured = std::env::var_os("ABNER_PLUGINS").map(PathBuf::from);
    let plugins = plugin_directory(&root, &std::env::current_dir()?, configured.as_deref());
    let args: Vec<String> = std::env::args().skip(1).collect();

    let crates = discover(&plugins, configured.is_none())?;
    if crates.is_empty() {
        eprintln!(
            "build-with-plugins: no external plugin crates in {}",
            plugins.display()
        );
    } else {
        eprintln!(
            "build-with-plugins: {} plugin crate(s) in {}",
            crates.len(),
            plugins.display()
        );
        for c in &crates {
            eprintln!("  {} ({})", c.name, c.path.display());
        }
    }

    let launcher = plugins.join(".launcher");
    write_launcher(&root, &launcher, &crates)?;

    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let target_dir = root.join("target");
    let status = Command::new(&cargo)
        .current_dir(&root) // so the repository's .cargo/config.toml (PKG_CONFIG_PATH) applies
        .arg("build")
        .arg("--manifest-path")
        .arg(launcher.join("Cargo.toml"))
        .arg("--target-dir")
        .arg(&target_dir)
        .args(&args)
        .status()
        .context("failed to run cargo")?;
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }

    let bin = output_binary(&target_dir, &args);
    ensure!(
        bin.is_file(),
        "cargo succeeded but no binary at {}",
        bin.display()
    );
    eprintln!("build-with-plugins: built {}", bin.display());
    Ok(())
}

struct Plugin {
    /// The package name as written in its manifest (may contain `-`).
    name: String,
    path: PathBuf,
    uses_git_sdk: bool,
}

impl Plugin {
    fn ident(&self) -> String {
        self.name.replace('-', "_")
    }
}

/// Resolve overrides against the caller's directory before Cargo changes its
/// working directory or interprets generated manifest-relative dependency paths.
fn plugin_directory(root: &Path, cwd: &Path, configured: Option<&Path>) -> PathBuf {
    match configured {
        Some(path) => cwd.join(path),
        None => root.join("plugins"),
    }
}

/// Every immediate subdirectory of `dir` holding a `Cargo.toml`, sorted by
/// name. Hidden directories (the generated `.launcher`) are skipped.
fn discover(dir: &Path, allow_missing: bool) -> Result<Vec<Plugin>> {
    let mut found = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if allow_missing && error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(found);
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("reading plugin directory {}", dir.display()));
        }
    };
    for entry in entries {
        let path = entry?.path();
        let hidden = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_none_or(|n| n.starts_with('.'));
        if hidden || !path.is_dir() {
            continue;
        }
        // A plugin kept in its own project is
        // linked here. The launcher must name the real directory, or the
        // crate's own relative path dependencies resolve against the link.
        let path = if path.symlink_metadata()?.file_type().is_symlink() {
            std::fs::canonicalize(&path)
                .with_context(|| format!("resolving plugin link {}", path.display()))?
        } else {
            path
        };
        let manifest = path.join("Cargo.toml");
        if !manifest.is_file() {
            continue;
        }
        let text = std::fs::read_to_string(&manifest)
            .with_context(|| format!("reading {}", manifest.display()))?;
        let doc: toml::Table = text
            .parse()
            .with_context(|| format!("parsing {}", manifest.display()))?;
        let name = doc
            .get("package")
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str())
            .with_context(|| format!("{} has no [package] name", manifest.display()))?
            .to_string();
        let valid = name
            .chars()
            .enumerate()
            .all(|(i, c)| c.is_ascii_alphanumeric() || c == '_' || (c == '-' && i > 0))
            && !name.starts_with(|c: char| c.is_ascii_digit())
            && !name.is_empty();
        ensure!(
            valid,
            "{}: package name {name:?} is not a usable crate name",
            manifest.display()
        );
        ensure!(
            name != "abner" && name != "abner-processor-api",
            "{}: a plugin may not be named {name:?}",
            manifest.display()
        );
        let uses_git_sdk = doc
            .get("dependencies")
            .and_then(|deps| deps.get("abner-processor-api"))
            .and_then(|sdk| sdk.get("git"))
            .and_then(toml::Value::as_str)
            == Some(SDK_GIT);
        // Resolve provider links before Cargo interprets relative dependencies.
        let path = path
            .canonicalize()
            .with_context(|| format!("resolving plugin directory {}", path.display()))?;
        found.push(Plugin {
            name,
            path,
            uses_git_sdk,
        });
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    for pair in found.windows(2) {
        if pair[0].ident() == pair[1].ident() {
            bail!(
                "two plugins resolve to the same crate name: {} and {}",
                pair[0].path.display(),
                pair[1].path.display()
            );
        }
    }
    Ok(found)
}

/// Writes the launcher's manifest and `main.rs`, only when their text changes,
/// so an unchanged plugin set does not invalidate cargo's fingerprint.
fn write_launcher(root: &Path, launcher: &Path, crates: &[Plugin]) -> Result<()> {
    let version = std::fs::read_to_string(root.join("Cargo.toml"))?
        .parse::<toml::Table>()?
        .get("package")
        .and_then(|p| p.get("version"))
        .and_then(|v| v.as_str())
        .unwrap_or("0.0.0")
        .to_string();

    let path_dep = |p: &Path| {
        let mut t = toml::Table::new();
        t.insert(
            "path".into(),
            toml::Value::String(p.to_string_lossy().into_owned()),
        );
        toml::Value::Table(t)
    };
    let mut deps = toml::Table::new();
    deps.insert("abner".into(), path_dep(root));
    deps.insert(
        "abner-processor-api".into(),
        path_dep(&root.join("crates/processor-api")),
    );
    deps.insert("anyhow".into(), toml::Value::String("1".into()));
    for c in crates {
        deps.insert(c.name.clone(), path_dep(&c.path));
    }
    let mut package = toml::Table::new();
    package.insert("name".into(), toml::Value::String(LAUNCHER_NAME.into()));
    package.insert("version".into(), toml::Value::String(version));
    package.insert("edition".into(), toml::Value::String("2024".into()));
    package.insert("publish".into(), toml::Value::Boolean(false));
    let mut manifest = toml::Table::new();
    manifest.insert("package".into(), toml::Value::Table(package));
    // Its own workspace: never a member of a parent, so its lockfile stays here.
    manifest.insert("workspace".into(), toml::Value::Table(toml::Table::new()));
    manifest.insert("dependencies".into(), toml::Value::Table(deps));
    if crates.iter().any(|plugin| plugin.uses_git_sdk) {
        let sdk = toml::Table::from_iter([(
            "abner-processor-api".into(),
            path_dep(&root.join("crates/processor-api")),
        )]);
        let patch = toml::Table::from_iter([(SDK_GIT.into(), toml::Value::Table(sdk))]);
        manifest.insert("patch".into(), toml::Value::Table(patch));
    }
    // Profiles come from the root package only, so abner's are repeated:
    // wgpu + shader-heavy code is unusably slow at opt-level 0.
    let profiles: toml::Table = r#"
        [dev]
        opt-level = 1
        [dev.package."*"]
        opt-level = 2
    "#
    .parse()?;
    manifest.insert("profile".into(), toml::Value::Table(profiles));
    let manifest_text = format!(
        "# Generated by `cargo build-with-plugins`; edits are overwritten.\n{}",
        toml::to_string(&manifest)?
    );

    let binding = if crates.is_empty() {
        "registry"
    } else {
        "mut registry"
    };
    let mut main = format!(
        "//! Generated by `cargo build-with-plugins`; edits are overwritten.\n\
         fn main() -> anyhow::Result<()> {{\n    \
         let {binding} = abner_processor_api::Registry::default();\n",
    );
    for c in crates {
        main.push_str(&format!(
            "    for factory in {}::factories() {{\n        \
             registry.register(factory).map_err(anyhow::Error::msg)?;\n    }}\n",
            c.ident()
        ));
    }
    main.push_str("    abner::run(registry)\n}\n");

    std::fs::create_dir_all(launcher.join("src"))?;
    write_if_changed(&launcher.join("Cargo.toml"), &manifest_text)?;
    write_if_changed(&launcher.join("src/main.rs"), &main)?;
    Ok(())
}

fn write_if_changed(path: &Path, text: &str) -> Result<()> {
    if std::fs::read_to_string(path).is_ok_and(|old| old == text) {
        return Ok(());
    }
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
}

/// Where `cargo build` put the launcher, from the flags that pick the profile
/// and target directory layout.
fn output_binary(target_dir: &Path, args: &[String]) -> PathBuf {
    let mut profile = "debug".to_string();
    let mut triple = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--release" | "-r" => profile = "release".into(),
            "--profile" => {
                if let Some(p) = it.next() {
                    profile = if p == "dev" || p == "test" {
                        "debug".into()
                    } else if p == "bench" {
                        "release".into()
                    } else {
                        p.clone()
                    };
                }
            }
            "--target" => triple = it.next().cloned(),
            _ => {
                if let Some(p) = a.strip_prefix("--profile=") {
                    profile = if p == "dev" || p == "test" {
                        "debug".into()
                    } else if p == "bench" {
                        "release".into()
                    } else {
                        p.into()
                    };
                } else if let Some(t) = a.strip_prefix("--target=") {
                    triple = Some(t.into());
                }
            }
        }
    }
    let mut dir = target_dir.to_path_buf();
    if let Some(t) = triple {
        dir.push(t);
    }
    dir.join(profile).join(LAUNCHER_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "abner-composition-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn relative_override_emits_absolute_dependency_and_launcher_paths() {
        let f = Fixture::new();
        let root = f.0.join("host");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"example-host\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let plugins = plugin_directory(&root, &f.0, Some(Path::new("local-plugins")));
        let crate_dir = plugins.join("example");
        std::fs::create_dir_all(&crate_dir).unwrap();
        std::fs::write(
            crate_dir.join("Cargo.toml"),
            "[package]\nname = \"example-effect\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let crates = discover(&plugins, false).unwrap();
        let launcher = plugins.join(".launcher");
        write_launcher(&root, &launcher, &crates).unwrap();
        assert!(launcher.is_absolute());
        let manifest: toml::Table = std::fs::read_to_string(launcher.join("Cargo.toml"))
            .unwrap()
            .parse()
            .unwrap();
        let emitted = Path::new(
            manifest["dependencies"]["example-effect"]["path"]
                .as_str()
                .unwrap(),
        );
        assert!(emitted.is_absolute());
        assert_eq!(launcher.join(emitted), crate_dir.canonicalize().unwrap());
        assert!(launcher.join(emitted).join("Cargo.toml").is_file());
        assert!(!manifest.contains_key("patch"));
    }
    #[cfg(unix)]
    #[test]
    fn linked_provider_uses_its_own_directory_and_the_host_sdk() {
        let f = Fixture::new();
        let provider = f.0.join("provider");
        let plugins = f.0.join("plugins");
        std::fs::create_dir(&provider).unwrap();
        std::fs::create_dir(&plugins).unwrap();
        std::fs::write(f.0.join("Cargo.toml"), "[package]\nversion = \"0.1.0\"\n").unwrap();
        std::fs::write(
            provider.join("Cargo.toml"),
            format!(
                "[package]\nname = \"example-effect\"\n[dependencies]\n\
                 abner-processor-api = {{ git = \"{SDK_GIT}\", rev = \"example\" }}\n"
            ),
        )
        .unwrap();
        std::os::unix::fs::symlink(&provider, plugins.join("linked")).unwrap();

        let crates = discover(&plugins, false).unwrap();
        assert_eq!(crates[0].path, provider.canonicalize().unwrap());
        let launcher = plugins.join(".launcher");
        write_launcher(&f.0, &launcher, &crates).unwrap();
        let manifest: toml::Table = std::fs::read_to_string(launcher.join("Cargo.toml"))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(
            manifest["patch"][SDK_GIT]["abner-processor-api"],
            manifest["dependencies"]["abner-processor-api"]
        );
    }
    #[test]
    fn a_linked_plugin_is_emitted_at_its_real_directory() {
        let f = Fixture::new();
        let real = f.0.join("elsewhere").join("example");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(
            real.join("Cargo.toml"),
            "[package]\nname = \"example-effect\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let plugins = f.0.join("plugins");
        std::fs::create_dir(&plugins).unwrap();
        std::os::unix::fs::symlink(Path::new("../elsewhere/example"), plugins.join("example"))
            .unwrap();
        let crates = discover(&plugins, false).unwrap();
        assert_eq!(crates.len(), 1);
        assert_eq!(crates[0].path, std::fs::canonicalize(&real).unwrap());
        assert!(
            !crates[0].path.starts_with(&plugins),
            "the link itself must not be the dependency path"
        );
    }
    #[test]
    fn absolute_override_is_preserved() {
        let f = Fixture::new();
        assert_eq!(
            plugin_directory(Path::new("/unused"), Path::new("/different"), Some(&f.0)),
            f.0
        );
    }
    #[test]
    fn only_missing_default_directory_is_optional() {
        let f = Fixture::new();
        let dir = plugin_directory(&f.0, Path::new("/unused"), None);
        assert_eq!(dir, f.0.join("plugins"));
        assert!(discover(&dir, true).unwrap().is_empty());
        let error = discover(&dir, false)
            .err()
            .expect("explicit missing directory must fail");
        assert!(error.to_string().contains(&dir.display().to_string()));
    }
    #[test]
    fn invalid_directory_errors_are_never_treated_as_empty() {
        let f = Fixture::new();
        let file = f.0.join("not-a-directory");
        std::fs::write(&file, "fixture").unwrap();
        for allow_missing in [false, true] {
            let error = discover(&file, allow_missing)
                .err()
                .expect("file is not a plugin directory");
            assert!(error.to_string().contains(&file.display().to_string()));
        }
    }
    #[test]
    fn existing_empty_directory_is_valid() {
        let f = Fixture::new();
        assert!(discover(&f.0, false).unwrap().is_empty());
    }
}
