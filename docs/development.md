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

For Cargo builds, set `MNIX_BUILD_REVISION` at compile time to include a source
revision in the About view. Nix package builds set this automatically. For example:

```sh
MNIX_BUILD_REVISION=$(git rev-parse HEAD) cargo build -p mnix-server
```

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

On every pull request and push to main, CI runs formatting, Clippy, Rust and
Postgres tests, version consistency, and all package builds (including the
web type check). VM tests are selected from the changed paths:

- Documentation and version-only release changes skip VMs; web changes run
  `vm-web`. Release metadata is compared structurally: dependency, build, and
  other non-version changes still select VMs.
- Changes to individual VM tests run those scenarios, including both database
  backends where applicable. Shared deployment fixtures run deployment, web,
  and fleet scenarios.
- Shared code, modules, flake inputs, CI configuration, and unrecognized paths
  run all five core scenarios: activation, SQLite deployment, web, fleet, and local HTTPS.
- Weekly and manual runs execute all nine VM scenarios.

New commits cancel older runs on the same pull request. Main branch runs execute
independently so an older run cannot block newer commits, and finish so they can
populate Cachix. Build and VM jobs time out after 20 minutes. Configure the repository
variable `CACHIX_NAME`
(`manage-nix`) and secret `CACHIX_AUTH_TOKEN` to enable cache reads in all jobs
and uploads on pushes to main. Each runner has its own Nix store; Cachix lets
later runs reuse outputs published by previous main branch runs.

The build job checks formatting, versions, and Darwin modules before compiling
Rust dependencies and all packages once. It exports separate Zstandard-compressed
binary caches as one-day workflow artifacts. Rust checks import the dependencies;
VM jobs import the packages, then run in parallel with the Rust checks. These
artifacts are scoped to the same workflow run, including on fork pull requests,
and need no cache upload credentials. Package artifacts are skipped when no VMs
are selected. The local artifact imports disable signature checks because they
contain this run's unsigned builds; external caches still require signatures.

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
