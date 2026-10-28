# Installation

manage.nix has two parts:

- **The server** (`mnix-server`) builds systems, keeps the history, and serves
  the web UI and API. You run one (or a few) of these.
- **The agent** (`mnix-agent`) runs on every managed machine. It connects to the
  server, activates new systems, and rolls back when they are unhealthy.

Both ship as NixOS and nix-darwin modules in this flake.

## 1. Add the flake

```nix
inputs.manage-nix.url = "github:szpolny/manage.nix/v0.3.0-alpha"; # x-release-please-version
```

Pick the latest tag from the
[releases page](https://github.com/szpolny/manage.nix/releases).

### Updating

**Pinned to a release (recommended).** Change the tag in the URL, then run
`nix flake update manage-nix`. Nothing changes until you do this.

**Always the latest.** Leave out the tag to follow the `main` branch:

```nix
inputs.manage-nix.url = "github:szpolny/manage.nix";
```

`nix flake update manage-nix` then pulls the newest commit. To do this
automatically, update `flake.lock` on a schedule, for example with the
[update-flake-lock](https://github.com/DeterminateSystems/update-flake-lock)
GitHub Action. `main` gets changes before they are released, so expect the
occasional breakage.

While manage.nix is in alpha, keep the server and the agents on the same
version.

## 2. Set up the server

```nix
{ manage-nix, ... }:
{
  imports = [ manage-nix.nixosModules.server ];  # or darwinModules.server

  services.manage-nix.server.enable = true;
}
```

This runs the server on `127.0.0.1:8080` with an SQLite database in
`/var/lib/manage-nix`. On first start it generates:

| File | What it is |
|---|---|
| `/var/lib/manage-nix/admin-token` | Token for the `mnix` CLI |
| `/var/lib/manage-nix/initial-admin-password` | Password of the web UI user `admin` |
| `/var/lib/manage-nix/signing-key` | Key the server signs built systems with |

### Put it behind HTTPS

The server speaks plain HTTP. Put a TLS reverse proxy in front of it. With
nginx:

```nix
services.nginx = {
  enable = true;
  virtualHosts."mnix.example.com" = {
    forceSSL = true;
    enableACME = true;
    locations."/" = {
      proxyPass = "http://127.0.0.1:8080";
      proxyWebsockets = true;  # agents connect over a WebSocket
    };
    # Live deployment logs use server-sent events.
    locations."~ ^/api/deployments/.*/events$" = {
      proxyPass = "http://127.0.0.1:8080";
      extraConfig = "proxy_buffering off;";
    };
  };
};
networking.firewall.allowedTCPPorts = [ 80 443 ];
```

> On a private network (for example Tailscale) you can skip the proxy and use
> `extraListenAddresses` instead. See [Configuration](configuration.md#listening-on-a-private-network).

### Log in

Open `https://mnix.example.com`, log in as `admin` with the password from
`initial-admin-password`, and change it on the account page.

## 3. Set up a cache

When the server builds a system, the machine has to download it. The server
pushes every build to an [Attic](https://github.com/zhaofengli/attic) cache:

```nix
services.manage-nix.server.attic = {
  endpoint = "https://cache.example.com/";
  cache = "manage-nix";
  tokenFile = "/run/secrets/attic-push-token";
};
```

> Don't want a cache? Let machines build their own systems instead. See
> [Building on the machine](configuration.md#building-on-the-machine).

## 4. Add a machine

On the server, create the machine and get its one-time enrollment token:

```sh
sudo mnix --token-file /var/lib/manage-nix/admin-token machine add web-1
```

You can also do this from the web UI. The token expires after one hour.

Then print the server's signing key:

```sh
sudo mnix --token-file /var/lib/manage-nix/admin-token signing-key
```

## 5. Set up the agent

On the machine, save the token to a file outside the Nix store (for example
`/run/secrets/mnix-enrollment-token`) and enable the agent:

```nix
{ manage-nix, ... }:
{
  imports = [ manage-nix.nixosModules.agent ];  # or darwinModules.agent

  services.manage-nix.agent = {
    enable = true;
    server.url = "https://mnix.example.com";
    enrollmentTokenFile = "/run/secrets/mnix-enrollment-token";
    trustedPublicKeys = [ "manage-nix-1:..." ];  # from `mnix signing-key`
  };

  # Download systems from the server's cache.
  nix.settings = {
    substituters = [ "https://cache.example.com/manage-nix" ];
    trusted-public-keys = [ "manage-nix:..." ];  # the Attic cache's key
  };
}
```

Switch to this configuration once by hand. The agent enrolls, stores its
credential in `/var/lib/manage-nix`, and shows up as online:

```sh
sudo mnix --token-file /var/lib/manage-nix/admin-token machines
```

From now on, deploy this machine through manage.nix.

## Next steps

- [Deploy your first revision](deploying.md)
- [Add health checks](configuration.md#health-checks) so bad deployments roll
  back automatically
- [Declare machines in Nix](configuration.md#declaring-repositories-and-machines)
  so enrollment needs no manual step
