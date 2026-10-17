fn main() -> anyhow::Result<()> {
    abner::run(abner_processor_api::Registry::default())
}
