//! Human-readable, per-processor settings. Built-in presets never live in this store.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Document {
    version: u32,
    pub processor: String,
    pub name: String,
    pub settings: Value,
}
pub(crate) fn directory() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(|h| PathBuf::from(h).join(".config/abner/presets"))
}
// Encode separators and '%' rather than collapsing different IDs/names into one filename.
fn filename(text: &str) -> String {
    text.as_bytes()
        .iter()
        .map(|&b| {
            if b.is_ascii_alphanumeric() || b"-_. ".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}
fn folder(root: &Path, processor: &str) -> PathBuf {
    root.join(format!("processor-{}", filename(processor)))
}
fn read(path: &Path, processor: &str) -> Result<Document, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let d: Document = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    if d.version != 1
        || d.processor != processor
        || d.name.trim().is_empty()
        || !d.settings.is_object()
    {
        return Err("Invalid preset version, processor, name or settings".into());
    }
    Ok(d)
}
pub(crate) fn load(root: &Path, processor: &str) -> (Vec<(PathBuf, Document)>, Vec<String>) {
    let entries = match std::fs::read_dir(folder(root, processor)) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (vec![], vec![]),
        Err(e) => return (vec![], vec![e.to_string()]),
    };
    let mut loaded = vec![];
    let mut errors = vec![];
    for entry in entries {
        match entry {
            Ok(e) if e.path().extension().is_some_and(|x| x == "json") => {
                match read(&e.path(), processor) {
                    Ok(d) => loaded.push((e.path(), d)),
                    Err(error) => errors.push(format!("{}: {error}", e.path().display())),
                }
            }
            Ok(_) => {}
            Err(e) => errors.push(e.to_string()),
        }
    }
    loaded.sort_by(|a, b| a.1.name.cmp(&b.1.name).then_with(|| a.0.cmp(&b.0)));
    (loaded, errors)
}
static NEXT: AtomicU64 = AtomicU64::new(0);
pub(crate) fn save(
    root: &Path,
    processor: &str,
    name: &str,
    settings: &Value,
) -> Result<PathBuf, String> {
    let name = name.trim();
    if name.is_empty() || name.len() > 160 || name.chars().any(char::is_control) {
        return Err("Enter a preset name of 1–160 bytes without control characters".into());
    }
    if !settings.is_object() {
        return Err("Preset settings must be an object".into());
    }
    let folder = folder(root, processor);
    std::fs::create_dir_all(&folder).map_err(|e| e.to_string())?;
    let path = folder.join(format!("{}.json", filename(name)));
    if path.exists() {
        let existing = read(&path, processor)?;
        if existing.name != name {
            return Err("Another preset uses this filename".into());
        }
    }
    let d = Document {
        version: 1,
        processor: processor.into(),
        name: name.into(),
        settings: settings.clone(),
    };
    let mut bytes = serde_json::to_vec_pretty(&d).map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    let temp = folder.join(format!(
        ".{}.{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&temp, &path)
    })()
    .map_err(|e: std::io::Error| e.to_string());
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result?;
    Ok(path)
}
pub(crate) fn delete(root: &Path, processor: &str, path: &Path) -> Result<(), String> {
    if path.parent() != Some(folder(root, processor).as_path()) {
        return Err("Not a custom preset for this processor".into());
    }
    read(path, processor)?;
    std::fs::remove_file(path).map_err(|e| e.to_string())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn readable_round_trip_update_delete_and_bad_file_isolation() {
        let root = std::env::temp_dir().join(format!(
            "abner-presets-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let settings = serde_json::json!({"strength":0.75,"automatic":null,"enabled":true,"mode":"high","nested":{"steps":[1,2]}});
        let path = save(&root, "test/processor", "My / preset", &settings).unwrap();
        assert!(path.starts_with(&root));
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("\n  \"settings\": {")
        );
        let (loaded, errors) = load(&root, "test/processor");
        assert!(errors.is_empty());
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].1.settings, settings);
        assert!(load(&root, "test-other").0.is_empty());
        save(
            &root,
            "test/processor",
            "My / preset",
            &serde_json::json!({"strength":1}),
        )
        .unwrap();
        assert_eq!(load(&root, "test/processor").0.len(), 1);
        std::fs::write(path.parent().unwrap().join("broken.json"), "{bad").unwrap();
        let (loaded, errors) = load(&root, "test/processor");
        assert_eq!(loaded.len(), 1);
        assert_eq!(errors.len(), 1);
        assert!(delete(&root, "other", &path).is_err());
        delete(&root, "test/processor", &path).unwrap();
        assert!(load(&root, "test/processor").0.is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }
}
