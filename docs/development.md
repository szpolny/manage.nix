# Development

## Getting started

```sh
nix develop          # Rust, sqlx-cli, Postgres, attic-client, Node.js
cargo test           # unit tests (store tests use SQLite)
nix flake check -L   # everything CI runs
```

To run the whole thing locally:

```sh
process-compose
```

This starts `mnix-server` from source (state in `.dev/`) and the web UI with
hot reload at <http://localhost:5173>. Log in as `admin` / `dev-password`.

## Repository layout

| Path | Contents |
|---|---|
| `crates/server` | `mnix-server`: API, enrollment, builds, deployments |
| `crates/agent` | `mnix-agent`: enrollment, activation, rollback, facts |
| `crates/cli` | `mnix`: command-line client |
| `crates/protocol` | Messages shared by agent and server |
| `crates/nix` | Store paths and signatures |
| `crates/store*` | The `Store` trait, its SQLite and Postgres backends, and their shared test suite |
| `migrations/` | sqlx migrations, numbered the same for both backends |
| `web/` | Web UI (SvelteKit, Svelte 5) |
| `nix/modules` | NixOS and nix-darwin modules |
| `nix/tests` | NixOS VM tests |

## Web UI

- `npm run dev` on its own proxies `/api` to a server on `127.0.0.1:8080`.
  Start that server with `--insecure-cookies` so the session cookie works over
  plain HTTP.
- `npm run check` type-checks the UI. The Nix build runs it too.
- To add a dependency, run `npm install`. Nix reads every package from
  `package-lock.json`.

## Database queries

Queries use sqlx's checked macros against the committed offline data in
`crates/store-sqlite/.sqlx` and `crates/store-postgres/.sqlx`. After changing
a query or a migration, run this in the dev shell and commit the result:

```sh
scripts/sqlx-prepare
```

Store tests also run against Postgres when `MNIX_TEST_POSTGRES_URL` points to
a server where the user may create databases. The `store-postgres` flake check
sets this up for you.

## VM tests

`checks.*.vm-*` are NixOS VM tests. `vm-web` drives the UI with Playwright
(`nix/tests/web/e2e.py`).

They run on Linux with KVM, and natively on Apple Silicon. On macOS, that
needs the `apple-virt` feature and a Linux builder:

```nix
nix.settings.system-features = [ "apple-virt" "benchmark" "big-parallel" "nixos-test" ];
nix.linux-builder.enable = true;
```

Don't run `checks.aarch64-linux.vm-*` inside the Linux builder on a Mac: without
nested virtualization QEMU falls back to emulation and the tests time out.

## Releases

Commits follow [Conventional Commits](https://www.conventionalcommits.org).
release-please opens a release PR; merging it tags `vX.Y.Z-alpha.N` and
publishes a GitHub pre-release. Every release is an alpha for now.

`checks.*.versions` fails if the versions in `Cargo.toml`, `Cargo.lock`,
`web/package*.json`, `version.txt`, and the release-please manifest disagree.
