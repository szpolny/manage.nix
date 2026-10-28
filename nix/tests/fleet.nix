# 0.2: one machine, two independent servers, no cache.
#
# `alpha` and `beta` each run mnix-server with `buildOn = "machine"` and
# declare the machine, with a pre-shared enrollment token, and the
# repository it is built from. The machine's agent connects to both; it lets
# alpha (but not beta) make it build.
#
#   1. The machine enrolls with both servers on its own.
#   2. alpha deploys revision a: the machine downloads the source from
#      alpha, builds its own system, activates it, and both servers see it.
#   3. beta cannot make the machine build.
#   4. With alpha down, beta still rolls the machine back.
#   5. alpha deploys a again: the machine has the commit already.
#
# As in deploy.nix, the VMs are offline: the repository's flake evaluates
# this test's machine with the revision's module, and the result is in the
# machine's store already, so `nix build` on the machine evaluates the flake
# from Git and finds its output present.
{ self, pkgs }:
let
  inherit (pkgs) lib;
  revisions.a = ./deploy/revision-a.nix;
  tokens = {
    alpha = pkgs.writeText "token-alpha" "mnix_enroll_alpha_0123456789abcdef";
    beta = pkgs.writeText "token-beta" "mnix_enroll_beta_0123456789abcdef";
  };

  server =
    name:
    { ... }:
    {
      imports = [ self.nixosModules.server ];
      services.manage-nix.server = {
        enable = true;
        listenAddress = "0.0.0.0";
        buildOn = "machine";
        heartbeatInterval = 2;
        heartbeatTimeout = 6;
        confirmTimeout = 20;
        repositories.infra.url = "/srv/git/infra";
        machines.machine = {
          enrollmentTokenFile = "/etc/mnix-test/token";
          repository = "infra";
          tags = [ "fleet" ];
        };
      };
      # Started by the test once the repository exists.
      systemd.services.mnix-server.wantedBy = lib.mkForce [ ];
      environment.etc."mnix-test/token" = {
        source = tokens.${name};
        mode = "0400";
        user = "manage-nix";
      };
      environment.systemPackages = [
        pkgs.git
        pkgs.jq
      ];
      networking.firewall.allowedTCPPorts = [ 8080 ];
      documentation.enable = false;
    };

  machine =
    {
      config,
      extendModules,
      pkgs,
      ...
    }:
    {
      imports = [ self.nixosModules.agent ];
      services.manage-nix.agent = {
        enable = true;
        servers = {
          alpha = {
            url = "http://alpha:8080";
            enrollmentTokenFile = "${tokens.alpha}";
            allowBuilds = true;
          };
          beta = {
            url = "http://beta:8080";
            enrollmentTokenFile = "${tokens.beta}";
          };
        };
      };
      nix.settings = {
        substituters = lib.mkForce [ ];
        experimental-features = [
          "nix-command"
          "flakes"
        ];
      };
      system.switch.enable = true;
      boot.loader.grub.enable = false;
      documentation.enable = false;
      environment.systemPackages = [
        pkgs.curl
        pkgs.git
      ];
      virtualisation = {
        memorySize = 4096;
        diskSize = 8192;
        cores = 2;
        useNixStoreImage = true;
        writableStore = true;
        # What `nix build` will find, and the flake's inputs.
        additionalPaths = [
          repositoryFlake
          (config.system.build.mnixWithRevision revisions.a).config.system.build.toplevel
        ];
      };

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

      system.build.mnixWithRevision = revision: extendModules { modules = [ revision ]; };
    };

  hostSystem = pkgs.stdenv.hostPlatform.system;

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
          test = import "''${manage-nix}/nix/tests/fleet.nix" {
            self = manage-nix;
            pkgs = nixpkgs.legacyPackages.${hostSystem};
          };
        in
        {
          nixosConfigurations.machine =
            test.nodes.machine.system.build.mnixWithRevision ./revision.nix;
        };
    }
  '';
