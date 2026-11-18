# Contributing

Thanks for taking the time to look at this. Issues and pull requests are both
welcome; a clear bug report with the command that produced it is as useful as a
patch.

## Before you start

- **Linux** is required to run the agent (kernel ≥ 5.5 with BTF, root or
  `CAP_BPF` + `CAP_PERFMON` + `CAP_SYS_ADMIN`, `perf_event_paranoid` ≤ 2).
  Building and testing the dashboard needs Node 20.19+.
- **Rust 1.98** is the pinned toolchain (`rust-version` in `Cargo.toml`); the
  release image in `deploy/docker/Dockerfile` uses the same one.
- The dashboard is embedded into the binary at compile time, so `cargo build`
  needs a dashboard export first. `make` targets do this for you; plain `cargo`
  does not.

```bash
make build          # exports the dashboard, then builds the workspace
make test           # unit, doc and integration tests
make lint           # clippy -D warnings, then cargo fmt --check
./scripts/ui-smoke.sh   # Chromium against the real binary (needs Docker)
```

`./scripts/demo.sh` starts the binary in Docker with seeded data and prints the
URL — the fastest way to look at the UI without a Linux host.

## What a change should carry

1. **A reason.** The pull request description says what was wrong and why this
   is the fix. If the change is invisible to users (a refactor), say what it
   makes possible.
2. **A test at the level the change lives at.** A parsing bug gets a unit test
   in the same module; an HTTP shape gets an integration test under
   `crates/lidlesseye-server/tests/`; anything that renders gets an assertion in
   `web/test/smoke.cjs`, which fails on any console error.
3. **Green gates.** `make lint` and `make test` locally, and the three CI jobs
   (dashboard, Rust, Helm chart) on the pull request. Don't mark a draft as
   ready with a red job unless you explain why in the description.
4. **Documentation where the behaviour is described.** `docs/api.md` for an
   endpoint, `docs/ui.md` for anything a person sees, `docs/deployment.md` for a
   chart or storage option, `docs/architecture.md` for a design decision.
   `CHANGELOG.md` gets an entry under *Unreleased* for user-visible changes.

## Front-end changes

The dashboard follows the
[Chrome Modern Web Guidance](https://github.com/GoogleChrome/modern-web-guidance-src)
and the rules collected in [docs/ui.md](docs/ui.md). The short version:

- Semantics and interaction are chosen separately — a card is an `<article>`
  with an `<h3>` and a `<ul>`; the whole-card action is a stretched native
  `<button>`, never a `div` with a key handler.
- A card draws **one** frame. Its border and tint already group what is inside,
  so no second hairline under the header, no borders on rows, and never two
  rules a pixel apart for one boundary.
- Selection changes colour only. Border width, padding and font weight stay put,
  so nothing jumps when a node becomes selected.
- Relationship is carried by depth, translucency and space before it is carried
  by linework.
- Every page keeps one `<h1>` (a visually hidden one is fine), a focusable
  `<main>` and a skip link; motion is `motion-safe:`.

## Commit messages

Subject in the imperative mood, ≤ 72 characters, no type prefix
(`Fix the archive merge…`, not `fix: …`). The body explains *why*: what the old
behaviour was, what it cost, and what the new one guarantees. Single-purpose
commits; a rename that touches forty files belongs in its own commit, not mixed
into a behaviour change.

## Releasing

Maintainers only. A release is a tag whose version matches `Cargo.toml`,
`deploy/helm/lidlesseye/Chart.yaml` and `web/package.json`, with a matching
section in `CHANGELOG.md`:

```bash
git tag v0.1.0 && git push origin v0.1.0
```

The `Release` workflow checks those four numbers, extracts the changelog
section, packages the chart and publishes the release. The `Image` workflow
builds `deploy/docker/Dockerfile` once, pushes it to GHCR, and copies the same
manifest to Docker Hub, which is the registry the chart points at. Docker Hub
publishing needs two repository secrets — `DOCKERHUB_USERNAME` and
`DOCKERHUB_TOKEN` (a Docker Hub access token, not the account password); without
them the image job warns and publishes to GHCR only, leaving the chart's default
`image.repository` unpullable for that tag. The chart has no `image.tag` of its
own: it resolves to `appVersion`, so a release cannot hand out a chart whose
image is a different version.

## Reporting a security issue

Please don't open a public issue for a vulnerability — see [SECURITY.md](SECURITY.md).
