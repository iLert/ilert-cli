mod classification;
mod cli;
mod client;
mod commands;
mod config;
mod endpoint;
mod errors;
mod http;
mod interactive;
mod jq;
mod mode;
mod oauth;
mod openapi;
mod output;
mod preview;
mod runner;
mod sanitize;
mod secret_store;
#[cfg(test)]
mod testutil;
mod tui;

use anyhow::Result;
use cli::Cli;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::new().await?;
    cli.run().await
}
