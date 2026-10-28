# Changelog

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
