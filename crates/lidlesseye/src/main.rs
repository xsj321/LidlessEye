//! The `lidlesseye` command line tool.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, bail};
use clap::{Args, Parser, Subcommand};
use lidlesseye::core::{ProfileType, QueryOptions, Selector, StoreConfig};
use lidlesseye::{Builder, PublisherConfig};

#[derive(Debug, Parser)]
#[command(
    name = "lidlesseye",
    version,
    about = "Continuous eBPF profiling with a columnar store",
    long_about = "LidlessEye profiles Linux processes with eBPF, stores the symbolized stacks in \
                  immutable columnar segments and serves flamegraphs over HTTP.\n\n\
                  Serve and profile this machine:\n  lidlesseye serve --agent --data-dir ./data\n\n\
                  Run a central server and per-node agents:\n  lidlesseye serve --data-dir /var/lib/lidlesseye\n  lidlesseye collect --server http://lidlesseye:4040"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the HTTP server (and optionally the eBPF collector).
    Serve(ServeArgs),
    /// Run only the eBPF collector and push profiles to a server.
    Collect(CollectArgs),
    /// Import reports from files or stdin.
    Ingest(IngestArgs),
    /// Inspect a data directory from the terminal.
    Query(QueryArgs),
    /// Verify the checksum of every segment.
    Fsck(FsckArgs),
    /// Print build and platform information.
    Info,
}

#[derive(Debug, Args)]
struct ServeArgs {
    #[arg(long, env = "LIDLESSEYE_DATA_DIR", default_value = "./lidlesseye-data")]
    data_dir: PathBuf,
    #[arg(long, env = "LIDLESSEYE_LISTEN", default_value = "0.0.0.0:4040")]
    listen: String,
    /// Delete profiles older than this (`7d`, `24h`, `0` to keep everything).
    #[arg(long, default_value = "7d")]
    retention: String,
    /// Soft cap for the data directory (`50GB`, `0` for unlimited).
    #[arg(long, default_value = "50GB")]
    max_disk: String,
    #[arg(long, env = "LIDLESSEYE_AUTH_TOKEN")]
    auth_token: Option<String>,
    /// Bind a bearer token to a tenant: `--tenant-token <token>=<tenant>`
    /// (repeatable). Implies authentication is required. Also read from
    /// `LIDLESSEYE_TENANT_TOKENS` as a comma-separated list, so a deployment
    /// can keep tokens out of the pod spec.
    #[arg(
        long = "tenant-token",
        env = "LIDLESSEYE_TENANT_TOKENS",
        value_delimiter = ',',
        value_parser = parse_tenant_token
    )]
    tenant_tokens: Vec<(String, String)>,
    /// Only allow these tenants (repeatable). Default: any valid id.
    #[arg(long = "allowed-tenant")]
    allowed_tenants: Vec<String>,
    /// Request header carrying the tenant id.
    #[arg(long, default_value = "X-Scope-OrgID")]
    tenant_header: String,
    /// Maximum number of tenants this server will host.
    #[arg(long, default_value_t = 1000)]
    max_tenants: usize,
    /// Per-tenant limit, `<tenant>:<field>=<value>` (repeatable). Fields:
    /// `retention` (`30d`), `max_disk` (`200GB`), `max_series` (`50000`).
    /// `0` means "inherit the server default".
    #[arg(long = "tenant-limit", value_parser = parse_tenant_limit)]
    tenant_limit: Vec<(String, String, String)>,
    /// Tenant the built-in agent writes to.
    #[arg(long, default_value = "default")]
    tenant: String,
    /// CORS origin, `*` for any.
    #[arg(long)]
    cors: Option<String>,
    /// Reject ingest requests.
    #[arg(long)]
    read_only: bool,
    /// Start the eBPF collector in this process (Linux + root).
    #[arg(long)]
    agent: bool,
    /// Service name for collected profiles (default: process name).
    #[arg(long)]
    service_name: Option<String>,
    /// Extra label, `key=value` (repeatable).
    #[arg(long = "label", value_parser = parse_label)]
    labels: Vec<(String, String)>,
    /// CPU sampling frequency in Hz.
    #[arg(long, default_value_t = 99)]
    frequency: u64,
    /// Collector flush interval (`10s`, `5s`).
    #[arg(long, default_value = "10s")]
    flush: String,
    #[arg(long)]
    no_cpu: bool,
    #[arg(long)]
    no_offcpu: bool,
}

