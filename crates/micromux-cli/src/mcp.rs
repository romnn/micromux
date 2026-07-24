//! Thin shim for the `micromux mcp` subcommand. Gated at the top behind the default-on `mcp`
//! feature, so with the feature off this module, the subcommand, and `rmcp` all vanish at compile
//! time.

use color_eyre::eyre;

/// Serve the MCP server over stdio until the agent disconnects.
///
/// # Errors
///
/// Returns an error if the stdio transport or the service loop fails.
pub async fn run() -> eyre::Result<()> {
    micromux_mcp::serve_stdio()
        .await
        .map_err(|err| eyre::eyre!("mcp server error: {err}"))?;
    Ok(())
}
