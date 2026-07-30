//! Thin shim for the `micromux mcp` subcommand. Gated at the top behind the default-on `mcp`
//! feature, so with the feature off this module, the subcommand, and `rmcp` all vanish at compile
//! time.

/// Serve the MCP server over stdio until the agent disconnects.
///
/// # Errors
///
/// Returns an error if the stdio transport or the service loop fails.
pub async fn run(allow_session_start: bool) -> Result<(), crate::Error> {
    micromux_mcp::serve_stdio(allow_session_start)
        .await
        .map_err(crate::Error::Mcp)?;
    Ok(())
}