#[derive(Debug, Args)]
struct CollectArgs {
    /// LidlessEye server base URL.
    #[arg(long, env = "LIDLESSEYE_SERVER")]
    server: String,
    #[arg(long, env = "LIDLESSEYE_AUTH_TOKEN")]
    auth_token: Option<String>,
    /// Tenant to write to on the server.
    #[arg(long, default_value = "default")]
    tenant: String,
    #[arg(long)]
    service_name: Option<String>,
    #[arg(long = "label", value_parser = parse_label)]
    labels: Vec<(String, String)>,
    #[arg(long, default_value_t = 99)]
    frequency: u64,
    #[arg(long, default_value = "10s")]
    flush: String,
    #[arg(long)]
    no_cpu: bool,
    #[arg(long)]
    no_offcpu: bool,
    /// Stop after this long (used by tests); run forever by default.
    #[arg(long)]
    duration: Option<String>,
}

#[derive(Debug, Args)]
struct IngestArgs {
    /// Write directly into this data directory.
    #[arg(long)]
    data_dir: Option<PathBuf>,
    /// Or push to a running server.
    #[arg(long)]
    server: Option<String>,
    #[arg(long, env = "LIDLESSEYE_AUTH_TOKEN")]
    auth_token: Option<String>,
    /// Tenant to import into.
    #[arg(long, default_value = "default")]
    tenant: String,
    /// Input format: `json`, `pprof` or `folded`.
    #[arg(long, default_value = "json")]
    format: String,
    /// Service name, required by `folded` and used for `pprof` without one.
    #[arg(long)]
    service_name: Option<String>,
    /// Profile type override (`cpu`, `wall`, `alloc_space`, ...).
    #[arg(long)]
    profile_type: Option<String>,
    /// Report files; reads stdin when empty.
    files: Vec<PathBuf>,
}

#[derive(Debug, Args)]
struct QueryArgs {
    #[arg(long, env = "LIDLESSEYE_DATA_DIR", default_value = "./lidlesseye-data")]
    data_dir: PathBuf,
    /// Tenant to inspect.
    #[arg(long, default_value = "default")]
    tenant: String,
    #[arg(long, default_value = "cpu")]
    profile_type: String,
    /// Start of the range: `1h`, `30m`, `7d` (relative) or RFC3339.
    #[arg(long, default_value = "1h")]
    from: String,
    #[arg(long)]
    until: Option<String>,
    #[arg(long)]
    service: Option<String>,
    #[arg(long = "selector")]
    raw_selector: Option<String>,
    /// Print the top N functions instead of the tree.
    #[arg(long)]
    top: bool,
    #[arg(long, default_value_t = 25)]
    limit: usize,
    /// Print the call tree instead of the top table.
    #[arg(long)]
    tree: bool,
    /// Hide nodes below this percentage of total.
    #[arg(long, default_value_t = 1.0)]
    min_percent: f64,
    /// Split the profile on this label (`service_name`, `pod`, ...).
    #[arg(long)]
    group_by: Option<String>,
}

