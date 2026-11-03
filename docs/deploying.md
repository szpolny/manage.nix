# Deploying

Every command below runs on the server. To save typing, set the token file
once:

```sh
export MNIX_TOKEN_FILE=/var/lib/manage-nix/admin-token
```

Everything here also works from the web UI.

## Connect a repository

manage.nix builds machines from a Git repository that contains a flake.

```sh
mnix repo add infra https://git.example.com/infra.git
mnix machine configure web-1 --repo infra
```

`web-1` is now built from `nixosConfigurations.web-1` (or
`darwinConfigurations.web-1` on macOS). Use `--configuration <name>` if the
flake calls it something else.

For a private repository, give the server a read-only SSH deploy key with
[`repositories.<name>.sshKeyFile`](configuration.md#declaring-repositories-and-machines).
To [update nixpkgs](#update-nixpkgs) from manage.nix, the key needs write
access.

## Deploy

```sh
mnix deploy web-1 --rev main
```

The command follows the log and exits with `0` only if every machine
succeeded. Leave out `--rev` to deploy the repository's default branch.

### Deploy to many machines with tags

```sh
mnix machine tag web-1 web
mnix machine tag web-2 web
mnix deploy tag:web --rev v1.4.0
```

### Activation modes

| `--mode` | What it does |
|---|---|
| `switch` (default) | Activate now and make it the boot default |
| `test` | Activate now, but keep the old boot default |
| `boot` | Only make it the boot default (NixOS only) |

## What happens during a deployment

1. The server fetches the revision and builds the system.
2. It signs the result and pushes it to the cache.
3. The machine checks the signature, downloads the system, and activates it.
4. The machine runs its [health checks](configuration.md#health-checks).
5. The machine reconnects to the server over a **new** connection.

If the health checks fail, or the machine can't reach the server within 60
seconds, it **rolls back on its own**. A bad firewall rule or a broken network
config can't lock you out.

## History and rollback

```sh
mnix deploys                    # recent deployments
mnix deploys show <id> --logs   # one deployment, with its log
mnix rollback web-1             # back to the previous system generation
```

A rollback runs the same health checks as a deployment.

## Updates

See which commits a machine is missing:

```sh
mnix updates web-1
```

Add `--preview` to build the new system on the server and see which packages
would change, without deploying anything:

```sh
mnix updates web-1 --rev main --preview
```

The machine list in the web UI shows how far behind each machine is, and
whether nixpkgs or Flatpak updates are available. The nixpkgs check compares
the `nixpkgs` input locked by the running commit's `flake.lock` with its
upstream branch; it works for GitHub inputs. The Flatpak check covers the
system installation and each user's, and needs an up-to-date agent with
`services.flatpak.enable`. Both refresh every 15 minutes.

### Update nixpkgs

```sh
mnix deploy web-1 --update-nixpkgs
```

The server runs `nix flake update nixpkgs` on the branch (`--rev`, or the
default branch), commits the new `flake.lock` as `manage.nix`, pushes it to
the branch, and then deploys that commit as usual. If nixpkgs is already
current, it deploys the branch as it is. The push is never forced: if the
branch moved since the server fetched it, the deployment fails and nothing is
deployed. `--rev` must name a branch, and the repository's SSH key needs
write access. In the web UI, use **Update nixpkgs…** on the machine's page,
or tick **Update nixpkgs first** when deploying.

### Update Flatpaks

```sh
mnix flatpak-update web-1
```

The agent runs `flatpak update` for the system installation and for each
user's. The machine must be online. The update shows up in the deployment
history with its output in the log.

### Automatic updates

```sh
mnix machine auto-update web-1 --nixpkgs true --flatpak true
```

Or tick **daily** next to nixpkgs or Flatpak on the machine's page. The
server then checks every 15 minutes and starts an update when one is
available, the machine is online, and nothing else is deploying to it. Each
kind runs at most once a day per machine, even if it failed, and shows up in
the history as requested by `auto-update`. Automatic nixpkgs updates deploy
the branch the machine was last deployed from, with `switch`.

## Reboot

```sh
mnix reboot web-1
```

The machine must be online. The command waits until the machine is back up
and its agent reconnects. Reboots show up in the deployment history.
