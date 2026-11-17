<!-- Thanks for the patch. The checklist is what a reviewer will look for; a
     short description beats a long one. -->

## What changed

<!-- The behaviour before and after, in two or three sentences. -->

## Why

<!-- The failure this fixes, the question this lets someone answer, or the
     constraint this removes. Link an issue if there is one. -->

## How it was checked

<!-- Which gates you ran. Delete what does not apply, keep the rest. -->

- [ ] `make lint` (clippy `-D warnings`, `cargo fmt --check`)
- [ ] `make test`
- [ ] `./scripts/ui-smoke.sh`
- [ ] `helm template` / installed against a cluster

## Checklist

- [ ] The change has a test at the level it lives at (unit, integration, or
      `web/test/smoke.cjs` for anything that renders).
- [ ] User-visible changes have a `CHANGELOG.md` entry under *Unreleased*.
- [ ] Documentation that describes this behaviour is updated: `docs/api.md`,
      `docs/ui.md`, `docs/deployment.md` or `docs/architecture.md`.
- [ ] Dashboard changes follow the rules in `docs/ui.md` — one frame per card,
      native controls, selection recolours only, relationship shown by depth
      before linework.
- [ ] No unrelated reformatting or renames are mixed in.
