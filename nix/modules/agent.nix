# NixOS module for mnix-agent.
#
# The `mnix-agent` service connects to every server in `servers`, enrolls
# with each once (using its `enrollmentTokenFile`), and keeps a WebSocket to
# each open. The credentials it receives are kept in
# /var/lib/manage-nix/servers/<name>.
#
# Systems a server deploys run in their own transient units
# (`mnix-activation-<deployment>.service`), so an activation finishes, or
# rolls back, even when the switch restarts this service or breaks the
# network. The machine fetches systems through Nix's own substituters (so
# configure `nix.settings.substituters` for a server's cache), or builds
# them itself for servers with `allowBuilds`.
#
# The agent reads `/etc/manage-nix/agent.json` from two places: servers and
# trusted keys from the running system, and health checks from the system
# being activated (`<toplevel>/etc/manage-nix/agent.json`), so each
# configuration declares what healthy means for itself.
self:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.manage-nix.agent;
  common = import ./agent-common.nix {
    inherit self config lib pkgs;
    isDarwin = false;
  };
in
{
  inherit (common) options;

  config = lib.mkMerge [
    common.config
    (lib.mkIf (cfg.enable && cfg.servers != { }) {
      systemd.services.mnix-agent = {
        description = "manage.nix agent";
        wantedBy = [ "multi-user.target" ];
        wants = [ "network-online.target" ];
        after = [ "network-online.target" ];
        # The server list is read at start.
        restartTriggers = [ config.environment.etc."manage-nix/agent.json".source ];
        environment.RUST_LOG = lib.mkDefault "info";
        # nix-store, nix-env, and nix for activations and builds; git for
        # the source of builds.
        path = [
          config.nix.package
          pkgs.git
        ];
        serviceConfig = {
          ExecStart = "${lib.getExe cfg.package} run";
          # Root: activating systems needs it.
          StateDirectory = "manage-nix";
          StateDirectoryMode = "0700";
          Restart = "always";
          RestartSec = 5;
        };
      };
    })
  ];
}
