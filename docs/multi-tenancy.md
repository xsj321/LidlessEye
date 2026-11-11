# Multi-tenancy

A tenant is a hard isolation boundary. It owns a directory, an index, a
retention policy and a set of limits. Nothing on the query path can reach across
tenants, because a query only ever holds the store of the tenant it was resolved
for — isolation is structural, not a filter applied at the end.

## Layout

```
<data-dir>/
  tenants/
    acme/
      cpu/
        checkout/
          1700000000000000000-1700000010000000000.leseg
      off_cpu/
        checkout/…
    globex/
      cpu/…
```

`<data-dir>/<profile-type>/<service>/…` — the layout used before multi-tenancy
existed — is migrated once into `tenants/default/` on first start, so an upgrade
keeps its history. The migration only touches directories that look like the old
layout, and it is idempotent.

Directory names are slugs: everything outside `[A-Za-z0-9._-]` becomes `_`,
trailing dots and spaces are trimmed (Windows strips them silently), reserved
device names (`CON`, `NUL`, `COM1`, …) are prefixed, and a component can never
contain or start with `..`. A hostile `service_name` of `../../etc/passwd`
therefore lands in a directory called `.._.._etc_passwd`… which is then further
reduced to `_.._etc_passwd`; either way it stays inside the tenant.

## Naming a tenant

| Situation | Tenant |
| --- | --- |
| `X-Scope-OrgID: acme` | `acme` |
| No header, no auth | `default` |
| No header, tenant-scoped token | the token's tenant |
| Empty header | `400` |
| Two headers with different values | `400` |
| Id with `/`, space, `..`, leading `.`, > 64 bytes | `400` |

Ids are **case-insensitive** and stored lowercased. This is not cosmetic: the id
becomes a directory name, and both NTFS (Windows) and default APFS (macOS) fold
case. Keeping `Acme` and `acme` distinct in memory would point both at one
directory — two tenants silently sharing a store. Normalizing at the boundary
makes memory and disk agree on every platform.

`--tenant-header` changes the header name; `x-tenant-id` is always accepted as an
alias.

## Authentication models

**Open** (default). No token; anyone who can reach the port chooses the tenant.
Only appropriate behind a network boundary or for local development.

**Global token.** `--auth-token` (or `LIDLESSEYE_AUTH_TOKEN`). One token, all
tenants; the caller picks the tenant with the header. This is the Pyroscope
model.

**Per-tenant tokens.** `--tenant-token <token>=<tenant>`, repeatable. The token
*is* the tenant: the header may be omitted, and naming a different tenant in the
header is a `403` rather than a silent rewrite. A global token may still be
configured alongside these, for operators who need cross-tenant access.

**Allow-list.** `--allowed-tenant acme --allowed-tenant globex` restricts which
tenants may be addressed at all, on reads and writes. Requests for anything else
are `403`. Without it, the first successful write creates the tenant.

Tokens are compared in constant time, and an authorization header of the wrong
scheme (`Basic`, `bearer`, a bare token) never authenticates.

## Limits

Each limit is per tenant, and each has a server-wide default:

| Limit | Flag | Effect |
| --- | --- | --- |
| Retention | `--retention 7d` | segments whose window ends before `now - retention` are deleted |
| Disk cap | `--max-disk 50GB` | the oldest segments are evicted until the tenant fits |
| Series cap | `--tenant-max-series N` | a write that would add a **new** series beyond `N` is rejected with `429`; existing series keep working |
| Tenant count | `--max-tenants N` | creating more than `N` tenants is `429` |

Per-tenant overrides live in `ServerConfig::tenant_limits` when embedding:

```rust
use lidlesseye_core::TenantLimits;

let mut limits = std::collections::BTreeMap::new();
limits.insert("acme".to_string(), TenantLimits {
    retention_ns: Some(30 * 86_400_000_000_000), // 30 days
    max_disk_bytes: Some(200 * 1024 * 1024 * 1024),
    max_series: Some(50_000),
});

let eye = LidlessEye::builder()
    .data_dir("/var/lib/lidlesseye")
    .tenant_limits(limits)
    .build()
    .await?;
```

`None` means "inherit the server default", so one global retention plus a single
override is a complete configuration.

A rejected write is atomic: a batch that would exceed the series cap stores
*nothing*, so a retry cannot duplicate the reports that did fit.

The disk cap is fixed when a tenant's store is opened, so lowering it takes
effect on the next start; retention changes apply on the next prune pass.

## Querying one tenant

Every query, ingest and maintenance call is scoped:

```bash
curl -s -H 'X-Scope-OrgID: acme' \
  'http://localhost:4040/api/v1/flamegraph?profile_type=cpu&from=now-1h'
```

`GET /api/v1/tenants` lists tenants with their size and limits — an operator
tool. A tenant-scoped token sees only its own entry.

`lidlesseye query --tenant acme`, `lidlesseye ingest --tenant acme` and
`lidlesseye fsck --tenant acme` do the same from the CLI.

Metrics are intentionally tenant-agnostic: no Prometheus label carries a tenant
id, because that would let any scraper enumerate customers.

## Pruning

Retention and eviction run per tenant on the prune interval (default 60 s) and
on demand via `TenantStore::prune`. Both are safe to run concurrently with
ingestion: the index is updated under a write lock and files are removed before
the index entry disappears, so a query either sees a segment or does not see it
at all.

## Failure behaviour

| Symptom | Cause | What to do |
| --- | --- | --- |
| `400 invalid tenant` | id failed validation | check the header for spaces, `/`, or a leading `.` |
| `403 this token is scoped to tenant …` | per-tenant token, wrong header | drop the header, or use that tenant's token |
| `429 … series limit` | new series beyond `max_series` | raise the limit, or send the new series under an existing label set |
| `429 the server already hosts N tenants` | tenant cap | raise `--max-tenants`, or delete an unused tenant directory |
| `404 tenant … has no data` | read before any write | ingest first; reading never creates a tenant |
