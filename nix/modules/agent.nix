# NixOS module for mnix-agent.
#
# With `server.url` set, the `mnix-agent` service enrolls the machine with the
# control plane (once, using `enrollmentTokenFile`) and keeps a WebSocket to
# it open. The credential it receives is kept in /var/lib/manage-nix.
#
# The agent reads `/etc/manage-nix/agent.json` from two places: trusted keys
# from the running system, and health checks from the system being activated
# (`<toplevel>/etc/manage-nix/agent.json`), so each configuration declares
# what healthy means for itself.
self:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.manage-nix.agent;
  inherit (lib) mkOption types;

  agentConfig = {
    version = 1;
    trusted_public_keys = cfg.trustedPublicKeys;
    health_checks = {
      timeout_secs = cfg.healthChecks.timeout;
      interval_secs = cfg.healthChecks.interval;
      systemd_units = cfg.healthChecks.systemdUnits;
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

    server.url = mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "https://mnix.example.com";
      description = ''
        URL of the control plane. When set, the `mnix-agent` service
        connects to it and keeps the connection open.
      '';
    };

    enrollmentTokenFile = mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "/run/secrets/mnix-enrollment-token";
      description = ''
        File containing the one-time token printed by
        `mnix machine add <name>`. It is read only while the machine has not
        enrolled; afterwards the agent uses the credential it stored in
        {file}`/var/lib/manage-nix/credential.json`. Keep it out of the Nix
        store.
      '';
    };

    trustedPublicKeys = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [ "manage-nix-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=" ];
      description = ''
        Nix signing keys (`name:base64`) trusted to sign systems for this
        machine. The agent refuses to activate a system whose top-level
        store path has no valid signature from one of these keys. Nix's own
        `trusted-public-keys` still govern the rest of the closure when it
        is substituted.

        These keys are deliberately separate from
        {option}`nix.settings.trusted-public-keys`: trusting
        cache.nixos.org for packages must not mean trusting any NixOS system
        it has built.
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

      systemdUnits = mkOption {
        type = types.listOf types.str;
        default = [ ];
        example = [ "nginx.service" ];
        description = "systemd units that must be active.";
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
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [ cfg.package ];
    environment.etc."manage-nix/agent.json".source =
      (pkgs.formats.json { }).generate "mnix-agent.json"
        agentConfig;

    systemd.services.mnix-agent = lib.mkIf (cfg.server.url != null) {
      description = "manage.nix agent";
      wantedBy = [ "multi-user.target" ];
      wants = [ "network-online.target" ];
      after = [ "network-online.target" ];
      environment.RUST_LOG = lib.mkDefault "info";
      serviceConfig = {
        ExecStart = lib.escapeShellArgs (
          [
            (lib.getExe cfg.package)
            "run"
            "--server-url"
            cfg.server.url
          ]
          ++ lib.optionals (cfg.enrollmentTokenFile != null) [
            "--enrollment-token-file"
            cfg.enrollmentTokenFile
          ]
        );
        # Root: activating systems (M3) needs it.
        StateDirectory = "manage-nix";
        StateDirectoryMode = "0700";
        Restart = "always";
        RestartSec = 5;
      };
    };
  };
}
