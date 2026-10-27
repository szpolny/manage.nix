# Revision A: a demo web service that passes its health checks.
{ pkgs, ... }:
{
  environment.etc."mnix-test/revision".text = "a";
  systemd.services.demo = {
    wantedBy = [ "multi-user.target" ];
    serviceConfig = {
      DynamicUser = true;
      ExecStart = "${pkgs.busybox}/bin/httpd -f -p 127.0.0.1:8000 -h ${pkgs.writeTextDir "index.html" "a\n"}";
    };
  };
  services.manage-nix.agent.healthChecks = {
    timeout = 15;
    interval = 1;
    systemdUnits = [ "demo.service" ];
    http = [ "http://127.0.0.1:8000/" ];
  };
}