#[derive(Debug, Args)]
struct FsckArgs {
    #[arg(long, env = "LIDLESSEYE_DATA_DIR", default_value = "./lidlesseye-data")]
    data_dir: PathBuf,
    /// Only check this tenant (default: every tenant).
    #[arg(long)]
    tenant: Option<String>,
    /// Delete segments that fail verification.
    #[arg(long)]
    fix: bool,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(4)
        .build()
        .context("failed to start the tokio runtime")?;
    runtime.block_on(run(cli))
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Command::Serve(args) => serve(args).await,
        Command::Collect(args) => collect(args).await,
        Command::Ingest(args) => ingest(args).await,
        Command::Query(args) => query(args),
        Command::Fsck(args) => fsck(args),
        Command::Info => {
            println!("lidlesseye {}", env!("CARGO_PKG_VERSION"));
            println!(
                "profile format: LESEG v{}",
                lidlesseye::core::segment::FORMAT_VERSION
            );
            println!(
                "ebpf support: {}",
                if cfg!(target_os = "linux") {
                    "yes"
                } else {
                    "no (Linux only)"
                }
            );
            println!(
                "ui assets embedded: {}",
                !lidlesseye::server::ui_assets().is_empty()
            );
            Ok(())
        }
    }
}

async fn serve(args: ServeArgs) -> anyhow::Result<()> {
    lidlesseye::init_tracing("lidlesseye=info,tower_http=warn");
    let labels: BTreeMap<String, String> = args.labels.into_iter().collect();
    let retention = parse_optional_duration(&args.retention)?;
    let max_disk = parse_optional_bytes(&args.max_disk)?;

    let mut builder = Builder::new()
        .data_dir(&args.data_dir)
        .listen(&args.listen)
        .retention(retention)
        .max_disk_bytes(max_disk)
        .auth_token(args.auth_token.clone())
        .cors(args.cors.clone())
        .read_only(args.read_only)
        .agent(args.agent)
        .labels(labels)
        .frequency_hz(args.frequency)
        .flush_interval(parse_duration(&args.flush)?)
        .collect_cpu(!args.no_cpu)
        .collect_off_cpu(!args.no_offcpu)
        .tenant(args.tenant)
        .tenant_header(args.tenant_header)
        .max_tenants(args.max_tenants);
    for (token, tenant) in args.tenant_tokens {
        builder = builder.tenant_token(token, tenant);
    }
    if !args.allowed_tenants.is_empty() {
        builder = builder.allowed_tenants(args.allowed_tenants);
    }
    for (tenant, limits) in assemble_tenant_limits(&args.tenant_limit)? {
        builder = builder.tenant_limits(tenant, limits);
    }
    if let Some(name) = args.service_name {
        builder = builder.service_name(name);
    }

    let eye = builder.build().await?;
    println!(
        "LidlessEye {} listening on {}",
        env!("CARGO_PKG_VERSION"),
        eye.base_url()
    );
    println!("  data dir : {}", eye.data_dir().display());
    println!("  dashboard: {}/", eye.base_url());
    if args.agent {
        println!("  eBPF     : collecting at {} Hz", args.frequency);
    }
    eye.run().await?;
    Ok(())
}

async fn collect(args: CollectArgs) -> anyhow::Result<()> {
    lidlesseye::init_tracing("lidlesseye=info");
    if !cfg!(target_os = "linux") {
        bail!("the eBPF collector requires Linux");
    }
    let mut cfg = PublisherConfig::new(&args.server);
    cfg.auth_token = args.auth_token;
    cfg.tenant = Some(args.tenant.clone());
    let (publisher, uploader) = lidlesseye::publisher::start(cfg)?;
    println!(
        "pushing profiles to {} as tenant `{}`",
        publisher.endpoint(),
        args.tenant
    );

    let labels: BTreeMap<String, String> = args.labels.into_iter().collect();
    let collector_cfg = lidlesseye::ebpf::CollectorConfig {
        service_name: args.service_name,
        labels,
        frequency_hz: args.frequency,
        flush_interval: parse_duration(&args.flush)?,
        collect_cpu: !args.no_cpu,
        collect_off_cpu: !args.no_offcpu,
        ..Default::default()
    };
    let sink = publisher.clone();
    let handle = lidlesseye::ebpf::start(collector_cfg, move |report| {
        sink.send(report);
    })
    .map_err(anyhow::Error::msg)?;

    match args.duration {
        Some(duration) => tokio::time::sleep(parse_duration(&duration)?).await,
        None => {
            tokio::signal::ctrl_c().await?;
        }
    }
    let stats = handle.stats();
    handle.stop();
    println!(
        "collected {} cpu / {} off-cpu records in {} flushes",
        stats.cpu_records, stats.offcpu_records, stats.drains
    );
    // Give the uploader a moment to drain its queue.
    tokio::time::sleep(Duration::from_millis(500)).await;
    drop(publisher);
    uploader.abort();
    Ok(())
}

