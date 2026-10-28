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

The machine list in the web UI shows how far behind each machine is.

## Reboot

```sh
mnix reboot web-1
```

The machine must be online. The command waits until the machine is back up
and its agent reconnects. Reboots show up in the deployment history.
