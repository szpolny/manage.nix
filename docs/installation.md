# Installation

manage.nix has two parts:

- **The server** (`mnix-server`) builds systems, keeps the history, and serves
  the web UI and API. You run one (or a few) of these.
- **The agent** (`mnix-agent`) runs on every managed machine. It connects to the
  server, activates new systems, and rolls back when they are unhealthy.

Both ship as NixOS and nix-darwin modules in this flake.

## 1. Add the flake

```nix
inputs.manage-nix.url = "github:szpolny/manage.nix/v0.4.2-alpha"; # x-release-please-version
```

Pick the latest tag from the
[releases page](https://github.com/szpolny/manage.nix/releases).

### Finding the running version

Open **About** in the web UI navigation to see the running server version and
its build revision when available. Nix builds include the flake revision; builds
without revision metadata show “Not available”.

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

### Prebuilt binaries

The manage.nix packages are built by CI and published to a public Cachix
cache, currently for `x86_64-linux` only. Add it to every machine that runs
the server or the agent so they download the packages instead of compiling
them:

```nix
nix.settings = {
  extra-substituters = [ "https://manage-nix.cachix.org" ];
  extra-trusted-public-keys = [
    "manage-nix.cachix.org-1:fQDsGQA9QFK9OphLdfUDAkqj3emN+KkeaA0ajFnXtuM="
  ];
};
```

The cache only matches the packages built against manage.nix's own pinned
nixpkgs. Setting `inputs.manage-nix.inputs.nixpkgs.follows` changes every
package, so Nix builds them from source instead.

This cache serves manage.nix itself. The cache for your own systems is a
separate step (see [Set up a cache](#3-set-up-a-cache)).

## 2. Set up the server

```nix
{ manage-nix, ... }:
{
  imports = [ manage-nix.nixosModules.server ];  # or darwinModules.server

  services.manage-nix.server.enable = true;
}
```

Open **http://manage.nix/** on the machine running the server. The module
adds a loopback hosts entry and an HTTP proxy on `127.0.0.1:80`. No local
certificate authority or Keychain setup is needed. The
backend runs on `127.0.0.1:8080` with an SQLite database in
`/var/lib/manage-nix`. On first start it generates:

| File | What it is |
|---|---|
| `/var/lib/manage-nix/admin-token` | Token for the `mnix` CLI |
| `/var/lib/manage-nix/initial-admin-password` | Password of the web UI user `admin` |
| `/var/lib/manage-nix/signing-key` | Key the server signs built systems with |

### Optional local HTTPS

The default dashboard uses HTTP on loopback. To opt into local HTTPS instead:

```nix
services.manage-nix.server.localHTTPS.enable = true;
```

This disables the default HTTP proxy and enables secure session cookies.
When upgrading from an older release, remove an explicit `localHTTPS.enable = true`
to use the new HTTP default. When switching from HTTPS, clear existing
`manage.nix` cookies before signing in over HTTP.

The proxy generates a private local certificate authority and renews its
certificates automatically. Its keys stay outside the Nix store in
`/var/lib/manage-nix-https`. On macOS, privileged activation waits for the
proxy's root, installs SSL trust in the System Keychain's admin trust store,
and verifies that macOS trusts it. Trust failures stop activation with an
error instead of leaving a silently untrusted dashboard. Activation also
maintains a marked entry in
`/etc/hosts`, preserving other entries. Disabling both local proxies or the
server removes that managed entry.

On NixOS, installation automatically adds the public root to the system CA
bundle and p11-kit trust source after the proxy starts. Existing configured
CAs are preserved. No certificate copy or second rebuild is needed. The
runtime trust bundle is refreshed on activation and proxy restart.

Browsers using system trust need no manual certificate import. Restart an
already open browser if it cached the old trust store. Browsers configured
with a separate certificate store may still need the root imported there.

This address opens the server on the same machine; it does not expose the
proxy to your network. The proxy needs loopback port 443 available.

### Use your own HTTPS proxy

Disable the bundled local proxy if you already serve HTTPS:

```nix
services.manage-nix.server = {
  localHTTP.enable = false;
  localHTTPS.enable = false;
};
```

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

Open `http://manage.nix/` (or your configured hostname), log in as `admin` with the password from
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
credential in `/var/lib/manage-nix-agent` on NixOS (or `/var/lib/manage-nix`
on macOS), and shows up as online:

```sh
sudo mnix --token-file /var/lib/manage-nix/admin-token machines
```

From now on, deploy this machine through manage.nix.

### Agent state directory upgrades on NixOS

The NixOS agent uses `/var/lib/manage-nix-agent`, owned by root. The server
continues to use `/var/lib/manage-nix`, owned by `manage-nix`. Separate directories
prevent service restarts from changing the ownership of the agent's Git sources.

When upgrading an existing installation, the agent automatically moves its
credentials, source repositories, and activation jobs from the old shared
directory. Compatibility links preserve paths used by running activation jobs.
Enrollment tokens do not need to be issued again. Server data stays in place.
If both locations already contain the same state entry, startup stops with an
error rather than overwriting either copy; reconcile those entries before
restarting `mnix-agent`.

## Next steps

- [Deploy your first revision](deploying.md)
- [Add health checks](configuration.md#health-checks) so bad deployments roll
  back automatically
- [Declare machines in Nix](configuration.md#declaring-repositories-and-machines)
  so enrollment needs no manual step
