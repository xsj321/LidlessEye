# Documentation

| Document | Read it when you want to… |
| --- | --- |
| [ui.md](ui.md) | use the dashboard: time-range histogram, label filters, compare view, URL deep links |
| [architecture.md](architecture.md) | understand the design: crates, data flow, DuckDB schema, query pipeline, concurrency, benchmarks |
| [api.md](api.md) | call the HTTP API: endpoints, parameters, payloads, status codes, metrics |
| [deployment.md](deployment.md) | run it: one host, server + agents, Docker, Kubernetes/Helm, privileges, sizing, monitoring, troubleshooting |
| [embedding.md](embedding.md) | embed the server in a Rust program, write a custom collector, ingest without the CLI |
| [multi-tenancy.md](multi-tenancy.md) | isolate tenants: layout, naming, tokens, limits, pruning, failure behaviour |
| [compatibility.md](compatibility.md) | compare with Pyroscope: what matches, what differs, what is out of scope |

The root [README](../README.md) is the tour and the quick start; the CLI is
documented there too.

`基本设计.md` at the repository root is the original brief (in Chinese) — the
requirements this project was built against. This directory is the maintained
documentation; where the two disagree, these documents describe what the code
does.
