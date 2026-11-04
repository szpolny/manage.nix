{ self, pkgs }:
pkgs.testers.runNixOSTest {
  name = "manage-nix-local-https";
  nodes.machine = {
    imports = [ self.nixosModules.server ];
    services.manage-nix.server.enable = true;
  };
  testScript = ''
    machine.wait_for_unit("mnix-server.service")
    machine.wait_for_unit("mnix-https.service")
    machine.wait_for_unit("mnix-https-trust.service")
    machine.wait_until_succeeds("test -s /var/lib/private/manage-nix-https/pki/authorities/local/root.crt")
    machine.wait_until_succeeds("curl --fail https://manage.nix/healthz")
    machine.succeed("curl --fail https://manage.nix/ | grep -i '<!doctype html>'")
    machine.succeed("systemctl restart mnix-https.service")
    machine.wait_for_unit("mnix-https-trust.service")
    machine.wait_until_succeeds("curl --fail https://manage.nix/healthz")
    machine.succeed("ss -ltn | grep '127.0.0.1:443'")
    machine.fail("ss -ltn | grep -E '(0.0.0.0|\\[::\\]):443'")
  '';
}
