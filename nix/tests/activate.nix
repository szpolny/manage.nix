# M1: the agent activates a healthy system, rolls back an unhealthy one, and
# refuses systems that are not signed by a trusted key.
#
# The node carries three variants of itself as specialisations, so their
# store paths are already in the VM's store:
#   good:   a demo web service that passes every health check
#   bad:    the demo service crashes after a few seconds, so the HTTP check
#           never passes and the agent rolls back after the timeout
#   broken: the demo service fails to start, so switch-to-configuration
#           itself fails
{ self, pkgs }:
let
  # A key pair for this test only, generated with `nix key generate-secret`.
  testKey = {
    public = ./keys/mnix-test-1.public;
    secret = ./keys/mnix-test-1.secret;
  };

  healthChecks = {
    timeout = 15;
    interval = 1;
    systemdUnits = [ "demo.service" ];
    http = [ "http://127.0.0.1:8080/" ];
    commands.marker = "test -f /etc/mnix-test/variant";
  };

  variant =
    name: service:
    { lib, ... }:
    {
      services.manage-nix.agent.healthChecks = healthChecks;
      environment.etc."mnix-test/variant".text = name;
      systemd.services.demo = lib.recursiveUpdate {
        wantedBy = [ "multi-user.target" ];
        serviceConfig.DynamicUser = true;
      } service;
    };
in
pkgs.testers.runNixOSTest {
  name = "manage-nix-activate";

  nodes.machine =
    { lib, pkgs, ... }:
    {
      imports = [ self.nixosModules.agent ];

      services.manage-nix.agent = {
        enable = true;
        trustedPublicKeys = [ (lib.fileContents testKey.public) ];
      };
      nix.settings.experimental-features = [ "nix-command" ];
      environment.etc."mnix-test/signing.secret".source = testKey.secret;
      environment.systemPackages = [ pkgs.curl ];

      specialisation = {
        good.configuration = variant "good" {
          serviceConfig.ExecStart = "${lib.getExe pkgs.python3} -m http.server --bind 127.0.0.1 8080";
        };
        bad.configuration = variant "bad" {
          serviceConfig = {
            ExecStart = "${pkgs.runtimeShell} -c 'sleep 3; exit 1'";
            Restart = "on-failure";
            RestartSec = 1;
          };
        };
        broken.configuration = variant "broken" {
          serviceConfig = {
            Type = "oneshot";
            RemainAfterExit = true;
            ExecStart = "${pkgs.coreutils}/bin/false";
          };
        };
      };
    };

  testScript = ''
    import json

    machine.wait_for_unit("multi-user.target")

    def resolve(path):
        return machine.succeed(f"readlink -f {path}").strip()

    base = resolve("/run/current-system")
    good, bad, broken = (
        resolve(f"/run/current-system/specialisation/{name}")
        for name in ["good", "bad", "broken"]
    )

    def activate(path, expected_exit, mode="switch"):
        status, out = machine.execute(f"mnix-agent activate --mode {mode} {path}")
        assert status == expected_exit, f"exit {status}, expected {expected_exit}: {out}"
        return json.loads(out)

    def sign(path, key="/etc/mnix-test/signing.secret"):
        machine.succeed(f"nix store sign --key-file {key} {path}")

    def assert_running(path):
        assert resolve("/run/current-system") == path, "unexpected running system"

    def assert_profile(path):
        assert resolve("/nix/var/nix/profiles/system") == path, "unexpected boot default"

    with subtest("unsigned and untrusted systems are refused"):
        report = activate(good, 1)
        assert report["status"] == "failed" and not report["system_changed"], report
        assert "no valid signature" in report["message"], report

        machine.succeed("nix key generate-secret --key-name untrusted-1 > /tmp/untrusted.secret")
        sign(good, key="/tmp/untrusted.secret")
        report = activate(good, 1)
        assert "untrusted-1" in report["message"], report

        report = activate("/etc/passwd", 1)
        assert "not a top-level Nix store path" in report["message"], report
        assert_running(base)

    for path in [good, bad, broken]:
        sign(path)

    with subtest("a healthy system is activated and stays"):
        report = activate(good, 0)
        assert report["status"] == "succeeded", report
        assert report["previous_system"] == base, report
        assert_running(good)
        assert_profile(good)
        machine.succeed("curl --fail --silent http://127.0.0.1:8080/")

    with subtest("an unhealthy system is rolled back"):
        report = activate(bad, 2)
        assert report["status"] == "rolled_back", report
        assert any("http" in c for c in report["failed_checks"]), report
        assert_running(good)
        assert_profile(good)
        machine.wait_until_succeeds("curl --fail --silent http://127.0.0.1:8080/")
        machine.succeed("grep -qx good /etc/mnix-test/variant")

    with subtest("a failing switch-to-configuration is rolled back in test mode"):
        report = activate(broken, 2, mode="test")
        assert report["status"] == "rolled_back", report
        assert "switch-to-configuration test" in report["message"], report
        assert_running(good)
        assert_profile(good)
        machine.wait_until_succeeds("curl --fail --silent http://127.0.0.1:8080/")

    with subtest("boot mode changes only the boot default"):
        report = activate(bad, 0, mode="boot")
        assert report["status"] == "succeeded", report
        assert_running(good)
        assert_profile(bad)
        activate(good, 0)
        assert_profile(good)

    with subtest("facts describe the running system"):
        facts = json.loads(machine.succeed("mnix-agent facts"))
        assert facts["hostname"] == "machine", facts
        assert facts["current_system"] == good, facts
        assert facts["booted_system"] == base, facts
        assert not facts["reboot_required"], facts
        assert facts["kernel"] == machine.succeed("uname -r").strip(), facts
        assert facts["nixos_version"], facts
        current = [g for g in facts["generations"] if g["current"]]
        assert len(current) == 1 and current[0]["store_path"] == good, facts
  '';
}
