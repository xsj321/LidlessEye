//! stdio MCP server exposing LidlessEye service maps and profiles to AI tools.

mod client;
mod summary;
mod tools;

use std::time::Duration;

use clap::Parser;
use rmcp::ServiceExt;
use rmcp::transport::stdio;
use tracing_subscriber::EnvFilter;

use crate::client::LidlessEyeClient;
use crate::tools::LidlessEyeTools;

/// LidlessEye's read-only MCP adapter.
#[derive(Debug, Parser)]
#[command(
    name = "lidlesseye-mcp",
    version,
    about = "Serve LidlessEye service-map and profiling data over Model Context Protocol stdio."
)]
struct Args {
    /// LidlessEye server base URL.
    #[arg(
        long,
        env = "LIDLESSEYE_BASE_URL",
        default_value = "http://127.0.0.1:4040"
    )]
    base_url: String,
    /// Bearer token sent to the LidlessEye server.
    #[arg(long, env = "LIDLESSEYE_AUTH_TOKEN")]
    token: Option<String>,
    /// Tenant sent with every tenant-scoped query.
    #[arg(long, env = "LIDLESSEYE_TENANT", default_value = "default")]
    tenant: String,
    /// Header used to carry the tenant.
    #[arg(
        long,
        env = "LIDLESSEYE_TENANT_HEADER",
        default_value = "X-Scope-OrgID"
    )]
    tenant_header: String,
    /// HTTP request timeout in seconds.
    #[arg(long, env = "LIDLESSEYE_TIMEOUT_SECONDS", default_value_t = 60)]
    timeout_seconds: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // stdout belongs to the MCP JSON-RPC stream; diagnostics go to stderr.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .init();

    let args = Args::parse();
    let client = LidlessEyeClient::new(
        &args.base_url,
        args.token,
        &args.tenant,
        &args.tenant_header,
        Duration::from_secs(args.timeout_seconds),
    )
    .map_err(anyhow::Error::msg)?;
    let tools = LidlessEyeTools::new(client);
    let service = tools.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
