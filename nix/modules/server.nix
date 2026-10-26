# NixOS module for mnix-server, the manage.nix control plane.
#
# The server speaks plain HTTP on `listenAddress:port`. Put a TLS-terminating
# reverse proxy (nginx, Caddy) in front of it, with WebSocket upgrades enabled
# for `/api/agent/ws`; agents only connect over HTTPS in production.
self:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.manage-nix.server;
  inherit (lib) mkOption types;
  packages = self.packages.${pkgs.stdenv.hostPlatform.system};

  stateDir = "/var/lib/manage-nix";
  localPostgresUrl = "postgres://manage-nix@localhost/manage-nix?host=/run/postgresql";
  adminTokenFile =
    if cfg.adminTokenFile == null then "${stateDir}/admin-token" else cfg.adminTokenFile;
in
{
  options.services.manage-nix.server = {
    enable = lib.mkEnableOption "the manage.nix control plane";

    package = mkOption {
      type = types.package;
      default = packages.mnix-server;
      defaultText = lib.literalExpression "manage-nix.packages.\${system}.mnix-server";
      description = "The mnix-server package to use.";
    };

    cliPackage = mkOption {
      type = types.nullOr types.package;
      default = packages.mnix;
      defaultText = lib.literalExpression "manage-nix.packages.\${system}.mnix";
      description = "The `mnix` command-line client to install, or null for none.";
    };

    listenAddress = mkOption {
      type = types.str;
      default = "127.0.0.1";
      description = "Address the HTTP server listens on.";
    };

    port = mkOption {
      type = types.port;
      default = 8080;
      description = "Port the HTTP server listens on.";
    };

    database = {
      url = mkOption {
        type = types.str;
        default = if cfg.database.createLocally then localPostgresUrl else "sqlite://${stateDir}/db.sqlite";
        defaultText = lib.literalExpression ''
          if createLocally then "${localPostgresUrl}" else "sqlite://${stateDir}/db.sqlite"
        '';
        description = ''
          `sqlite://...` or `postgres://...`. SQLite supports a single server
          instance; use Postgres for replicas. Put URLs containing passwords in
          {option}`environmentFile` as `MNIX_DATABASE_URL` instead.
        '';
      };

      createLocally = mkOption {
        type = types.bool;
        default = false;
        description = ''
          Run Postgres on this machine with a `manage-nix` database and
          role (peer authentication over the Unix socket), and use it.
        '';
      };
    };

    adminTokenFile = mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "/run/secrets/manage-nix-admin-token";
      description = ''
        File with the admin API token used by `mnix`. If null, the server
        generates one on first start in {file}`${stateDir}/admin-token`
        (readable by root and the `manage-nix` user).
      '';
    };

    environmentFile = mkOption {
      type = types.nullOr types.str;
      default = null;
      description = ''
        systemd environment file for secrets, for example
        `MNIX_DATABASE_URL=postgres://user:password@host/db`.
      '';
    };

    heartbeatInterval = mkOption {
      type = types.ints.positive;
      default = 15;
      description = "Seconds between agent heartbeats.";
    };

    heartbeatTimeout = mkOption {
      type = types.ints.positive;
      default = 45;
      description = "Seconds without a heartbeat before a machine shows as offline.";
    };

    enrollmentTokenTtl = mkOption {
      type = types.str;
      default = "1h";
      description = "How long enrollment tokens are valid unless `mnix machine add --expires-in` says otherwise.";
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = cfg.heartbeatTimeout > cfg.heartbeatInterval;
        message = "services.manage-nix.server.heartbeatTimeout must be longer than heartbeatInterval.";
      }
    ];

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
      serviceConfig = {
        ExecStart = lib.escapeShellArgs (
          [
            (lib.getExe cfg.package)
            "serve"
            "--listen"
            "${
              if lib.hasInfix ":" cfg.listenAddress then "[${cfg.listenAddress}]" else cfg.listenAddress
            }:${toString cfg.port}"
            "--admin-token-file"
            adminTokenFile
            "--heartbeat-interval"
            "${toString cfg.heartbeatInterval}s"
            "--heartbeat-timeout"
            "${toString cfg.heartbeatTimeout}s"
            "--enrollment-token-ttl"
            cfg.enrollmentTokenTtl
          ]
          ++ lib.optional (cfg.adminTokenFile == null) "--create-admin-token"
        );
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
