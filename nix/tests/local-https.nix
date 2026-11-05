{ self, pkgs }:
pkgs.testers.runNixOSTest {
  name = "manage-nix-local-https";
  nodes.machine = {
    imports = [ self.nixosModules.server ];
    services.manage-nix.server = {
      enable = true;
      localHTTPS.enable = true;
    };
  };
  nodes.http = {
    imports = [ self.nixosModules.server ];
    services.manage-nix.server = {
      enable = true;
      initialAdminPasswordFile = "${pkgs.writeText "http-test-password" "http-test-password"}";
    };
  };
  testScript = ''
    http.wait_for_unit("mnix-server.service")
    http.wait_for_unit("mnix-http.service")
    http.wait_until_succeeds("curl --fail http://manage.nix/healthz")
    http.succeed("curl --fail http://manage.nix/ | grep -i '<!doctype html>'")
    http.succeed("ss -ltn | grep '127.0.0.1:80'")
    http.fail("ss -ltn | grep -E '(0.0.0.0|\\[::\\]):80'")
    http.fail("ss -ltn | grep ':443'")
    http.fail("test -e /var/lib/private/manage-nix-https/pki/authorities/local/root.crt")
    http.succeed("curl --fail -D /tmp/login-headers -c /tmp/cookies --json '{\"username\":\"admin\",\"password\":\"http-test-password\"}' http://manage.nix/api/session")
    http.fail("grep -i '^set-cookie:.*; secure' /tmp/login-headers")
    http.succeed("curl --fail -b /tmp/cookies http://manage.nix/api/session | grep admin")
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
