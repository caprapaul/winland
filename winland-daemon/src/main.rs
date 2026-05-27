use anyhow::Result;

fn main() -> Result<()> {
    tracing_subscriber::fmt().with_target(false).init();
    println!("winland-daemon is scaffolded; daemon behavior is intentionally not implemented yet.");
    Ok(())
}
