# NixOS module for mnix-server, the manage.nix control plane.
#
# The server speaks plain HTTP on `listenAddress:port` (and
# `extraListenAddresses`) and serves the web UI there too. Put a TLS-terminating reverse proxy (nginx, Caddy) in front of
# it, with WebSocket upgrades enabled for `/api/agent/ws` and response
# buffering off for `/api/deployments/*/events` (server-sent events); agents
# and browsers only connect over HTTPS in production.
#
# With `buildOn = "server"`, the server builds machines' systems itself, as
# the `manage-nix` user, through this machine's Nix daemon, signs them with
# its signing key, and pushes them to Attic if `attic.endpoint` is set. With
# `buildOn = "machine"`, it sends each machine the source to build.
self:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.manage-nix.server;
  common = import ./server-common.nix {
    inherit self config lib pkgs;
    stateDir = "/var/lib/manage-nix";
    isDarwin = false;
  };
  inherit (common) stateDir;
in
{
  inherit (common) options;

  config = lib.mkIf cfg.enable {
    inherit (common) assertions;

    environment.systemPackages = [
      cfg.package
    ]
    ++ lib.optional (cfg.cliPackage != null) cfg.cliPackage;

    users.users.manage-nix = {
      isSystemUser = true;
      group = "manage-nix";
      home = stateDir;
    };
    users.groups.manage-nix = { };

    services.postgresql = lib.mkIf cfg.database.createLocally {
      enable = true;
      ensureDatabases = [ "manage-nix" ];
      ensureUsers = [
        {
          name = "manage-nix";
          ensureDBOwnership = true;
        }
      ];
    };

    systemd.services.mnix-server = {
      description = "manage.nix control plane";
      wantedBy = [ "multi-user.target" ];
      after = [ "network.target" ] ++ lib.optional cfg.database.createLocally "postgresql.target";
      requires = lib.optional cfg.database.createLocally "postgresql.target";
      environment = {
        MNIX_DATABASE_URL = cfg.database.url;
        RUST_LOG = lib.mkDefault "info";
      };
      # Builds run through the Nix daemon; git fetches repositories, over
      # ssh for repositories with an SSH key.
      path = [
        config.nix.package
        pkgs.git
        pkgs.openssh
      ]
      ++ lib.optional (cfg.attic.endpoint != null) pkgs.attic-client;
      serviceConfig = {
        ExecStart = lib.escapeShellArgs common.args;
        EnvironmentFile = lib.mkIf (cfg.environmentFile != null) cfg.environmentFile;
        User = "manage-nix";
        Group = "manage-nix";
        StateDirectory = "manage-nix";
        StateDirectoryMode = "0750";
        UMask = "0077";
        Restart = "on-failure";
        RestartSec = 2;

        NoNewPrivileges = true;
        PrivateTmp = true;
        PrivateDevices = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectControlGroups = true;
        RestrictAddressFamilies = [
          "AF_INET"
          "AF_INET6"
          "AF_UNIX"
        ];
        RestrictNamespaces = true;
        LockPersonality = true;
        MemoryDenyWriteExecute = true;
        SystemCallArchitectures = "native";
      };
    };
  };
}
