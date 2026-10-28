# nix-darwin module for mnix-agent: the same options as the NixOS module
# (without systemd health checks), run as a launchd daemon.
#
# Activations run as their own launchd daemons
# (`org.manage-nix.activation.<deployment>`), so they finish, or roll back,
# when activating a system reloads the agent's daemon. nix-darwin has no
# boot default apart from the running system: deployments use `switch` or
# `test`, and `boot` is refused.
self:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.manage-nix.agent;
  common = import ../agent-common.nix {
    inherit self config lib pkgs;
    isDarwin = true;
  };
in
{
  inherit (common) options;

  config = lib.mkMerge [
    common.config
    (lib.mkIf (cfg.enable && cfg.servers != { }) {
      launchd.daemons.mnix-agent = {
        # Restarted when the configuration changes, like any daemon whose
        # plist does: the plist names the configuration file's store path.
        serviceConfig = {
          ProgramArguments = [
            (lib.getExe cfg.package)
            "run"
          ];
          EnvironmentVariables = {
            RUST_LOG = "info";
            AGENT_CONFIG = "${config.environment.etc."manage-nix/agent.json".source}";
            # nix for activations and builds (also when Nix is installed
            # outside nix-darwin), git for the source of builds.
            PATH = lib.concatStringsSep ":" [
              "${pkgs.git}/bin"
              "/nix/var/nix/profiles/default/bin"
              "/run/current-system/sw/bin"
              "/usr/bin"
              "/bin"
              "/usr/sbin"
              "/sbin"
            ];
          };
          RunAtLoad = true;
          KeepAlive = true;
          ThrottleInterval = 5;
          StandardErrorPath = "/var/log/mnix-agent.log";
          StandardOutPath = "/var/log/mnix-agent.log";
        };
      };
    })
  ];
}
