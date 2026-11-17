# Security policy

## Supported versions

The `0.1.x` series is the current release line; fixes go there first. Older
tags are not maintained.

| Version | Supported |
| --- | --- |
| 0.1.x | yes |

## Reporting a vulnerability

**Do not open a public issue.** Use GitHub's private reporting:
[Security → Report a vulnerability](https://github.com/xsj321/LidlessEye/security/advisories/new),
or email the maintainer listed on
[the repository's people page](https://github.com/xsj321/LidlessEye/graphs/contributors)
if you cannot use advisories.

Please include the version (`lidlesseye version`), the deployment shape
(bare metal or Helm, tenant mode, storage mode) and the smallest reproduction
you have. We aim to acknowledge within three working days and to ship a fix or
a mitigation within thirty days of a confirmed issue.

## What is in scope

LidlessEye runs a **privileged** component by design, so the interesting
failures are not only memory-safety bugs:

- **The agent** loads eBPF programs and reads `/proc`, `/sys/kernel/btf` and
  host binaries. It runs as root with `hostPID` in the Helm chart. A flaw that
  lets a local process feed it crafted input (a fake BTF file, an oversized
  stack, a hostile `/proc` entry) and gain code execution or a kernel fault is
  in scope.
- **The ingest path** accepts reports with stacks, strings and symbols from any
  agent. Parsing that data is untrusted input: panics, unbounded allocation or
  path traversal from a label are all bugs.
- **Tenant isolation.** `X-Scope-OrgID` decides which store a request reads.
  Any way to read or write another tenant's data — through a query parameter, a
  crafted selector, a shared DuckDB file or a Parquet object key — is a
  vulnerability, not a missing feature.
- **Storage backends.** S3 credentials, object keys and the local Parquet cache
  must not be reachable through the HTTP API.
- **The dashboard** is served from the same origin as the API. Reflected XSS in
  a label name or a frame name counts.

Out of scope: the consequences of running the agent with more privilege than it
needs, alerts from a scanner against the demo container, and the fact that
`/api/v1/*` is unauthenticated when `server.auth.enabled=false` — that is a
documented deployment choice.

## Hardening checklist

If you are deploying this, the operator-facing advice lives in
[docs/deployment.md](docs/deployment.md): enable tenant auth, keep the agent's
`hostPID` scope, restrict the API to the cluster network, and point storage at a
bucket whose credentials can only write the configured prefix.