async fn ingest(args: IngestArgs) -> anyhow::Result<()> {
    lidlesseye::init_tracing("lidlesseye=warn");
    let tenant = lidlesseye::core::TenantId::parse(&args.tenant)?;

    // Read the payloads once; they are either JSON or a binary profile format.
    let mut blobs: Vec<Vec<u8>> = Vec::new();
    if args.files.is_empty() {
        let mut buf = Vec::new();
        std::io::stdin()
            .read_to_end(&mut buf)
            .context("failed to read stdin")?;
        blobs.push(buf);
    } else {
        for file in &args.files {
            blobs.push(
                std::fs::read(file)
                    .with_context(|| format!("failed to read {}", file.display()))?,
            );
        }
    }

    let now = lidlesseye::core::now_ns();
    let format = args.format.to_ascii_lowercase();
    let mut reports = Vec::new();
    for blob in &blobs {
        match format.as_str() {
            "json" => {
                let document =
                    std::str::from_utf8(blob).context("JSON input is not valid UTF-8")?;
                reports.extend(parse_reports(&[document.to_string()])?);
            }
            "pprof" | "protobuf" | "proto" => {
                let opts = lidlesseye::core::PprofOptions {
                    service_name: args.service_name.clone(),
                    profile_type: args.profile_type.clone(),
                    labels: BTreeMap::new(),
                    start_ns: None,
                    end_ns: None,
                };
                reports.extend(lidlesseye::core::decode_pprof(blob, &opts)?);
            }
            "folded" | "collapsed" => {
                let service_name = args.service_name.clone().ok_or_else(|| {
                    anyhow::anyhow!("--service-name is required for folded input")
                })?;
                let text = std::str::from_utf8(blob).context("folded input is not valid UTF-8")?;
                let opts = lidlesseye::core::FoldedOptions {
                    service_name,
                    profile_type: ProfileType::new(
                        args.profile_type
                            .clone()
                            .unwrap_or_else(|| "cpu".to_string()),
                    ),
                    start_ns: now.saturating_sub(10_000_000_000),
                    end_ns: now,
                    labels: BTreeMap::new(),
                };
                reports.push(lidlesseye::core::decode_folded(text, &opts)?);
            }
            other => bail!("unsupported --format `{other}`; expected json, pprof or folded"),
        }
    }
    if reports.is_empty() {
        bail!("no reports found in the input");
    }

    if let Some(dir) = args.data_dir {
        let registry = lidlesseye::core::TenantStore::open(&dir, StoreConfig::new(&dir))
            .context("failed to open the data directory")?;
        let store = registry.open_tenant(&tenant)?;
        let mut count = 0;
        for report in &reports {
            let meta = store.append(report)?;
            println!(
                "wrote {} ({} samples, {} bytes)",
                meta.path.display(),
                meta.sample_count,
                meta.size_bytes
            );
            count += 1;
        }
        println!(
            "imported {count} reports into {} as tenant `{tenant}`",
            dir.display()
        );
        return Ok(());
    }

    let server = args
        .server
        .ok_or_else(|| anyhow::anyhow!("either --data-dir or --server is required"))?;
    let mut cfg = PublisherConfig::new(server);
    cfg.auth_token = args.auth_token;
    cfg.tenant = Some(tenant.to_string());
    let (publisher, uploader) = lidlesseye::publisher::start(cfg)?;
    for report in reports {
        publisher.send_now(report).await?;
    }
    uploader.abort();
    println!("uploaded to {} as tenant `{tenant}`", publisher.endpoint());
    Ok(())
}

