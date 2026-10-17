//! Experimental in-process CPU frame contract. No implementation dependencies.
use serde::Serialize;
use std::collections::BTreeMap;
pub type Result<T> = std::result::Result<T, String>;
#[derive(Clone, Serialize)]
pub struct Descriptor {
    pub id: String,
    pub name: String,
    pub settings_schema: serde_json::Value,
}
/// Packed full-range RGB16, SDR Rec.709; no alpha. Other formats must be rejected
/// or explicitly converted by the host before using this prototype interface.
pub struct Frame {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u16>,
    pub pts: i64,
    pub duration: i64,
    pub time_base: [u32; 2],
}
impl Frame {
    pub fn validate(&self) -> Result<()> {
        if self.width == 0
            || self.height == 0
            || self.width > 8192
            || self.height > 8192
            || self
                .width
                .checked_mul(self.height)
                .and_then(|n| n.checked_mul(3))
                != Some(self.pixels.len())
            || self.duration <= 0
            || self.time_base.contains(&0)
        {
            return Err("invalid RGB16 frame geometry or timing".into());
        }
        Ok(())
    }
}
#[derive(Clone, Serialize)]
pub struct Progress {
    pub phase: String,
    pub completed: u64,
    pub total: Option<u64>,
}
/// One input/one output prototype only. No interpolation, lookahead, GPU handles,
/// masks or asynchronous previews are promised by API 0.1.
pub trait Session {
    fn process(&mut self, input: Frame) -> Result<Frame>;
    fn progress(&self) -> Progress;
}
pub trait Factory {
    fn descriptor(&self) -> Descriptor;
    fn create(
        &self,
        width: usize,
        height: usize,
        settings: &serde_json::Value,
    ) -> Result<Box<dyn Session>>;
}
#[derive(Default)]
pub struct Registry(BTreeMap<String, Box<dyn Factory>>);
impl Registry {
    pub fn register(&mut self, factory: Box<dyn Factory>) -> Result<()> {
        let id = factory.descriptor().id;
        if id.is_empty() || self.0.contains_key(&id) {
            return Err(format!("duplicate or empty processor ID: {id}"));
        }
        self.0.insert(id, factory);
        Ok(())
    }
    pub fn descriptors(&self) -> Vec<Descriptor> {
        self.0.values().map(|p| p.descriptor()).collect()
    }
    pub fn get(&self, id: &str) -> Result<&dyn Factory> {
        self.0
            .get(id)
            .map(|p| p.as_ref())
            .ok_or_else(|| format!("processor not registered: {id}"))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    struct Example;
    impl Factory for Example {
        fn descriptor(&self) -> Descriptor {
            Descriptor {
                id: "example.identity".into(),
                name: "Example".into(),
                settings_schema: serde_json::json!({}),
            }
        }
        fn create(&self, _: usize, _: usize, _: &serde_json::Value) -> Result<Box<dyn Session>> {
            Err("fixture only".into())
        }
    }
    #[test]
    fn duplicate_registration_is_rejected_without_replacing() {
        let mut r = Registry::default();
        r.register(Box::new(Example)).unwrap();
        assert!(r.register(Box::new(Example)).is_err());
        assert_eq!(r.descriptors().len(), 1);
        assert!(r.get("missing").is_err());
    }
}
