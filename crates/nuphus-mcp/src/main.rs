//! nuphus-mcp binary entrypoint — stdio JSON-RPC transport layer.
//!
//! Protocol: one JSON line over stdin/stdout (matches the stdio client in `src/mcp/client.rs`).
//! stdout only carries JSON-RPC response lines; all logs go to stderr.
//!
//! Security option: `--confirm-write` (or env var `NUPHUS_MCP_CONFIRM_WRITE=1`)
//! enables strict confirmation mode — write tools require an explicit `"confirm": true` argument.

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};

use nuphus_browser::shutdown_browser;
use nuphus_mcp::security::SecurityPolicy;
use nuphus_mcp::server::McpServer;

/// Stack size for the thread that runs the stdio loop and every tool dispatch.
///
/// The deepest tool path (`browser_navigate` → CDP client → per-command
/// timeouts → nested chromiumoxide futures) needs far more stack than the
/// Windows default main-thread stack. On that path the process used to die with
/// `thread 'main' has overflowed its stack` mid-tool, after which *every*
/// `tools/call` went unanswered — indistinguishable from a total tool hang, and
/// only recoverable by killing the process. Running the loop on a dedicated
/// thread with room to spare removes that failure mode.
const SERVE_STACK_BYTES: usize = 64 * 1024 * 1024;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let handle = std::thread::Builder::new()
        .name("nuphus-mcp-serve".to_string())
        .stack_size(SERVE_STACK_BYTES)
        .spawn(run_serve)?;
    match handle.join() {
        Ok(Ok(())) => Ok(()),
        // Stringified on the serve thread so the error crosses the join safely.
        Ok(Err(message)) => Err(message.into()),
        // A panic in the serve thread belongs to the process, not to this
        // wrapper: resume unwinding so the default handler reports it.
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

/// Build the process runtime and run the stdio loop (on the big-stack thread).
fn run_serve() -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("failed to build the tokio runtime: {e}"))?;
    runtime.block_on(serve()).map_err(|e| e.to_string())
}

async fn serve() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "nuphus_mcp=info,warn".into()),
        )
        .with_writer(std::io::stderr) // stdout is reserved for the JSON-RPC protocol
        .init();

    // --confirm-write CLI flag overrides the environment variable
    let cli_confirm = std::env::args().any(|a| a == "--confirm-write");
    let policy = if cli_confirm {
        SecurityPolicy {
            strict_confirm: true,
        }
    } else {
        SecurityPolicy::from_env()
    };
    if policy.strict_confirm {
        tracing::warn!("[nuphus-mcp] STRICT CONFIRM MODE: write tools require \"confirm\": true");
    }

    let mut server = McpServer::with_policy(policy);
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    let stdout = tokio::io::stdout();
    let mut writer = BufWriter::new(stdout);

    let mut line: Vec<u8> = Vec::new();
    loop {
        line.clear();
        let n = match read_line_capped(&mut reader, &mut line, MAX_LINE_BYTES).await {
            Ok(n) => n,
            Err(e) => {
                // Oversized request line: reply with a protocol error and keep serving.
                let err = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": { "code": -32700, "message": format!("request too large: {e}") }
                });
                writer.write_all(err.to_string().as_bytes()).await?;
                writer.write_all(b"\n").await?;
                writer.flush().await?;
                continue;
            }
        };
        if n == 0 {
            break; // EOF (client closed the pipe / process ended)
        }
        // Lossy: a non-UTF-8 line yields replacement chars and is rejected by the JSON
        // parser with a parse error, instead of aborting the whole server on read.
        let trimmed = String::from_utf8_lossy(&line);
        let trimmed = trimmed.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(response) = server.handle_line(trimmed).await {
            writer.write_all(response.as_bytes()).await?;
            writer.write_all(b"\n").await?;
            writer.flush().await?;
        }
        // MCP lifecycle: an `exit` notification terminates the server. Flush any
        // pending output, then exit 0 when it followed a `shutdown` request,
        // 1 when the client skipped the graceful shutdown (per spec).
        if server.exit_received() {
            writer.flush().await?;
            let code = server.exit_code();
            tracing::info!("[nuphus-mcp] exit notification received, exiting (code={code})");
            // Explicit browser shutdown: the shared client lives in a `static` and is never
            // dropped on exit, so without this a launched Chrome would be orphaned, holding
            // the profile lock and destabilizing the next session (see shared::shutdown_browser).
            shutdown_browser().await;
            std::process::exit(code as i32);
        }
    }

    // stdin EOF (host closed the pipe): same explicit cleanup before returning.
    tracing::info!("[nuphus-mcp] stdin EOF, shutting down");
    shutdown_browser().await;
    Ok(())
}

/// Max bytes per JSON-RPC request line. Requests larger than this are rejected with a
/// parse error rather than letting the buffer grow without bound (defense-in-depth for
/// a misbehaving host process).
const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;

/// Read a single LF-terminated line with a hard byte cap.
///
/// Returns the number of bytes read (0 on EOF). If the line exceeds `max`, the rest of
/// the line is drained to keep the stream aligned and `Err` is returned so the caller
/// can emit a protocol error and continue serving.
async fn read_line_capped(
    reader: &mut BufReader<tokio::io::Stdin>,
    out: &mut Vec<u8>,
    max: usize,
) -> std::io::Result<usize> {
    use std::io::ErrorKind;
    loop {
        let buf = reader.fill_buf().await?;
        if buf.is_empty() {
            return Ok(out.len()); // EOF
        }
        let newline = buf
            .iter()
            .position(|&b| b == b'\n')
            .map(|i| i + 1)
            .unwrap_or(buf.len());
        if out.len() + newline > max {
            let mut rest = Vec::new();
            let _ = reader.read_until(b'\n', &mut rest).await; // drain to stay aligned
            return Err(std::io::Error::new(
                ErrorKind::InvalidData,
                format!("request line exceeds {max} byte limit"),
            ));
        }
        out.extend_from_slice(&buf[..newline]);
        // Stop at the line terminator; if the whole chunk was consumed without a
        // newline, keep buffering the next chunk.
        let ended = newline < buf.len() || buf[newline - 1] == b'\n';
        reader.consume(newline);
        if ended {
            break;
        }
    }
    Ok(out.len())
}