fn query(args: QueryArgs) -> anyhow::Result<()> {
    let tenant = lidlesseye::core::TenantId::parse(&args.tenant)?;
    let registry =
        lidlesseye::core::TenantStore::open(&args.data_dir, StoreConfig::new(&args.data_dir))
            .context("failed to open the data directory")?;
    let store = registry.tenant(&tenant).ok_or_else(|| {
        anyhow::anyhow!(
            "tenant `{tenant}` has no data in {}",
            args.data_dir.display()
        )
    })?;
    let now = lidlesseye::core::now_ns();
    let until = match &args.until {
        Some(value) => resolve_time(value, now)?,
        None => now,
    };
    let from = resolve_time(&args.from, until)?;
    if until <= from {
        bail!("--from must be before --until");
    }

    let selector = if let Some(raw) = &args.raw_selector {
        Selector::parse(raw)?
    } else if let Some(service) = &args.service {
        Selector::service(service)
    } else {
        Selector::default()
    };

    let opts = QueryOptions::new(
        ProfileType::new(&args.profile_type),
        from,
        until,
        selector.clone(),
    )
    .with_group_by(args.group_by.clone());

    if args.tree || !args.top {
        let flamegraph = lidlesseye::core::flamegraph(&store, &opts)?;
        if flamegraph.is_empty() {
            println!("no samples in the selected range");
            return Ok(());
        }
        println!(
            "{} profile, total {}, {} nodes\n",
            args.profile_type,
            format_value(flamegraph.total, &flamegraph.unit),
            flamegraph.names.len()
        );
        for root in flamegraph.to_tree() {
            print_tree(&root, 0, flamegraph.total, args.min_percent);
        }
        return Ok(());
    }

    let result = lidlesseye::core::top(&store, &opts, args.limit)?;
    if result.items.is_empty() {
        println!("no samples in the selected range");
        return Ok(());
    }
    let total = result.total_value.max(1);
    println!("{:<44} {:>12} {:>12}  MODULE", "FUNCTION", "SELF", "TOTAL");
    for row in result.items {
        let name = truncate(&row.name, 44);
        println!(
            "{:<44} {:>12} {:>12}  {}",
            name,
            format_value(row.self_value, "nanoseconds"),
            format_value(row.total_value, "nanoseconds"),
            truncate(&row.module, 60)
        );
    }
    println!("\ntotal: {}", format_value(total, "nanoseconds"));
    Ok(())
}

fn fsck(args: FsckArgs) -> anyhow::Result<()> {
    let registry =
        lidlesseye::core::TenantStore::open(&args.data_dir, StoreConfig::new(&args.data_dir))
            .context("failed to open the data directory")?;
    let only = match &args.tenant {
        Some(raw) => Some(lidlesseye::core::TenantId::parse(raw)?),
        None => None,
    };
    let results: Vec<_> = registry
        .fsck()
        .into_iter()
        .filter(|(tenant, _, _)| only.as_ref().is_none_or(|want| want == tenant))
        .collect();
    let mut failures = 0;
    let mut bytes = 0u64;
    for (tenant, path, result) in &results {
        match result {
            Ok(()) => {
                if let Ok(meta) = std::fs::metadata(path) {
                    bytes += meta.len();
                }
            }
            Err(err) => {
                failures += 1;
                eprintln!("FAIL [{tenant}] {}: {err}", path.display());
                if args.fix {
                    std::fs::remove_file(path)
                        .with_context(|| format!("failed to delete {}", path.display()))?;
                    eprintln!("  removed");
                }
            }
        }
    }
    println!(
        "checked {} segments ({}), {failures} failed",
        results.len(),
        human_bytes(bytes)
    );
    if failures > 0 && !args.fix {
        bail!("{failures} segments are corrupt; re-run with --fix to delete them");
    }
    Ok(())
}

