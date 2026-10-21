# manage.nix

A dashboard and control plane for fleets of Nix-managed machines.

## Layout

| Path | Contents |
|---|---|
| `crates/protocol` | Message types shared by agent and server |
| `crates/store` | `Store` trait with SQLite and Postgres implementations |
| `crates/agent` | `mnix-agent`: activation, rollback, facts |
| `crates/server` | `mnix-server`: API, scheduler, builds |
| `crates/cli` | `mnix`: command-line client |
| `migrations/{sqlite,postgres}` | sqlx migrations, same numbering in both |
| `nix/modules` | NixOS modules (`nixosModules.agent`, `nixosModules.server`) |
| `nix/tests` | NixOS VM tests, one per milestone |
| `web/` | SvelteKit UI (from M4) |

## Development

```sh
nix develop          # Rust toolchain, sqlx-cli, Postgres, attic-client, Node.js
cargo test           # unit tests; store tests run against SQLite
nix flake check -L   # everything CI runs
```

Store tests also run against Postgres when `MNIX_TEST_POSTGRES_URL` points
at a server where the user may create databases (each test creates its own
database). The `store-postgres` flake check starts a throwaway server and
sets this for you.

NixOS VM tests (`checks.<linux-system>.vm-*`) need a Linux builder that
advertises the `kvm` and `nixos-test` system features. `nix flake check` on
macOS only runs the darwin checks; run the Linux ones explicitly, for example
`nix build .#checks.aarch64-linux.vm-hello -L`. With nix-darwin:

```nix
nix.linux-builder = {
  enable = true;
  supportedFeatures = [ "kvm" "benchmark" "big-parallel" "nixos-test" ];
};
```

SQL queries use sqlx's checked macros against committed `.sqlx/` offline
data (`SQLX_OFFLINE=true` in Nix builds and the dev shell). M0 has no
checked queries yet, so `.sqlx/` arrives with the first one in M2.
