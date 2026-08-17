use std::io::Write;
use std::time::Duration;

use anyhow::Result;
use reqwest::Method;

use crate::cli::RunContext;
use crate::http::HttpClient;
use crate::openapi::Operation;

const DEFAULT_INTERVAL: u64 = 5;

/// Run a watch loop, re-executing the operation every `interval` seconds.
pub async fn run_watch(
    client: &HttpClient,
    operation: &Operation,
    path: &str,
    query: &[(String, String)],
    headers: &[(String, String)],
    interval_secs: Option<u64>,
    ctx: &RunContext,
) -> Result<()> {
    let interval = Duration::from_secs(interval_secs.unwrap_or(DEFAULT_INTERVAL));
    let method: Method = operation.method.parse()?;

    loop {
        // Clear screen
        print!("\x1B[2J\x1B[H");
        std::io::stdout().flush()?;

        // Header
        let now = chrono::Local::now().format("%H:%M:%S");
        eprintln!(
            "Every {}s: ilert {} {} | {}",
            interval.as_secs(),
            operation.tag,
            operation.action,
            now,
        );
        eprintln!();

        match client
            .request(method.clone(), path, query, headers, None)
            .await
        {
            // Each tick is a full render, `--jq` and `--fields` included: a
            // watch is the same output repeated, not a different one.
            Ok((_, body)) => ctx.print_response(&body)?,
            Err(e) => {
                // A watch clears the screen and redraws every tick, so an
                // escape sequence in a server error message would be replayed
                // for as long as the command runs — and it is the one place
                // where an error is printed instead of returned to
                // `output::print_error`.
                eprintln!("Error: {}", crate::sanitize::terminal_string(e.to_string()));
            }
        }

        tokio::time::sleep(interval).await;
    }
}
