# M2: machines enroll with the control plane and report what they run.
#
# One server (behind nginx with TLS, as in production) and two machines.
# Both enroll with one-time tokens and appear online with correct facts. A
# reused or expired token is rejected. A machine that stops sending
# heartbeats (its VM is killed, so the TCP connection never closes) shows as
# offline after the heartbeat timeout, and agents reconnect after the server
# restarts.
#
# Runs once per database backend: `backend` is "sqlite" or "postgres".
{
  self,
  pkgs,
  backend,
}:
let
  # A throwaway CA and a certificate for the host name `server`.
  certs =
    pkgs.runCommand "mnix-test-certs"
      {
        nativeBuildInputs = [ pkgs.openssl ];
      }
      ''
        mkdir $out
        openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
          -days 36500 -subj /CN=mnix-test-ca \
          -addext basicConstraints=critical,CA:TRUE \
          -addext keyUsage=critical,keyCertSign,cRLSign \
          -keyout ca.key -out $out/ca.crt
        openssl req -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
          -subj /CN=server -keyout $out/server.key -out server.csr
        cat > server.ext <<EOF
        subjectAltName = DNS:server
        basicConstraints = critical,CA:FALSE
        keyUsage = critical,digitalSignature
        extendedKeyUsage = serverAuth
        EOF
        openssl x509 -req -in server.csr -CA $out/ca.crt -CAkey ca.key -CAcreateserial \
          -days 36500 -extfile server.ext -out $out/server.crt
      '';

  tokenFile = "/run/mnix-enrollment-token";

  machine =
    { pkgs, ... }:
    {
      imports = [ self.nixosModules.agent ];
      services.manage-nix.agent = {
        enable = true;
        server.url = "https://server";
        enrollmentTokenFile = tokenFile;
      };
      security.pki.certificateFiles = [ "${certs}/ca.crt" ];
      environment.systemPackages = [ pkgs.curl ];
    };