fn parse_reports(documents: &[String]) -> anyhow::Result<Vec<lidlesseye::core::Report>> {
    let mut reports = Vec::new();
    for document in documents {
        let value: serde_json::Value =
            serde_json::from_str(document).context("input is not valid JSON")?;
        match value {
            serde_json::Value::Array(items) => {
                for item in items {
                    reports.push(serde_json::from_value(item)?);
                }
            }
            serde_json::Value::Object(mut map) => {
                if let Some(serde_json::Value::Array(items)) = map.remove("reports") {
                    for item in items {
                        reports.push(serde_json::from_value(item)?);
                    }
                } else {
                    reports.push(serde_json::from_value(serde_json::Value::Object(map))?);
                }
            }
            _ => bail!("expected a report object or an array of reports"),
        }
    }
    Ok(reports)
}

fn print_tree(node: &lidlesseye::core::TreeNode, depth: usize, total: i64, min_percent: f64) {
    let percent = if total > 0 {
        node.value as f64 * 100.0 / total as f64
    } else {
        0.0
    };
    if percent < min_percent && depth > 0 {
        return;
    }
    let indent = "  ".repeat(depth);
    println!(
        "{indent}{} — {} ({:.1}%)",
        truncate(&node.name, 60),
        format_value(node.value, "nanoseconds"),
        percent
    );
    let mut children: Vec<&lidlesseye::core::TreeNode> = node.children.iter().collect();
    children.sort_by_key(|c| std::cmp::Reverse(c.value));
    for child in children {
        print_tree(child, depth + 1, total, min_percent);
    }
}

