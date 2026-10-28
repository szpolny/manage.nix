# CLI reference

`mnix` talks to the server's API. The server module installs it.

## Connecting

| Flag | Environment variable | Default |
|---|---|---|
| `--server <url>` | `MNIX_SERVER` | `http://127.0.0.1:8080` |
| `--token-file <path>` | `MNIX_TOKEN_FILE` | none |

The admin token lives in `/var/lib/manage-nix/admin-token` unless you set
`adminTokenFile`. Run `mnix --help` or `mnix <command> --help` for every flag.

## Machines

| Command | What it does |
|---|---|
| `mnix machines` | List machines with their status and current system |
| `mnix machine add <name>` | Add a machine and print its enrollment token |
| `mnix machine token <machine>` | Print a new enrollment token |
| `mnix machine show <machine>` | Show details and facts as JSON |
| `mnix machine configure <machine> --repo <repo>` | Build the machine from a repository |
| `mnix machine tag <machine> <tag>...` | Add tags |
| `mnix machine untag <machine> <tag>...` | Remove tags |

## Repositories

| Command | What it does |
|---|---|
| `mnix repos` | List repositories |
| `mnix repo add <name> <url>` | Add a repository |

## Deployments

| Command | What it does |
|---|---|
| `mnix deploy <machine or tag:name>... [--rev <rev>]` | Build and activate a revision |
| `mnix rollback <machine>` | Go back to the previous system |
| `mnix reboot <machine>` | Restart a machine and wait for it |
| `mnix updates <machine> [--preview]` | Show commits the machine is missing |
| `mnix deploys` | List recent deployments |
| `mnix deploys show <id> [--logs]` | Show one deployment |
| `mnix deploys logs <id> [--follow]` | Print a deployment's log |
| `mnix signing-key` | Print the key machines must trust |

`deploy`, `rollback`, and `reboot` follow the log until they finish. Pass
`--no-wait` to return right away.

Most list commands accept `--json`.

## Agent commands

These run on a managed machine:

| Command | What it does |
|---|---|
| `mnix-agent facts` | Print what the machine runs, as JSON |
| `mnix-agent activate <store-path> [--mode switch\|boot\|test]` | Verify, activate, health-check, and roll back if needed |