in
pkgs.testers.runNixOSTest {
  name = "manage-nix-enroll-${backend}";

  nodes = {
    server =
      { pkgs, ... }:
      {
        imports = [
          self.nixosModules.server
          self.nixosModules.agent
        ];
        # Exercise a co-located agent, even before it has an enrollment token.
        services.manage-nix.agent = {
          enable = true;
          server.url = "https://server";
          enrollmentTokenFile = tokenFile;
        };
        services.manage-nix.server = {
          enable = true;
          database.createLocally = backend == "postgres";
          heartbeatInterval = 2;
          heartbeatTimeout = 6;
        };
        services.nginx = {
          enable = true;
          virtualHosts.server = {
            onlySSL = true;
            sslCertificate = "${certs}/server.crt";
            sslCertificateKey = "${certs}/server.key";
            locations."/" = {
              proxyPass = "http://127.0.0.1:8080";
              proxyWebsockets = true;
            };
          };
        };
        networking.firewall.allowedTCPPorts = [ 443 ];
        environment.systemPackages = [
          pkgs.jq
          pkgs.git
        ];
      };
    machine1 = machine;
    machine2 = machine;
  };

  testScript = ''
    import json

    backend = "${backend}"

    def mnix(args):
        return server.succeed(
            f"mnix --token-file /var/lib/manage-nix/admin-token {args}"
        )

    def listed_machines():
        return {m["name"]: m for m in json.loads(mnix("machines --json"))}

    def wait_for_status(name, status):
        server.wait_until_succeeds(
            "mnix --token-file /var/lib/manage-nix/admin-token machines --json"
            f" | jq -e '.[] | select(.name == \"{name}\") | .status == \"{status}\"'",
            timeout=60,
        )

    def enroll_status(node, token):
        body = json.dumps({"token": token})
        return node.succeed(
            "curl --silent --output /dev/null --write-out '%{http_code}'"
            " -H 'Content-Type: application/json'"
            f" --data '{body}' https://server/api/agent/enroll"
        )

    start_all()
    server.wait_for_unit("mnix-server.service")
    server.wait_for_unit("nginx.service")
    server.wait_for_open_port(8080)
    server.wait_for_open_port(443)

    with subtest("agent restarts preserve server state access"):
        server.succeed("systemctl restart mnix-agent.service")
        server.succeed("test $(stat -c %U:%a /var/lib/manage-nix) = manage-nix:750")
        server.succeed("runuser -u manage-nix -- test -r /var/lib/manage-nix/admin-token")
        mnix("machines --json")

    with subtest("co-located source migration survives server ownership repair"):
        server.systemctl("stop mnix-agent.service")
        server.succeed(
            "rm -f /var/lib/manage-nix/sources",
            "rmdir /var/lib/manage-nix-agent/sources",
            "mkdir -p /var/lib/manage-nix/sources",
            "git init --bare /var/lib/manage-nix/sources/default.git",
            "chown -R manage-nix:manage-nix /var/lib/manage-nix/sources",
        )
        server.systemctl("start mnix-agent.service")
        server.succeed(
            "test $(stat -c %U:%a /var/lib/manage-nix-agent) = root:700",
            "git -C /var/lib/manage-nix-agent/sources/default.git for-each-ref",
            # Force systemd to repair the server's directory recursively.
            "chown root:root /var/lib/manage-nix",
        )
        server.systemctl("restart mnix-server.service")
        server.succeed(
            "git -C /var/lib/manage-nix-agent/sources/default.git for-each-ref",
            "test $(stat -c %U /var/lib/manage-nix-agent/sources/default.git) = root",
        )
        server.systemctl("restart mnix-agent.service")
        server.succeed(
            "git -C /var/lib/manage-nix-agent/sources/default.git for-each-ref",
            "test $(stat -c %U /var/lib/manage-nix-agent/sources/default.git) = root",
            "test $(stat -c %U /var/lib/manage-nix) = manage-nix",
            "runuser -u manage-nix -- test -r /var/lib/manage-nix/admin-token",
        )
        mnix("machines --json")

    with subtest(f"the server uses {backend}"):
        server.wait_until_succeeds(
            f"journalctl -u mnix-server | grep -i '{backend} database'"
        )
        if backend == "sqlite":
            server.succeed("test -f /var/lib/manage-nix/db.sqlite")
        else:
            server.fail("test -e /var/lib/manage-nix/db.sqlite")
            server.succeed(
                "sudo -u postgres psql -d manage-nix -tAc 'SELECT count(*) FROM machines'"
            )

    with subtest("the admin API needs the admin token"):
        server.fail("mnix --token not-the-admin-token machines")
        assert "No machines" in mnix("machines")

    tokens = {}
    with subtest("machines enroll with one-time tokens"):
        for node in [machine1, machine2]:
            token = mnix(f"machine add {node.name}").strip()
            assert token.startswith("mnix_enroll_"), token
            tokens[node.name] = token
            assert listed_machines()[node.name]["status"] == "pending"
            node.wait_for_unit("multi-user.target")
            node.succeed(f"echo {token} > ${tokenFile}")
            node.systemctl("restart mnix-agent.service")
        for node in [machine1, machine2]:
            wait_for_status(node.name, "online")
            node.succeed("test $(stat -c %a /var/lib/manage-nix-agent/servers/default/credential.json) = 600")

    with subtest("online machines report correct facts"):
        listed = listed_machines()
        for node in [machine1, machine2]:
            m = listed[node.name]
            system = node.succeed("readlink -f /run/current-system").strip()
            assert m["status"] == "online", m
            assert m["current_system"] == system, m
            assert m["facts"]["hostname"] == node.name, m
            assert m["facts"]["kernel"] == node.succeed("uname -r").strip(), m
            assert m["enrolled_at"] and m["last_seen_at"] and m["agent_version"], m
        table = mnix("machines")
        assert "machine1" in table and "online" in table, table
        shown = json.loads(mnix(f"machine show {listed['machine1']['id']}"))
        assert shown["name"] == "machine1", shown

    with subtest("existing enrollment and activation paths survive migration"):
        node = machine1
        node.systemctl("stop mnix-agent.service")
        # Emulate the old layout, without retaining a usable enrollment token.
        node.succeed(
            "rm ${tokenFile}",
            "mkdir -p /var/lib/manage-nix",
            "cp /var/lib/manage-nix-agent/servers/default/credential.json /run/credential-before",
            "touch /var/lib/manage-nix-agent/activations/migration-marker",
            "mv /var/lib/manage-nix-agent/servers /var/lib/manage-nix/servers",
            "mv /var/lib/manage-nix-agent/activations /var/lib/manage-nix/activations",
            "chown -R nobody:nogroup /var/lib/manage-nix",
        )
        node.systemctl("start mnix-agent.service")
        wait_for_status(node.name, "online")
        node.succeed(
            "cmp /run/credential-before /var/lib/manage-nix-agent/servers/default/credential.json",
            "test -f /var/lib/manage-nix/activations/migration-marker",
            "test $(stat -c %U:%a /var/lib/manage-nix-agent/servers/default/credential.json) = root:600",
        )
        node.systemctl("restart mnix-agent.service")
        wait_for_status(node.name, "online")

    with subtest("a reused enrollment token is rejected"):
        assert enroll_status(machine1, tokens["machine1"]) == "401"
        server.succeed("journalctl -u mnix-server | grep 'already_used token'")

    with subtest("an expired enrollment token is rejected"):
        token = mnix("machine add late --expires-in 1s").strip()
        machine1.sleep(2)
        assert enroll_status(machine1, token) == "401"
        server.succeed("journalctl -u mnix-server | grep 'expired token'")
        assert listed_machines()["late"]["status"] == "pending"
        # A new token for the same machine works.
        token = mnix("machine token late").strip()
        assert enroll_status(machine1, token) == "200"

    with subtest("agents reconnect after the server restarts"):
        server.systemctl("restart mnix-server.service")
        for node in [machine1, machine2]:
            wait_for_status(node.name, "online")

    with subtest("a stopped agent is offline at once"):
        machine1.systemctl("stop mnix-agent.service")
        wait_for_status("machine1", "offline")
        machine1.systemctl("start mnix-agent.service")
        wait_for_status("machine1", "online")

    with subtest("a machine that stops sending heartbeats goes offline"):
        machine2.crash()
        wait_for_status("machine2", "offline")
        listed = listed_machines()
        assert listed["machine1"]["status"] == "online", listed
        server.succeed("journalctl -u mnix-server | grep 'machine2: disconnected (heartbeat_timeout)'")
  '';
}
