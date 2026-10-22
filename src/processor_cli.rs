//! Generic composition probes, before the GUI or media I/O is initialized.
use abner_processor_api::{Frame, Registry};
pub fn handle(registry: &Registry) -> anyhow::Result<bool> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--list-processors") if args.len() == 1 => {
            let listed: Vec<_> = registry
                .descriptors()
                .into_iter()
                .map(|d| {
                    let f = registry.get(&d.id).map_err(anyhow::Error::msg)?;
                    let mut v = serde_json::to_value(&d)?;
                    v["categories"] = serde_json::to_value(f.categories())?;
                    v["capabilities"] = serde_json::to_value(f.capabilities())?;
                    Ok(v)
                })
                .collect::<anyhow::Result<_>>()?;
            println!("{}", serde_json::to_string_pretty(&listed)?);
        }
        Some("--check-processor") => {
            anyhow::ensure!(args.len() == 3, "usage: --check-processor ID SETTINGS_JSON");
            let settings = serde_json::from_str(&args[2])?;
            let factory = registry.get(&args[1]).map_err(anyhow::Error::msg)?;
            let mut session = factory
                .create(64, 64, &settings)
                .map_err(anyhow::Error::msg)?;
            let mut outputs = Vec::new();
            for i in 0..4 {
                let input = Frame {
                    width: 64,
                    height: 64,
                    pixels: (0..64 * 64 * 3)
                        .map(|n| ((n * 7919 + i * 31) % 65536) as u16)
                        .collect(),
                    pts: i as i64,
                    duration: 1,
                    time_base: [1, 24],
                };
                outputs.extend(session.process(input).map_err(anyhow::Error::msg)?);
            }
            outputs.extend(session.finish().map_err(anyhow::Error::msg)?);
            anyhow::ensure!(
                outputs.len() == 4,
                "processor changed frame count in this one-to-one probe"
            );
            let mut frames = Vec::new();
            for (i, out) in outputs.into_iter().enumerate() {
                out.validate().map_err(anyhow::Error::msg)?;
                anyhow::ensure!(
                    out.pts == i as i64 && out.duration == 1 && out.time_base == [1, 24],
                    "processor changed timing"
                );
                let checksum = out
                    .pixels
                    .iter()
                    .fold(0u64, |v, &p| v.wrapping_mul(31).wrapping_add(u64::from(p)));
                frames.push(serde_json::json!({"width":out.width,"height":out.height,"pts":out.pts,"checksum":checksum,"progress":session.progress()}));
            }
            println!("{}", serde_json::to_string_pretty(&frames)?);
        }
        Some("--list-processors") => anyhow::bail!("--list-processors takes no arguments"),
        _ => return Ok(false),
    }
    Ok(true)
}
