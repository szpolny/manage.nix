# Changelog

## [0.3.3-alpha](https://github.com/szpolny/manage.nix/compare/v0.3.2-alpha...v0.3.3-alpha) (2026-11-05)


### Bug Fixes

* **darwin:** restore local HTTPS dashboard ([b54f42a](https://github.com/szpolny/manage.nix/commit/b54f42a57d1d2a9bc1ad33c760e940327f98e064))

## [0.3.2-alpha](https://github.com/szpolny/manage.nix/compare/v0.3.1-alpha...v0.3.2-alpha) (2026-11-04)


### Features

* **server:** serve trusted local HTTPS at manage.nix ([7c8a5b2](https://github.com/szpolny/manage.nix/commit/7c8a5b2263eca26b62150c6aed7b39d457a7ed13))

## [0.3.1-alpha](https://github.com/szpolny/manage.nix/compare/v0.3.0-alpha...v0.3.1-alpha) (2026-11-04)


### Features

* show and apply nixpkgs and Flatpak updates ([5c8e1b8](https://github.com/szpolny/manage.nix/commit/5c8e1b8b78aad04960c5bffc84c1c72eae7e5fe1))
* **web:** redesign the dashboard ([cee4bc9](https://github.com/szpolny/manage.nix/commit/cee4bc9e839fc09d8feba346b5bf76afe7ba0ce3))


### Bug Fixes

* **server:** return uncached 404s for missing web assets ([2e458b0](https://github.com/szpolny/manage.nix/commit/2e458b0915a7a9f89215245fd9e5f530b7b8d884))

## [0.3.0-alpha](https://github.com/szpolny/manage.nix/compare/v0.2.0...v0.3.0-alpha) (2026-10-28)


### Features

* show machine updates and reboot machines ([90730a7](https://github.com/szpolny/manage.nix/commit/90730a70b7b696fb1a9bc6dff4323d81aaa3afb6))

## [0.2.0](https://github.com/szpolny/manage.nix/compare/v0.1.0...v0.2.0) (2026-10-28)


### ⚠ BREAKING CHANGES

* `mnix-agent run` reads its servers from /etc/manage-nix/agent.json instead of `--server-url` and `--enrollment-token-file`; credentials move to /var/lib/manage-nix/servers/<name>/ (migrated on start). Facts call `nixos_version` `os_version`.

### Features

* add authenticated web dashboard and deployment streaming ([97de9a9](https://github.com/szpolny/manage.nix/commit/97de9a9f90fe86f915014f672b47d6e13b61d17c))
* add end-to-end deployments and connectivity rollback ([9edc768](https://github.com/szpolny/manage.nix/commit/9edc7681141c4d0ce3cae6bd66d9e10d385c6165))
* add machine enrollment and control plane ([5b1e4b7](https://github.com/szpolny/manage.nix/commit/5b1e4b73933797bf47eefb89c527876fbbd298d1))
* **agent:** activate systems with health checks and rollback ([dc8548c](https://github.com/szpolny/manage.nix/commit/dc8548c6157dfe03a3b5ec6f8538d3f570438c25))
* **dev:** add process-compose development environment ([051fec0](https://github.com/szpolny/manage.nix/commit/051fec0d54fdead57ac78a64d5090228c8207ac9))
* several servers, machine builds, nix-darwin, and declared machines ([af1e5c8](https://github.com/szpolny/manage.nix/commit/af1e5c8c19baa96335160e7605a4479689a964eb))
