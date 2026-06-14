pub mod client;
pub mod fs;
pub mod shell;

pub async fn start(
    relay_url: String,
    _key: Option<String>,
    root: String,
    token_type: String,
) -> anyhow::Result<()> {
    tracing::info!(
        "Agent starting: relay_url={}, root={}, token_type={}",
        relay_url,
        root,
        token_type
    );
    Ok(())
}
