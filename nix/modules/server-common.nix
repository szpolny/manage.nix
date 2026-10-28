# What the NixOS and nix-darwin server modules share: their options, the
# provisioning file, and the server's command line.
{
  self,
  config,
  lib,
  pkgs,
  stateDir,
  isDarwin,
}:
let
  cfg = config.services.manage-nix.server;
  inherit (lib) mkOption types;
  packages = self.packages.${pkgs.stdenv.hostPlatform.system};

  signingKeyFile =
    if cfg.signingKeyFile == null then "${stateDir}/signing-key" else cfg.signingKeyFile;
  localPostgresUrl = "postgres://manage-nix@localhost/manage-nix?host=/run/postgresql";
  adminTokenFile =
    if cfg.adminTokenFile == null then "${stateDir}/admin-token" else cfg.adminTokenFile;
  address = a: if lib.hasInfix ":" a then "[${a}]:${toString cfg.port}" else "${a}:${toString cfg.port}";

  provisionFile = (pkgs.formats.json { }).generate "mnix-provision.json" {
    repositories = lib.mapAttrs (_: r: {
      inherit (r) url;
      ssh_key_file = r.sshKeyFile;
    }) cfg.repositories;
    machines = lib.mapAttrs (_: m: {
      enrollment_token_file = m.enrollmentTokenFile;
      inherit (m) repository configuration tags;
    }) cfg.machines;
  };