in
pkgs.testers.runNixOSTest {
  name = "manage-nix-fleet";

  nodes = {
    alpha = server "alpha";
    beta = server "beta";
    inherit machine;
  };

  testScript =
    { nodes, ... }:
    let
      systemA = (nodes.machine.system.build.mnixWithRevision revisions.a).config.system.build.toplevel;
    in
    ''
      import json

      def mnix(server, args):
          return server.succeed(f"mnix --token-file /var/lib/manage-nix/admin-token {args}")

      def machine_on(server):
          return json.loads(mnix(server, "machines --json"))[0]

      def wait_for(server, condition):
          server.wait_until_succeeds(
              "mnix --token-file /var/lib/manage-nix/admin-token machines --json"
              f" | jq -e '.[0] | {condition}'",
              timeout=180,
          )

      def deploy(server, args, expect_success):
          d = json.loads(mnix(server, f"deploy {args} --no-wait --json"))
          try:
              server.wait_until_succeeds(
                  f"mnix --token-file /var/lib/manage-nix/admin-token deploys show {d['id']} --json"
                  " | jq -e '.status == \"succeeded\" or .status == \"failed\"'",
                  timeout=900,
              )
          finally:
              print(mnix(server, f"deploys logs {d['id']}"))
          d = json.loads(mnix(server, f"deploys show {d['id']} --json"))
          assert (d["status"] == "succeeded") == expect_success, d
          return d

      def running():
          return machine.succeed("readlink -f /run/current-system").strip()

      start_all()

      with subtest("both servers have the same repository and start"):
          commits = []
          for server in [alpha, beta]:
              server.succeed(
                  "mkdir -p /srv/git/infra",
                  "cd /srv/git/infra && git init --quiet --initial-branch=main",
                  "cp ${repositoryFlake} /srv/git/infra/flake.nix",
                  "cp ${revisions.a} /srv/git/infra/revision.nix",
                  "cd /srv/git/infra && git add flake.nix revision.nix",
                  # The same commit on both servers.
                  "cd /srv/git/infra && GIT_AUTHOR_DATE=2026-10-01T00:00:00Z"
                  " GIT_COMMITTER_DATE=2026-10-01T00:00:00Z"
                  " git -c user.name=test -c user.email=test@example.com commit --quiet -m a",
                  "cd /srv/git/infra && git tag a",
              )
              commits.append(server.succeed("git -C /srv/git/infra rev-parse a").strip())
              # The server's git refuses repositories owned by someone else.
              server.succeed("chown -R manage-nix:manage-nix /srv/git/infra")
              server.systemctl("start mnix-server.service")
              server.wait_for_open_port(8080)
          commit = commits[0]
          assert commits == [commit, commit], commits

      with subtest("the machine enrolls with both servers on its own"):
          machine.wait_for_unit("mnix-agent.service")
          for server in [alpha, beta]:
              wait_for(server, '.status == "online"')
              m = machine_on(server)
              assert m["name"] == "machine" and m["tags"] == ["fleet"], m
              assert m["repository"] == "infra" and m["configuration"] == "machine", m
              assert m["facts"]["os"] == "nixos", m
          machine.succeed("test -f /var/lib/manage-nix/servers/alpha/credential.json")
          machine.succeed("test -f /var/lib/manage-nix/servers/beta/credential.json")
          booted = running()

      with subtest("alpha deploys revision a, which the machine builds itself"):
          d = deploy(alpha, "machine --rev a", expect_success=True)
          assert d["commit"] == commit, d
          m = d["machines"][0]
          assert m["status"] == "succeeded" and m["store_path"] == "${systemA}", m
          assert running() == "${systemA}"
          machine.succeed("grep -qx a /etc/mnix-test/revision")
          logs = mnix(alpha, f"deploys logs {d['id']}")
          assert "machine builds nixosConfigurations" in logs, logs
          assert "downloaded" in logs and "built /nix/store/" in logs, logs
          assert "the server confirmed the new system" in logs, logs
          machine.succeed(f"git -C /var/lib/manage-nix/sources/alpha.git cat-file -e {commit}")
          # beta sees the new system too.
          wait_for(beta, '.current_system == "${systemA}"')

      with subtest("beta may not make the machine build"):
          d = deploy(beta, "machine --rev a", expect_success=False)
          m = d["machines"][0]
          assert "does not build for the server" in m["message"], m
          assert running() == "${systemA}"

      with subtest("with alpha down, beta still rolls the machine back"):
          alpha.systemctl("stop mnix-server.service")
          wait_for(beta, '.status == "online"')
          status, out = beta.execute(
              "timeout 600 mnix --token-file /var/lib/manage-nix/admin-token rollback machine"
          )
          print(out)
          assert status == 0, out
          assert running() == booted
          machine.fail("test -e /etc/mnix-test/revision")

      with subtest("alpha comes back and deploys a from what the machine has"):
          alpha.systemctl("start mnix-server.service")
          wait_for(alpha, '.status == "online"')
          d = deploy(alpha, "machine --rev a", expect_success=True)
          assert running() == "${systemA}"
          logs = mnix(alpha, f"deploys logs {d['id']}")
          assert f"already have commit {commit}" in logs, logs
    '';
}
