# manage.nix

A dashboard and control plane for fleets of Nix-managed machines.

> **Alpha.** manage.nix is early software: expect breaking changes between
> releases.

- **Deploy from Git.** Point a machine at a flake and deploy a branch, tag, or
  commit to one machine or a whole tag.
- **Safe by default.** Machines check their own health after every activation
  and roll back on their own if something is wrong.
- **See what's running.** A web UI and the `mnix` CLI show every machine, its
  current system, pending updates, and deployment history.
- **NixOS and macOS.** Runs on NixOS and nix-darwin, server and agent alike.

## Installation

Add the flake to your configuration:

```nix
{
  inputs.manage-nix.url = "github:szpolny/manage.nix/v0.3.0-alpha"; # x-release-please-version

  outputs = { nixpkgs, manage-nix, ... }: {
    # The control plane
    nixosConfigurations.server = nixpkgs.lib.nixosSystem {
      modules = [
        manage-nix.nixosModules.server
        { services.manage-nix.server.enable = true; }
      ];
    };

    # Every machine you want to manage
    nixosConfigurations.web-1 = nixpkgs.lib.nixosSystem {
      modules = [
        manage-nix.nixosModules.agent
        {
          services.manage-nix.agent = {
            enable = true;
            server.url = "https://mnix.example.com";
            enrollmentTokenFile = "/run/secrets/mnix-enrollment-token";
          };
        }
      ];
    };
  };
}
```

Then follow the [installation guide](docs/installation.md) to put the server
behind HTTPS, enroll the machine, and run your first deployment.

## Documentation

- [Installation](docs/installation.md)
- [Deploying](docs/deploying.md)
- [Configuration](docs/configuration.md)
- [CLI reference](docs/cli.md)
- [Development](docs/development.md)
