# NixOS module for mnix-server, the manage.nix control plane.
#
# The server speaks plain HTTP on `listenAddress:port`. Put a TLS-terminating
# reverse proxy (nginx, Caddy) in front of it, with WebSocket upgrades enabled
# for `/api/agent/ws`; agents only connect over HTTPS in production.
#
# The server builds machines' systems itself, as the `manage-nix` user,
# through this machine's Nix daemon, signs them with its signing key, and
# pushes them to Attic if `attic.endpoint` is set.
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
  signingKeyFile =
    if cfg.signingKeyFile == null then "${stateDir}/signing-key" else cfg.signingKeyFile;
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

    signingKeyFile = mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "/run/secrets/manage-nix-signing-key";
      description = ''
        Nix secret key (from `nix key generate-secret`) the server signs
        built systems with. If null, the server generates one on first start
        in {file}`${stateDir}/signing-key`. Print the public key with
        `mnix signing-key` and add it to
        {option}`services.manage-nix.agent.trustedPublicKeys` on every
        machine.
      '';
    };

    attic = {
      endpoint = mkOption {
        type = types.nullOr types.str;
        default = null;
        example = "https://cache.example.com/";
        description = ''
          Attic server to push built systems to. Machines must substitute
          from that cache ({option}`nix.settings.substituters`, with the
          cache's public key in {option}`nix.settings.trusted-public-keys`).
        '';
      };

      cache = mkOption {
        type = types.str;
        default = "manage-nix";
        description = "The Attic cache to push to.";
      };

      tokenFile = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "File with an Attic token allowed to push to the cache.";
      };
    };

    confirmTimeout = mkOption {
      type = types.ints.positive;
      default = 60;
      description = ''
        Seconds a machine has, after a healthy `switch` or `test`
        activation, to reach the server over a new connection. Machines
        that do not roll back on their own.
      '';
    };

    activationTimeout = mkOption {
      type = types.str;
      default = "30m";
      description = ''
        How long a machine may take to report the result of an activation,
        counted from when its system was built (so including any time
        offline).
      '';
    };

    buildTimeout = mkOption {
      type = types.str;
      default = "1h";
      description = "How long fetching and building one deployment may take.";
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = cfg.heartbeatTimeout > cfg.heartbeatInterval;
        message = "services.manage-nix.server.heartbeatTimeout must be longer than heartbeatInterval.";
      }
      {
        assertion = cfg.attic.endpoint == null || cfg.attic.tokenFile != null;
        message = "services.manage-nix.server.attic.tokenFile must be set with attic.endpoint.";
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
      # Builds run through the Nix daemon; git fetches repositories.
      path = [
        config.nix.package
        pkgs.git
      ]
      ++ lib.optional (cfg.attic.endpoint != null) pkgs.attic-client;
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
            "--state-dir"
            stateDir
            "--signing-key-file"
            signingKeyFile
            "--confirm-timeout"
            "${toString cfg.confirmTimeout}s"
            "--activation-timeout"
            cfg.activationTimeout
            "--build-timeout"
            cfg.buildTimeout
          ]
          ++ lib.optional (cfg.adminTokenFile == null) "--create-admin-token"
          ++ lib.optional (cfg.signingKeyFile == null) "--create-signing-key"
          ++ lib.optionals (cfg.attic.endpoint != null) [
            "--attic-endpoint"
            cfg.attic.endpoint
            "--attic-cache"
            cfg.attic.cache
            "--attic-token-file"
            cfg.attic.tokenFile
          ]
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