fn format_value(value: i64, unit: &str) -> String {
    match unit {
        "bytes" => human_bytes(value.max(0) as u64),
        "count" => format!("{value}"),
        _ => {
            let nanos = value as f64;
            if nanos >= 1e9 {
                format!("{:.2}s", nanos / 1e9)
            } else if nanos >= 1e6 {
                format!("{:.1}ms", nanos / 1e6)
            } else if nanos >= 1e3 {
                format!("{:.1}µs", nanos / 1e3)
            } else {
                // Sub-microsecond values are normal for a 99 Hz sampler, and
                // rounding them to `0µs` would read as "no time at all".
                format!("{:.0}ns", nanos)
            }
        }
    }
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn truncate(input: &str, max: usize) -> String {
    if input.chars().count() <= max {
        return input.to_string();
    }
    let mut out: String = input.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn parse_label(input: &str) -> Result<(String, String), String> {
    let (key, value) = input
        .split_once('=')
        .ok_or_else(|| format!("expected key=value, got `{input}`"))?;
    if key.trim().is_empty() {
        return Err("label key is empty".to_string());
    }
    Ok((key.trim().to_string(), value.trim().to_string()))
}

/// `--tenant-token <token>=<tenant>`; the tenant is validated here so a typo
/// fails at startup rather than on the first request.
fn parse_tenant_token(input: &str) -> Result<(String, String), String> {
    let (token, tenant) = input
        .split_once('=')
        .ok_or_else(|| format!("expected token=tenant, got `{input}`"))?;
    if token.trim().is_empty() {
        return Err("token is empty".to_string());
    }
    lidlesseye::core::TenantId::parse(tenant).map_err(|err| err.to_string())?;
    Ok((token.trim().to_string(), tenant.trim().to_string()))
}

/// `--tenant-limit <tenant>:<field>=<value>`.
///
/// Split into three parts here so a typo (`retentoin`) fails at startup with a
/// message naming the field, rather than being silently ignored.
fn parse_tenant_limit(input: &str) -> Result<(String, String, String), String> {
    let (tenant, rest) = input
        .split_once(':')
        .ok_or_else(|| format!("expected <tenant>:<field>=<value>, got `{input}`"))?;
    let (field, value) = rest
        .split_once('=')
        .ok_or_else(|| format!("expected <field>=<value> after the tenant, got `{rest}`"))?;
    let tenant = tenant.trim();
    let field = field.trim().to_ascii_lowercase().replace('-', "_");
    let value = value.trim();
    if tenant.is_empty() || value.is_empty() {
        return Err(format!("`{input}` has an empty tenant or value"));
    }
    match field.as_str() {
        "retention" | "max_disk" | "max_series" => {}
        other => {
            return Err(format!(
                "unknown tenant limit `{other}`; expected `retention`, `max_disk` or `max_series`"
            ));
        }
    }
    Ok((tenant.to_string(), field, value.to_string()))
}

/// Fold the repeated `--tenant-limit` flags into one `TenantLimits` per tenant.
fn assemble_tenant_limits(
    raw: &[(String, String, String)],
) -> anyhow::Result<std::collections::BTreeMap<String, lidlesseye::core::TenantLimits>> {
    let mut out: std::collections::BTreeMap<String, lidlesseye::core::TenantLimits> =
        std::collections::BTreeMap::new();
    for (tenant, field, value) in raw {
        let id = lidlesseye::core::TenantId::parse(tenant)?;
        let entry = out.entry(id.to_string()).or_default();
        match field.as_str() {
            "retention" => {
                entry.retention_ns = parse_optional_duration(value)?.map(|d| d.as_nanos() as u64);
            }
            "max_disk" => entry.max_disk_bytes = parse_optional_bytes(value)?,
            "max_series" => {
                entry.max_series = if value == "0" {
                    None
                } else {
                    Some(value.parse::<usize>().with_context(|| {
                        format!("invalid max_series `{value}` (expected a whole number)")
                    })?)
                };
            }
            _ => unreachable!("validated by parse_tenant_limit"),
        }
    }
    Ok(out)
}

fn parse_duration(input: &str) -> anyhow::Result<Duration> {
    let nanos = lidlesseye::core::parse_duration_ns(input)
        .map_err(|err| anyhow::anyhow!("invalid duration `{input}`: {err}"))?;
    Ok(Duration::from_nanos(nanos))
}

fn parse_optional_duration(input: &str) -> anyhow::Result<Option<Duration>> {
    if input.trim() == "0" {
        return Ok(None);
    }
    Ok(Some(parse_duration(input)?))
}

fn parse_optional_bytes(input: &str) -> anyhow::Result<Option<u64>> {
    let input = input.trim();
    if input == "0" {
        return Ok(None);
    }
    let split = input
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(input.len());
    let (number, unit) = input.split_at(split);
    let value: f64 = number
        .parse()
        .with_context(|| format!("invalid size `{input}`"))?;
    let multiplier: f64 = match unit.trim().to_ascii_lowercase().as_str() {
        "b" | "" => 1.0,
        "kb" => 1e3,
        "mb" => 1e6,
        "gb" => 1e9,
        "tb" => 1e12,
        "kib" => 1024.0,
        "mib" => 1024.0 * 1024.0,
        "gib" => 1024.0 * 1024.0 * 1024.0,
        "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        other => bail!("unknown size unit `{other}`"),
    };
    Ok(Some((value * multiplier) as u64))
}

fn resolve_time(input: &str, now: u64) -> anyhow::Result<u64> {
    let input = input.trim();
    if input.eq_ignore_ascii_case("now") {
        return Ok(now);
    }
    if let Some(rest) = input.strip_prefix("now-") {
        let delta = lidlesseye::core::parse_duration_ns(rest)?;
        return Ok(now.saturating_sub(delta));
    }
    if input.chars().all(|c| c.is_ascii_digit()) && input.len() <= 10 {
        // Bare durations like `30` are seconds in the past, matching the
        // `--from 1h` shorthand.
        let secs: u64 = input.parse()?;
        return Ok(now.saturating_sub(secs * 1_000_000_000));
    }
    if input.ends_with(|c: char| c.is_ascii_alphabetic())
        && input.chars().any(|c| c.is_ascii_digit())
    {
        let delta = lidlesseye::core::parse_duration_ns(input)?;
        return Ok(now.saturating_sub(delta));
    }
    lidlesseye::server::parse_timestamp(input, now)
        .map_err(|err| anyhow::anyhow!("invalid timestamp `{input}`: {err}"))
}
