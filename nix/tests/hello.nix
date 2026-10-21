# M0: the agent module installs a working mnix-agent.
{ self, pkgs }:
pkgs.testers.runNixOSTest {
  name = "manage-nix-hello";

  nodes.machine = {
    imports = [ self.nixosModules.agent ];
    services.manage-nix.agent.enable = true;
  };

  testScript = ''
    machine.wait_for_unit("multi-user.target")
    version = machine.succeed("mnix-agent --version")
    assert version.startswith("mnix-agent "), version
    machine.succeed("mnix-agent | grep -F 'protocol v1'")
  '';
}
