# manage.nix

A dashboard and control plane for fleets of Nix-managed machines.

## Layout

| Path | Contents |
|---|---|
| `crates/protocol` | Message types shared by agent and server |
| `crates/nix` | Store paths and signatures (signing on the server, verifying on the agent) |
| `crates/store` | Storage facade: `connect(url)` and the shared test suite |
| `crates/store-core` | The `Store` trait and records |
| `crates/store-sqlite`, `crates/store-postgres` | The two backends, each with its sqlx offline data in `.sqlx/` |
| `crates/agent` | `mnix-agent`: enrollment, the server connection, activation, rollback, facts |
| `crates/server` | `mnix-server`: API, enrollment, builds, deployments |
| `crates/cli` | `mnix`: command-line client |
| `scripts/sqlx-prepare` | Regenerates the sqlx offline data for both backends |
| `migrations/{sqlite,postgres}` | sqlx migrations, same numbering in both |
| `nix/modules` | NixOS modules (`nixosModules.agent`, `nixosModules.server`) |
| `nix/tests` | NixOS VM tests, one per milestone |
| `web/` | The web UI: a SvelteKit single-page app (`packages.mnix-web`) |

## Control plane

`mnix-server serve` runs the HTTP API and the agent WebSocket endpoint on
plain HTTP (`127.0.0.1:8080` by default); put a TLS reverse proxy in front of
it. `nixosModules.server` runs it with SQLite by default, or with a local
Postgres (`services.manage-nix.server.database.createLocally = true;`).

```sh
mnix --token-file /var/lib/manage-nix/admin-token machine add web-1  # prints a one-time token
mnix --token-file /var/lib/manage-nix/admin-token machines           # status and current system
```

On the machine, set `services.manage-nix.agent.server.url` and
`enrollmentTokenFile` (a file holding that token). The `mnix-agent` service
enrolls once, stores its credential in `/var/lib/manage-nix`, and stays
connected. The protocol's message types live in `crates/protocol`.

## Web UI

The server serves the web UI at `/` (`nixosModules.server` does by default).
On first start it creates the user `admin` with the password in
`services.manage-nix.server.initialAdminPasswordFile`, or a generated one in
`/var/lib/manage-nix/initial-admin-password`; change it on the account page.
To set a password (or add a user) on the server:

```sh
echo 'new password' | sudo -u manage-nix MNIX_DATABASE_URL=sqlite:///var/lib/manage-nix/db.sqlite \
  mnix-server set-password alice
```

The session cookie is `Secure`, so serve the UI over HTTPS (or set
`secureCookies = false` for a test setup). Disable response buffering for
`/api/deployments/*/events` in the proxy: the deploy page follows logs over
server-sent events.

## Deploying

The server builds each machine from a Git flake
(`nixosConfigurations.<name>`), signs the result, and pushes it to Attic
(`services.manage-nix.server.attic`). Machines need the server's key in
`services.manage-nix.agent.trustedPublicKeys` (`mnix signing-key` prints it)
and the cache in `nix.settings.substituters`.

```sh
mnix repo add infra https://git.example.com/infra.git
mnix machine configure web-1 --repo infra      # builds nixosConfigurations.web-1
mnix machine tag web-1 web
mnix deploy tag:web --rev main                 # follows the log; exit 0 if all succeeded
mnix deploys                                   # history
mnix deploys show <id> --logs
mnix rollback web-1                            # previous system profile generation
```

Each machine activates its system in its own systemd unit, checks its
health, and then must reach the server over a **new** connection within
`confirmTimeout` (60 s); otherwise it rolls back on its own.

## Agent

`mnix-agent activate <store-path> [--mode switch|boot|test]` verifies that a
NixOS system is signed by a key in `services.manage-nix.agent.trustedPublicKeys`,
activates it, runs the health checks the new system declares in
`services.manage-nix.agent.healthChecks`, and rolls back on its own if they
do not pass. It prints a JSON report. `mnix-agent facts` prints what the
machine runs as JSON.

## Development

```sh
nix develop          # Rust toolchain, sqlx-cli, Postgres, attic-client, Node.js
cargo test           # unit tests; store tests run against SQLite
nix flake check -L   # everything CI runs
```

The UI lives in `web/` (SvelteKit 3, Svelte 5). `process-compose` (in the
dev shell, from the repository root) runs `mnix-server` from source with its
state in `.dev/` and the UI with hot reload at <http://localhost:5173>; log
in as `admin` / `dev-password`. On its
own, `npm run dev` proxies `/api` to a server on `127.0.0.1:8080` started
with `--insecure-cookies`, so the browser keeps the session cookie over
plain HTTP. `npm run check`
type-checks it; the flake's `mnix-web` build runs that too. Adding a
dependency only needs `package-lock.json` updated (`npm install`): Nix reads
every package from it.

Store tests also run against Postgres when `MNIX_TEST_POSTGRES_URL` points
at a server where the user may create databases (each test creates its own
database). The `store-postgres` flake check starts a throwaway server and
sets this for you.

`checks.*.vm-web` is the browser test: the deployment test's machines plus a
client VM where Playwright drives Chromium through the UI
(`nix/tests/web/e2e.py`).

NixOS VM tests (`checks.*.vm-*`) run on Linux with KVM, and natively on
Apple Silicon macOS: there the test driver runs QEMU with Apple's
hypervisor and the guests are `aarch64-linux`, so `nix flake check` on an
`aarch64-darwin` machine runs them too. That needs the `apple-virt` system
feature and a Linux builder for the guest systems. With nix-darwin:

```nix
nix.settings.system-features = [ "apple-virt" "benchmark" "big-parallel" "nixos-test" ];
nix.linux-builder.enable = true;
```

Don't run the VM tests *inside* the Linux builder
(`checks.aarch64-linux.vm-*`) on a Mac without nested virtualization: QEMU
falls back to emulation and the multi-machine tests time out.

SQL queries use sqlx's checked macros against committed offline data in
`crates/store-sqlite/.sqlx` and `crates/store-postgres/.sqlx`
(`SQLX_OFFLINE=true` in Nix builds and the dev shell). After changing a query
or a migration, run `scripts/sqlx-prepare` in the dev shell and commit the
result; it prepares each backend against a throwaway database.
