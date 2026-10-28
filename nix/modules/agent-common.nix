# What the NixOS and nix-darwin agent modules share: their options and the
# agent's configuration file, /etc/manage-nix/agent.json.
{
  self,
  config,
  lib,
  pkgs,
  isDarwin,
}:
let
  cfg = config.services.manage-nix.agent;
  inherit (lib) mkOption types;

  servers = lib.mapAttrsToList (name: server: {
    inherit name;
    inherit (server) url;
    enrollment_token_file = server.enrollmentTokenFile;
    allow_builds = server.allowBuilds;
  }) cfg.servers;
in
{
  options.services.manage-nix.agent = {
    enable = lib.mkEnableOption "the manage.nix agent";

    package = mkOption {
      type = types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.mnix-agent;
      defaultText = lib.literalExpression "manage-nix.packages.\${system}.mnix-agent";
      description = "The mnix-agent package to use.";
    };

    servers = mkOption {
      default = { };
      example = lib.literalExpression ''
        {
          obsidian = {
            url = "http://100.64.0.1:8080";
            enrollmentTokenFile = config.age.secrets.mnix-obsidian.path;
            allowBuilds = true;
          };
          quartz.url = "http://100.64.0.2:8080";
        }
      '';
      description = ''
        Control planes to connect to, by name. The agent stays connected to
        every one it can reach; each enrolls the machine separately, sees
        its facts, and can deploy to it. The servers need not reach each
        other. Names are used in the state directory: letters, digits, `-`,
        and `_`.
      '';
      type = types.attrsOf (
        types.submodule {
          options = {
            url = mkOption {
              type = types.str;
              example = "https://mnix.example.com";
              description = ''
                Base URL of the server. Plain `http://` sends the machine's
                credential unencrypted: use it only over a private network
                such as a VPN or loopback.
              '';
            };

            enrollmentTokenFile = mkOption {
              type = types.nullOr types.str;
              default = null;
              example = "/run/secrets/mnix-enrollment-token";
              description = ''
                File containing the one-time token printed by
                `mnix machine add <name>`, or the token declared for this
                machine in the server's
                {option}`services.manage-nix.server.machines`. It is read
                only while the machine has not enrolled with this server;
                afterwards the agent uses the credential it stored in
                {file}`/var/lib/manage-nix/servers/<name>/credential.json`.
                Keep it out of the Nix store.
              '';
            };

            allowBuilds = mkOption {
              type = types.bool;
              default = false;
              description = ''
                Let this server have the machine build its own system from
                source the server sends (the server's `buildOn = "machine"`),
                instead of only activating systems signed by a trusted key.
                The machine then runs whatever configuration the server
                chooses, so this trusts the server as much as root.
              '';
            };
          };
        }
      );
    };

    server.url = mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "https://mnix.example.com";
      description = ''
        Shorthand for one server: the same as
        `servers.default.url`, with {option}`enrollmentTokenFile`.
      '';
    };

    enrollmentTokenFile = mkOption {
      type = types.nullOr types.str;
      default = null;
      description = "The enrollment token file of {option}`server.url`.";
    };

    trustedPublicKeys = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [ "manage-nix-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=" ];
      description = ''
        Nix signing keys (`name:base64`) trusted to sign systems for this
        machine; include the keys of servers that build systems
        (`mnix signing-key`). The agent refuses to activate a system whose
        top-level store path has no valid signature from one of these keys,
        unless it built the system itself for a server with
        `allowBuilds`. Nix's own `trusted-public-keys` still govern the
        rest of the closure when it is substituted.

        These keys are deliberately separate from
        {option}`nix.settings.trusted-public-keys`: trusting
        cache.nixos.org for packages must not mean trusting any system it
        has built.
      '';
    };

    healthChecks = {
      timeout = mkOption {
        type = types.ints.positive;
        default = 60;
        description = ''
          Seconds the health checks may take to pass after activation
          before the agent rolls back.
        '';
      };

      interval = mkOption {
        type = types.ints.positive;
        default = 2;
        description = "Seconds between rounds of health checks.";
      };

      http = mkOption {
        type = types.listOf types.str;
        default = [ ];
        example = [ "http://127.0.0.1:8080/healthz" ];
        description = "URLs that must answer a GET request with a 2xx status.";
      };

      commands = mkOption {
        type = types.attrsOf (types.either types.lines types.package);
        default = { };
        example = lib.literalExpression ''
          {
            database = "pg_isready --quiet";
          }
        '';
        description = ''
          Custom checks that must exit with status 0, by name. A string is a
          shell script; a package's main program is run directly. Commands
          run with an empty environment and `PATH` set to the new system's
          {file}`/run/current-system/sw/bin`, and time out after 30 seconds.
        '';
      };
    }
    // lib.optionalAttrs (!isDarwin) {
      systemdUnits = mkOption {
        type = types.listOf types.str;
        default = [ ];
        example = [ "nginx.service" ];
        description = "systemd units that must be active.";
      };
    };
  };

  config = lib.mkIf cfg.enable {
    services.manage-nix.agent.servers = lib.mkIf (cfg.server.url != null) {
      default = {
        inherit (cfg.server) url;
        inherit (cfg) enrollmentTokenFile;
      };
    };

    environment.systemPackages = [ cfg.package ];
    environment.etc."manage-nix/agent.json".source = (pkgs.formats.json { }).generate "mnix-agent.json" {
      version = 1;
      trusted_public_keys = cfg.trustedPublicKeys;
      inherit servers;
      health_checks = {
        timeout_secs = cfg.healthChecks.timeout;
        interval_secs = cfg.healthChecks.interval;
        systemd_units = cfg.healthChecks.systemdUnits or [ ];
        inherit (cfg.healthChecks) http;
        commands = lib.mapAttrs (
          name: command:
          if lib.isString command then
            "${pkgs.writeShellScript "mnix-health-check-${name}" command}"
          else
            lib.getExe command
        ) cfg.healthChecks.commands;
      };
    };
  };
}