in
{
  inherit stateDir adminTokenFile;

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

    webPackage = mkOption {
      type = types.nullOr types.package;
      default = packages.mnix-web;
      defaultText = lib.literalExpression "manage-nix.packages.\${system}.mnix-web";
      description = "The web UI to serve at `/`, or null for the API only.";
    };

    secureCookies = mkOption {
      type = types.bool;
      default = true;
      description = ''
        Mark the web UI's session cookie `Secure`, so browsers send it only
        over HTTPS. Turn this off only to test without a TLS proxy.
      '';
    };

    sessionLifetime = mkOption {
      type = types.str;
      default = "7d";
      description = "How long a web UI login lasts.";
    };

    initialAdminPasswordFile = mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "/run/secrets/manage-nix-admin-password";
      description = ''
        Password of the `admin` user the server creates on first start, when
        it has no users. If null, the server generates one into
        {file}`${stateDir}/initial-admin-password`. Change it after logging
        in, or with `mnix-server set-password admin`.
      '';
    };

    listenAddress = mkOption {
      type = types.str;
      default = "127.0.0.1";
      description = "Address the HTTP server listens on.";
    };

    extraListenAddresses = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [ "100.64.0.1" ];
      description = ''
        More addresses to listen on, at the same port: for example the
        machine's Tailscale address, so other machines' agents can connect
        while the web UI stays on loopback. The server starts without
        them and keeps trying until they exist. They must not overlap
        {option}`listenAddress` (`0.0.0.0` overlaps every IPv4 address).
      '';
    };

    buildOn = mkOption {
      type = types.enum [
        "server"
        "machine"
      ];
      default = "server";
      description = ''
        Where deployments build systems. `server`: this machine builds,
        signs, and (with {option}`attic.endpoint`) pushes each system, and
        machines activate it if they trust the signing key. `machine`: this
        server only fetches the repository and resolves the revision, then
        sends each machine the source to build its own system; machines
        must allow it ({option}`services.manage-nix.agent.servers.<name>.allowBuilds`).
        No cache is needed, and a nix-darwin server can deploy NixOS
        machines and the other way round.
      '';
    };

    repositories = mkOption {
      default = { };
      example = lib.literalExpression ''
        {
          nix = {
            url = "git@github.com:me/nix.git";
            sshKeyFile = config.age.secrets.mnix-deploy-key.path;
          };
        }
      '';
      description = ''
        Repositories to add (or move to a new URL) at every start, by name.
        Repositories added in the web UI or with `mnix repo add` are kept.
      '';
      type = types.attrsOf (
        types.submodule {
          options = {
            url = mkOption {
              type = types.str;
              description = "Anything `git clone` accepts.";
            };
            sshKeyFile = mkOption {
              type = types.nullOr types.str;
              default = null;
              description = ''
                SSH private key (a read-only deploy key) to fetch the
                repository with, readable by the server. Host keys are
                trusted on first use and kept in
                {file}`${stateDir}/known_hosts`.
              '';
            };
          };
        }
      );
    };

    machines = mkOption {
      default = { };
      example = lib.literalExpression ''
        {
          web-1 = {
            enrollmentTokenFile = config.age.secrets.mnix-web-1.path;
            repository = "nix";
            tags = [ "web" ];
          };
        }
      '';
      description = ''
        Machines to add at every start, by name, with their configuration
        and tags. Machines added in the web UI or with `mnix machine add`
        are kept.
      '';
      type = types.attrsOf (
        types.submodule {
          options = {
            enrollmentTokenFile = mkOption {
              type = types.nullOr types.str;
              default = null;
              description = ''
                A pre-shared enrollment token (16 characters or more, for
                example `openssl rand -hex 32`), readable by the server. The
                machine's agent enrolls with the same token in its
                {option}`services.manage-nix.agent.servers.<name>.enrollmentTokenFile`,
                so nobody has to run `mnix machine add`. It works once, and
                does not expire.
              '';
            };
            repository = mkOption {
              type = types.nullOr types.str;
              default = null;
              description = "The name of a repository in {option}`repositories`.";
            };
            configuration = mkOption {
              type = types.nullOr types.str;
              default = null;
              description = ''
                The machine's name under `nixosConfigurations` or
                `darwinConfigurations`; the machine's name if null.
              '';
            };
            tags = mkOption {
              type = types.nullOr (types.listOf types.str);
              default = null;
              description = "The machine's tags; left alone if null.";
            };
          };
        }
      );
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

  provisionFile = if cfg.repositories == { } && cfg.machines == { } then null else provisionFile;

  # The server's command line, without the database URL (in the
  # environment).
  args =
    [
      (lib.getExe cfg.package)
      "serve"
      "--listen"
      (lib.concatMapStringsSep "," address ([ cfg.listenAddress ] ++ cfg.extraListenAddresses))
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
      "--session-lifetime"
      cfg.sessionLifetime
      "--build-on"
      cfg.buildOn
    ]
    ++ lib.optionals (cfg.webPackage != null) [
      "--web-dir"
      "${cfg.webPackage}"
    ]
    ++ lib.optional (!cfg.secureCookies) "--insecure-cookies"
    ++ lib.optionals (cfg.initialAdminPasswordFile != null) [
      "--initial-admin-password-file"
      cfg.initialAdminPasswordFile
    ]
    ++ lib.optional (cfg.adminTokenFile == null) "--create-admin-token"
    ++ lib.optional (cfg.signingKeyFile == null) "--create-signing-key"
    ++ lib.optionals (cfg.repositories != { } || cfg.machines != { }) [
      "--provision"
      "${provisionFile}"
    ]
    ++ lib.optionals (cfg.attic.endpoint != null) [
      "--attic-endpoint"
      cfg.attic.endpoint
      "--attic-cache"
      cfg.attic.cache
      "--attic-token-file"
      cfg.attic.tokenFile
    ];

  assertions = [
    {
      assertion = cfg.heartbeatTimeout > cfg.heartbeatInterval;
      message = "services.manage-nix.server.heartbeatTimeout must be longer than heartbeatInterval.";
    }
    {
      assertion = cfg.attic.endpoint == null || cfg.attic.tokenFile != null;
      message = "services.manage-nix.server.attic.tokenFile must be set with attic.endpoint.";
    }
    {
      assertion = lib.all (
        m: m.repository == null || cfg.repositories ? ${m.repository}
      ) (lib.attrValues cfg.machines);
      message = "services.manage-nix.server.machines.*.repository must name one of services.manage-nix.server.repositories.";
    }
  ];
}
