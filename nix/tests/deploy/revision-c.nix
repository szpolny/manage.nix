# Revision C: healthy, but the firewall drops everything from the server,
# including packets of connections that are already open. The machine
# cannot confirm the new system, so it must roll back on its own.
{ nodes, pkgs, ... }:
let
  inherit (nodes.server.networking) primaryIPAddress primaryIPv6Address;
in
{
  environment.etc."mnix-test/revision".text = "c";
  systemd.services.demo = {
    wantedBy = [ "multi-user.target" ];
    serviceConfig = {
      DynamicUser = true;
      ExecStart = "${pkgs.busybox}/bin/httpd -f -p 127.0.0.1:8000 -h ${pkgs.writeTextDir "index.html" "c\n"}";
    };
  };
  services.manage-nix.agent.healthChecks = {
    timeout = 15;
    interval = 1;
    systemdUnits = [ "demo.service" ];
    http = [ "http://127.0.0.1:8000/" ];
  };
  # Ahead of the rule accepting established connections.
  networking.firewall.extraCommands = ''
    iptables -I nixos-fw 1 -s ${primaryIPAddress} -j nixos-fw-refuse
    ip6tables -I nixos-fw 1 -s ${primaryIPv6Address} -j nixos-fw-refuse
  '';
}
