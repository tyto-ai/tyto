use anyhow::Result;
use rmcp::{ClientHandler, ServiceExt, model::{CallToolRequestParams, ClientInfo}};

use crate::config::Config;

struct MinimalClient;

impl ClientHandler for MinimalClient {
    fn get_info(&self) -> ClientInfo {
        ClientInfo::default()
    }
}

/// CLI entry point for `tyto request <tool> [json-args]`.
/// Exits 0 silently when serve is not running.
pub async fn run(config: &Config, tool: &str, args: Option<&str>) -> Result<()> {
    if let Some(text) = call_tool_on_server(config, tool, args).await? {
        print!("{text}");
    }
    Ok(())
}

/// Call the `search` tool on the running serve instance.
/// Returns None if serve is not reachable, the call fails, or the timeout elapses.
pub async fn call_search(config: &Config, query: &str, limit: usize, timeout_ms: u64) -> Option<String> {
    let args = serde_json::json!({"query": query, "limit": limit}).to_string();
    let fut = call_tool_on_server(config, "search", Some(&args));
    with_timeout(fut, timeout_ms).await
}

/// Call the `session_context` tool on the running serve instance.
/// Returns None if serve is not reachable, the call fails, or the timeout elapses.
pub async fn call_session_context(config: &Config, timeout_ms: u64) -> Option<String> {
    let fut = call_tool_on_server(config, "session_context", None);
    with_timeout(fut, timeout_ms).await
}

async fn with_timeout(
    fut: impl std::future::Future<Output = anyhow::Result<Option<String>>>,
    timeout_ms: u64,
) -> Option<String> {
    if timeout_ms == 0 {
        return fut.await.ok().flatten();
    }
    match tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), fut).await {
        Ok(result) => result.ok().flatten(),
        Err(_) => {
            tracing::debug!(timeout_ms, "socket call timed out");
            None
        }
    }
}

/// Connect to the running serve instance, call one tool, and return the text output.
/// Returns Ok(None) when serve is not running or the socket is unreachable.
pub async fn call_tool_on_server(
    config: &Config,
    tool: &str,
    args: Option<&str>,
) -> Result<Option<String>> {
    let t = std::time::Instant::now();

    #[cfg(unix)]
    {
        use tokio::net::UnixStream;
        let socket_path = config.serve_socket_path();
        if !socket_path.exists() {
            tracing::debug!(tool, "socket not found, skipping IPC");
            return Ok(None);
        }
        let t_connect = std::time::Instant::now();
        let stream = match UnixStream::connect(&socket_path).await {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(tool, error = %e, "socket connect failed");
                let _ = std::fs::remove_file(&socket_path);
                return Ok(None);
            }
        };
        tracing::debug!(elapsed_ms = t_connect.elapsed().as_millis(), tool, "socket connect");
        let client = MinimalClient.serve(stream).await?;
        let result = invoke_tool(client.peer(), tool, args).await?;
        tracing::debug!(elapsed_ms = t.elapsed().as_millis(), tool, "IPC call total");
        return Ok(Some(result));
    }

    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ClientOptions;
        let pipe_name = config.serve_pipe_name();
        let stream = match ClientOptions::new().open(&pipe_name) {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(tool, error = %e, "pipe connect failed");
                return Ok(None);
            }
        };
        let client = MinimalClient.serve(stream).await?;
        let result = invoke_tool(client.peer(), tool, args).await?;
        tracing::debug!(elapsed_ms = t.elapsed().as_millis(), tool, "IPC call total");
        return Ok(Some(result));
    }

    #[allow(unreachable_code)]
    Ok(None)
}

async fn invoke_tool(
    peer: &rmcp::Peer<rmcp::RoleClient>,
    tool: &str,
    args: Option<&str>,
) -> Result<String> {
    let t = std::time::Instant::now();
    let arguments = args
        .map(serde_json::from_str::<serde_json::Map<String, serde_json::Value>>)
        .transpose()?;
    let params = match arguments {
        Some(map) => CallToolRequestParams::new(tool.to_string()).with_arguments(map),
        None => CallToolRequestParams::new(tool.to_string()),
    };
    let result = peer.call_tool(params).await?;
    tracing::debug!(elapsed_ms = t.elapsed().as_millis(), tool, "tool RPC");
    let text = result
        .content
        .iter()
        .filter_map(|c| c.raw.as_text())
        .map(|t| t.text.as_str())
        .collect::<Vec<_>>()
        .join("");
    Ok(text)
}
