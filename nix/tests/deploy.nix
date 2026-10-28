# M3: the core loop. The server builds machines' systems from a Git
# repository, signs them, and pushes them to Attic; the machines fetch and
# activate them, and roll back on their own when the result is unhealthy or
# cannot reach the server.
#
# One server (mnix-server, Attic, and the Git repository) and two machines.
# The repository has three tagged revisions (./deploy/revision-*.nix):
#   a: a healthy demo service. Deployed to both machines.
#   b: the demo service crashes. machine1 rolls back to a.
#   c: healthy, but the firewall drops the server's packets. machine2
#      cannot confirm the new system and rolls back to a.
# Finally `mnix rollback` returns machine1 to the system it booted.
#
# The VMs are offline, so the server cannot really build anything. Instead,
# the repository's flake evaluates this very test's machines with the
# revision's module, which gives exactly the systems built below on the
# host, and those are in the server's store already: `nix build` evaluates
# the flake from Git and finds its output present. Everything else (fetching
# the revision, evaluation, signing, pushing, fetching from Attic on the
# machines) is real.
#
# Runs once per database backend: `backend` is "sqlite" or "postgres".
#
# M4: with `scenario = "web"`, a browser on a `client` node does the same
# through the web UI instead (nix/tests/web/e2e.py, Playwright): it logs in,
# enrolls machine1, configures it, deploys revision a and follows the live
# log, and then sees the revision and the generation diff on the machine's
# page.
{
  self,
  pkgs,
  backend,
  scenario ? "cli",
}:
let
  inherit (pkgs) lib;
  web = scenario == "web";
  adminPassword = pkgs.writeText "mnix-admin-password" "correct horse battery";

  testKey = {
    public = ./keys/mnix-test-1.public;
    secret = ./keys/mnix-test-1.secret;
  };
  tokenFile = "/run/mnix-enrollment-token";
  revisions = {
    a = ./deploy/revision-a.nix;
    b = ./deploy/revision-b.nix;
    c = ./deploy/revision-c.nix;
  };

  machine =
    {
      config,
      extendModules,
      lib,
      pkgs,
      ...
    }:
    {
      imports = [ self.nixosModules.agent ];
      services.manage-nix.agent = {
        enable = true;
        server.url = "http://server:8080";
        enrollmentTokenFile = tokenFile;
        trustedPublicKeys = [ (lib.fileContents testKey.public) ];
      };
      # The only substituter is the server's Attic cache, whose key exists
      # once the test has created the cache.
      nix.settings.substituters = lib.mkForce [ ];
      nix.extraOptions = "!include /run/mnix-test/nix.conf";
      # Test nodes leave switch-to-configuration out unless asked; real
      # machines have it.
      system.switch.enable = true;
      # Test VMs boot their kernel directly. On x86_64 the default GRUB
      # would try (and fail) to install itself on every switch.
      boot.loader.grub.enable = false;
      documentation.enable = false;
      environment.systemPackages = [ pkgs.curl ];
      virtualisation.memorySize = 1536;

      # An installed NixOS machine has a system profile; a test VM starts
      # without one.
      systemd.services.mnix-test-profile = {
        wantedBy = [ "multi-user.target" ];
        before = [ "mnix-agent.service" ];
        unitConfig.ConditionPathExists = "!/nix/var/nix/profiles/system";
        serviceConfig.Type = "oneshot";
        path = [ config.nix.package ];
        script = ''
          nix-env --profile /nix/var/nix/profiles/system --set "$(readlink -f /run/current-system)"
        '';
      };

      # This machine with a revision's module added: what the test
      # repository's flake builds.
      system.build.mnixWithRevision = revision: extendModules { modules = [ revision ]; };
    };

  hostSystem = pkgs.stdenv.hostPlatform.system;

  # The test repository's flake. Its inputs are the store paths this test is
  # built from, so it evaluates offline to the same systems.
  repositoryFlake = pkgs.writeText "flake.nix" ''
    {
      inputs = {
        nixpkgs.url = "path:${self.inputs.nixpkgs}";
        crane.url = "path:${self.inputs.crane}";
        manage-nix = {
          url = "path:${self}";
          inputs.nixpkgs.follows = "nixpkgs";
          inputs.crane.follows = "crane";
        };
      };
      outputs =
        { manage-nix, nixpkgs, ... }:
        let
          test = import "''${manage-nix}/nix/tests/deploy.nix" {
            self = manage-nix;
            pkgs = nixpkgs.legacyPackages.${hostSystem};
            backend = "${backend}";
            scenario = "${scenario}";
          };
        in
        {
          nixosConfigurations = builtins.mapAttrs (
            _: node: node.system.build.mnixWithRevision ./revision.nix
          ) { inherit (test.nodes) machine1 machine2; };
        };
    }
  '';

  systemOf =
    nodes: name: revision:
    (nodes.${name}.system.build.mnixWithRevision revisions.${revision}).config.system.build.toplevel;
  deployed = [
    [
      "machine1"
      "a"
    ]
    [
      "machine1"
      "b"
    ]
    [
      "machine2"
      "a"
    ]
    [
      "machine2"
      "c"
    ]
  ];

  atticEnvironment = pkgs.runCommand "atticd-env" { } ''
    echo ATTIC_SERVER_TOKEN_RS256_SECRET_BASE64="$(${lib.getExe pkgs.openssl} genrsa -traditional 4096 | ${pkgs.coreutils}/bin/base64 -w0)" > $out
  '';
