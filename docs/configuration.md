# Configuration

Common setups for the server and the agent. Every option is documented in the
modules: [server](../nix/modules/server-common.nix),
[agent](../nix/modules/agent-common.nix).

## Server

### Database

SQLite is the default. To use a local Postgres instead:

```nix
services.manage-nix.server.database.createLocally = true;
```

For a remote Postgres, put the URL in an environment file so the password
stays out of the Nix store:

```nix
services.manage-nix.server.environmentFile = "/run/secrets/mnix-env";
# /run/secrets/mnix-env:
# MNIX_DATABASE_URL=postgres://user:password@db.example.com/mnix
```

### Secrets

The server generates its secrets on first start. To manage them yourself (for
example with agenix or sops-nix):

```nix
services.manage-nix.server = {
  adminTokenFile = "/run/secrets/mnix-admin-token";
  signingKeyFile = "/run/secrets/mnix-signing-key";  # nix key generate-secret
  initialAdminPasswordFile = "/run/secrets/mnix-admin-password";
};
```

### Users and passwords

Change your password on the account page. To reset a password or add a user
from the shell:

```sh
echo 'new password' | sudo -u manage-nix \
  MNIX_DATABASE_URL=sqlite:///var/lib/manage-nix/db.sqlite \
  mnix-server set-password alice
```

### Listening on a private network

Keep the web UI on loopback, but let agents connect over a VPN:

```nix
services.manage-nix.server.extraListenAddresses = [ "100.64.0.1" ];
```

Agents can then use `http://100.64.0.1:8080`. Use plain HTTP only on a
private network: it sends the machine's credential unencrypted.

To test the web UI without HTTPS, also set `secureCookies = false`.

### Declaring repositories and machines

Instead of running `mnix repo add` and `mnix machine add`, declare them on the
server:

```nix
services.manage-nix.server = {
  repositories.infra = {
    url = "git@github.com:me/infra.git";
    sshKeyFile = "/run/secrets/mnix-deploy-key";
  };

  machines.web-1 = {
    repository = "infra";
    tags = [ "web" ];
    # A pre-shared token (openssl rand -hex 32). The agent uses the same file.
    enrollmentTokenFile = "/run/secrets/mnix-web-1-token";
  };
};
```

With a pre-shared token, the machine enrolls by itself the first time it
starts. Things you add by hand are kept.

### Building on the machine

By default the server builds every system, which needs a cache. Instead, the
server can send each machine the source and let it build its own system:

```nix
# On the server
services.manage-nix.server.buildOn = "machine";

# On the machine
services.manage-nix.agent.servers.main = {
  url = "https://mnix.example.com";
  allowBuilds = true;
};
```

No cache and no signing key are needed. This also lets a macOS server deploy
NixOS machines and the other way round.

> `allowBuilds` lets the server run any configuration it chooses on the
> machine. Only enable it for servers you trust as much as root.

## Agent

### Health checks

After every activation the agent waits for these checks to pass. If they don't
pass within the timeout, it rolls back.

```nix
services.manage-nix.agent.healthChecks = {
  timeout = 60;                               # seconds
  systemdUnits = [ "nginx.service" ];         # must be active (NixOS only)
  http = [ "http://127.0.0.1:8080/healthz" ]; # must answer 2xx
  commands.database = "pg_isready --quiet";   # must exit 0
};
```

### Several servers

A machine can be managed by more than one server. Each one enrolls the machine
on its own, and they don't need to reach each other:

```nix
services.manage-nix.agent.servers = {
  home = {
    url = "https://mnix.home.example.com";
    enrollmentTokenFile = "/run/secrets/mnix-home-token";
  };
  work.url = "https://mnix.work.example.com";
};
```

`server.url` and `enrollmentTokenFile` are a shorthand for a single server
named `default`.

### Trusted keys

The agent only activates systems signed by a key in `trustedPublicKeys`. These
are separate from `nix.settings.trusted-public-keys` on purpose: trusting
cache.nixos.org for packages doesn't mean trusting it to pick your system.

## macOS

Use `darwinModules.server` and `darwinModules.agent`. They run under launchd
and support everything above, except `--mode boot` and `systemdUnits` health
checks.
