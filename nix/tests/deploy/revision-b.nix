# Revision B: the demo service crashes a few seconds after starting, so its
# health checks never pass.
{ pkgs, ... }:
{
  environment.etc."mnix-test/revision".text = "b";
  systemd.services.demo = {
    wantedBy = [ "multi-user.target" ];
    serviceConfig = {
      DynamicUser = true;
      ExecStart = "${pkgs.runtimeShell} -c 'sleep 3; exit 1'";
      Restart = "on-failure";
      RestartSec = 1;
    };
  };
  services.manage-nix.agent.healthChecks = {
    timeout = 15;
    interval = 1;
    systemdUnits = [ "demo.service" ];
    http = [ "http://127.0.0.1:8000/" ];
  };
}