in
pkgs.testers.runNixOSTest {
  name = "manage-nix-${if web then "web" else "deploy"}-${backend}";

  nodes = {
    server =
      { nodes, pkgs, ... }:
      {
        imports = [ self.nixosModules.server ];
        services.manage-nix.server = {
          enable = true;
          listenAddress = "0.0.0.0";
          database.createLocally = backend == "postgres";
          heartbeatInterval = 2;
          heartbeatTimeout = 6;
          signingKeyFile = "/etc/mnix-test/signing.secret";
          confirmTimeout = 20;
          # The browser reaches the server over plain HTTP.
          secureCookies = !web;
          initialAdminPasswordFile = "${adminPassword}";
          attic = {
            endpoint = "http://server:8081/";
            cache = "mnix";
            tokenFile = "/run/mnix-test/attic-token";
          };
        };
        # Started by the test once Attic has given it a token.
        systemd.services.mnix-server.wantedBy = lib.mkForce [ ];
        environment.etc."mnix-test/signing.secret" = {
          source = testKey.secret;
          mode = "0400";
          user = "manage-nix";
        };

        services.atticd = {
          enable = true;
          environmentFile = atticEnvironment;
          settings = {
            listen = "[::]:8081";
            # The VMs have little CPU to spare; a test cache need not be small.
            compression.type = "none";
          };
        };

        networking.firewall.allowedTCPPorts = [
          8080
          8081
        ];
        nix.settings = {
          experimental-features = [
            "nix-command"
            "flakes"
          ];
          substituters = lib.mkForce [ ];
        };
        documentation.enable = false;
        environment.systemPackages = [
          pkgs.attic-client
          pkgs.git
          pkgs.jq
        ];
        virtualisation = {
          memorySize = 4096;
          diskSize = 8192;
          cores = 2;
          # The server reads store paths byte for byte (to push them to
          # Attic), so give it a store image instead of the host's store over
          # 9p: on macOS hosts that has Nix's case-hack file names, and the
          # paths no longer match their hashes.
          useNixStoreImage = true;
          writableStore = true;
          # What `nix build` will find, and the flake's inputs.
          additionalPaths = [
            repositoryFlake
          ]
          ++ map (d: systemOf nodes (lib.elemAt d 0) (lib.elemAt d 1)) deployed
          # The system machine1 boots, to diff against what it is deployed.
          ++ lib.optional web nodes.machine1.system.build.toplevel;
        };
      };
    machine1 = machine;
    machine2 = machine;
  }
  // lib.optionalAttrs web {
    client =
      { pkgs, ... }:
      {
        environment.systemPackages = [
          (pkgs.python3.withPackages (ps: [ ps.playwright ]))
        ];
        environment.variables = {
          PLAYWRIGHT_BROWSERS_PATH = "${pkgs.playwright-driver.browsers}";
          PLAYWRIGHT_SKIP_VALIDATE_HOST_REQUIREMENTS = "true";
          MNIX_PASSWORD_FILE = "${adminPassword}";
        };
        environment.etc."mnix-test/e2e.py".source = ./web/e2e.py;
        documentation.enable = false;
        virtualisation = {
          memorySize = 2048;
          cores = 2;
        };
      };
  };

  testScript =
    { nodes, ... }:
    let
      system = systemOf nodes;

      # The web UI, in a browser on `client`. Commands print their results
      # last.
      webScript = ''
        from datetime import timedelta

        def browser(args):
            try:
                out = client.succeed(
                    f"python3 /etc/mnix-test/e2e.py {args}", timeout=timedelta(minutes=25)
                )
            except Exception:
                client.execute("ls /tmp/e2e")
                client.copy_from_machine("/tmp/e2e", "screenshots")
                raise
            print(out)
            lines = out.strip().splitlines()
            return lines[-1] if lines else ""

        with subtest("a browser logs in, enrolls machine1, and configures it"):
            client.wait_for_unit("multi-user.target")
            token = browser("enroll machine1")
            assert token.startswith("mnix_enroll_"), token
            machine1.wait_for_unit("multi-user.target")
            machine1.succeed(f"echo {token} > ${tokenFile}")
            machine1.systemctl("restart mnix-agent.service")
            browser("configure machine1 infra")

        with subtest("the browser deploys revision a and follows it to the end"):
            deployment = browser("deploy machine1 a")
            d = json.loads(mnix(f"deploys show {deployment} --json"))
            assert d["status"] == "succeeded", d
            assert d["requested_by"] == "admin", d
            assert running(machine1) == "${system "machine1" "a"}"
            machine1.succeed("grep -qx a /etc/mnix-test/revision")

        with subtest("the machine page shows the revision and the generation diff"):
            browser("verify machine1 a")
      '';

      cliScript = ''
        with subtest("machines enroll and are configured"):
            for node in [machine1, machine2]:
                token = mnix(f"machine add {node.name}").strip()
                node.wait_for_unit("multi-user.target")
                node.succeed(f"echo {token} > ${tokenFile}")
                node.systemctl("restart mnix-agent.service")
                mnix(f"machine configure {node.name} --repo infra")
                mnix(f"machine tag {node.name} fleet")
            for node in [machine1, machine2]:
                wait_for_status(node.name, "online")
            booted = {node.name: running(node) for node in [machine1, machine2]}
            # Nothing of revision a is on the machines yet.
            machine1.fail("nix-store --check-validity ${system "machine1" "a"}")

        with subtest("revision a is built, fetched from Attic, and activated"):
            d = deploy("tag:fleet --rev a --as tester", expect_success=True)
            assert d["status"] == "succeeded", d
            assert d["rev"] == "a" and len(d["commit"]) == 40, d
            assert d["requested_by"] == "tester", d
            expected = {"machine1": "${system "machine1" "a"}", "machine2": "${system "machine2" "a"}"}
            for m in d["machines"]:
                assert m["status"] == "succeeded", m
                assert m["store_path"] == expected[m["machine"]], m
            for node in [machine1, machine2]:
                assert running(node) == expected[node.name]
                assert profile(node) == expected[node.name]
                node.succeed("grep -qx a /etc/mnix-test/revision")
                node.succeed("curl --fail --silent http://127.0.0.1:8000/ | grep -qx a")
                server.wait_until_succeeds(
                    "mnix --token-file /var/lib/manage-nix/admin-token machines --json"
                    f" | jq -e '.[] | select(.name == \"{node.name}\") | .current_system == \"{expected[node.name]}\"'",
                    timeout=60,
                )
            logs = mnix(f"deploys logs {d['id']}")
            assert "pushing to the Attic cache mnix" in logs, logs
            assert "signed with mnix-test-1" in logs, logs
            assert "the server confirmed the new system" in logs, logs

        with subtest("revision b fails its health checks and machine1 rolls back to a"):
            d = deploy("machine1 --rev b", expect_success=False)
            m = d["machines"][0]
            assert d["status"] == "failed" and m["status"] == "rolled_back", d
            assert m["report"]["failed_checks"], m
            assert "health checks did not pass" in m["message"], m
            assert running(machine1) == expected["machine1"]
            assert profile(machine1) == expected["machine1"]
            machine1.succeed("grep -qx a /etc/mnix-test/revision")
            machine1.wait_until_succeeds("curl --fail --silent http://127.0.0.1:8000/")

        with subtest("revision c cuts machine2 off and it rolls back to a by itself"):
            d = deploy("machine2 --rev c", expect_success=False)
            m = d["machines"][0]
            assert d["status"] == "failed" and m["status"] == "rolled_back", d
            assert "did not reach the server" in m["message"], m
            assert not m["report"]["failed_checks"], m
            assert running(machine2) == expected["machine2"]
            assert profile(machine2) == expected["machine2"]
            machine2.succeed("grep -qx a /etc/mnix-test/revision")
            wait_for_status("machine2", "online")
            machine2.succeed("journalctl -u mnix-agent | grep 'reconnecting to show'")

        with subtest("deployments are listed with their results"):
            listed = mnix("deploys")
            assert listed.count("failed") == 2 and "succeeded" in listed, listed
            shown = mnix(f"deploys show {d['id'][:8]}")
            assert "rolled_back" in shown and "machine2" in shown, shown

        with subtest("mnix rollback returns machine1 to the system it booted"):
            status, out = server.execute(
                "timeout 600 mnix --token-file /var/lib/manage-nix/admin-token rollback machine1"
            )
            print(out)
            assert status == 0, out
            d = json.loads(mnix("deploys --limit 1 --json"))[0]
            assert d["kind"] == "rollback" and d["status"] == "succeeded", d
            assert d["machines"][0]["store_path"] == booted["machine1"], d
            assert running(machine1) == booted["machine1"]
            machine1.fail("test -e /etc/mnix-test/revision")
            wait_for_status("machine1", "online")
            wait_for_status("machine2", "online")
      '';
    in
    ''
      import json
      import re

      def mnix(args, check=True):
          command = f"mnix --token-file /var/lib/manage-nix/admin-token {args}"
          if check:
              return server.succeed(command)
          return server.execute(command)

      def wait_for_status(name, status):
          server.wait_until_succeeds(
              "mnix --token-file /var/lib/manage-nix/admin-token machines --json"
              f" | jq -e '.[] | select(.name == \"{name}\") | .status == \"{status}\"'",
              timeout=120,
          )

      def deploy(args, expect_success):
          d = json.loads(mnix(f"deploy {args} --no-wait --json"))
          try:
              server.wait_until_succeeds(
                  f"mnix --token-file /var/lib/manage-nix/admin-token deploys show {d['id']} --json"
                  " | jq -e '.status == \"succeeded\" or .status == \"failed\"'",
                  timeout=900,
              )
          finally:
              print(mnix(f"deploys logs {d['id']}"))
          d = json.loads(mnix(f"deploys show {d['id']} --json"))
          assert (d["status"] == "succeeded") == expect_success, d
          return d

      def running(node):
          return node.succeed("readlink -f /run/current-system").strip()

      def profile(node):
          return node.succeed("readlink -f /nix/var/nix/profiles/system").strip()

      start_all()

      with subtest("the cache and the server start"):
          server.wait_for_unit("atticd.service")
          server.wait_for_open_port(8081)
          token = server.succeed(
              "atticd-atticadm make-token --sub mnix --validity 1y --create-cache '*'"
              " --pull '*' --push '*' --configure-cache '*'"
          ).strip()
          server.succeed(
              "mkdir -p /run/mnix-test",
              f"echo {token} > /run/mnix-test/attic-token",
              "chown manage-nix /run/mnix-test/attic-token",
              "chmod 0400 /run/mnix-test/attic-token",
              f"attic login local http://server:8081 {token}",
              "attic cache create --public mnix",
          )
          info = server.succeed("attic cache info mnix 2>&1")
          found = re.search(r"Public Key: (\S+)", info)
          assert found, info
          cache_key = found.group(1)
          for node in [machine1, machine2]:
              node.succeed(
                  "mkdir -p /run/mnix-test",
                  "echo 'extra-substituters = http://server:8081/mnix' > /run/mnix-test/nix.conf",
                  f"echo 'extra-trusted-public-keys = {cache_key}' >> /run/mnix-test/nix.conf",
              )

          server.systemctl("start mnix-server.service")
          server.wait_for_unit("mnix-server.service")
          server.wait_for_open_port(8080)
          assert mnix("signing-key").strip() == "${lib.fileContents testKey.public}"

      with subtest("the repository has three revisions"):
          server.succeed(
              "mkdir -p /srv/git/infra",
              "cd /srv/git/infra && git init --quiet --initial-branch=main",
              "cp ${repositoryFlake} /srv/git/infra/flake.nix",
          )
          for revision, module in [("a", "${revisions.a}"), ("b", "${revisions.b}"), ("c", "${revisions.c}")]:
              server.succeed(
                  f"cp {module} /srv/git/infra/revision.nix",
                  "cd /srv/git/infra && git add flake.nix revision.nix",
                  "cd /srv/git/infra && nix flake lock",
                  "cd /srv/git/infra && git add flake.lock",
                  f"cd /srv/git/infra && git -c user.name=test -c user.email=test@example.com commit --quiet -m {revision}",
                  f"cd /srv/git/infra && git tag {revision}",
              )
          # The server's git refuses repositories owned by someone else.
          server.succeed("chown -R manage-nix:manage-nix /srv/git/infra")
          mnix("repo add infra /srv/git/infra")
          assert "infra" in mnix("repos")
    ''
    + (if web then webScript else cliScript);
}
