# NixOS module for mnix-server, the manage.nix control plane.
#
# A local HTTPS proxy serves https://manage.nix by default. Disable
# localHTTPS.enable when using an existing TLS proxy instead.
#
# With `buildOn = "server"`, the server builds machines' systems itself, as
# the `manage-nix` user, through this machine's Nix daemon, signs them with
# its signing key, and pushes them to Attic if `attic.endpoint` is set. With
# `buildOn = "machine"`, it sends each machine the source to build.
self:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.manage-nix.server;
  common = import ./server-common.nix {
    inherit
      self
      config
      lib
      pkgs
      ;
    stateDir = "/var/lib/manage-nix";
    isDarwin = false;
  };
  inherit (common) stateDir;
  trustDir = "/var/lib/manage-nix-trust";
  # Keep the declarative CA bundle intact; only the local public root is
  # appended at activation/startup. Private keys never enter the Nix store.
  updateTrust = pkgs.writeShellScript "manage-nix-update-trust" ''
    set -eu
    install -d -m 0755 ${trustDir}
    bundle=$(mktemp ${trustDir}/bundle.XXXXXX)
    trap 'rm -f "$bundle"' EXIT
    cat ${config.security.pki.caBundle} > "$bundle"
    if [ -s ${trustDir}/root.crt ]; then
      printf '\n' >> "$bundle"
      cat ${trustDir}/root.crt >> "$bundle"
    fi
    chmod 0644 "$bundle"
    mv -f "$bundle" ${trustDir}/ca-bundle.crt
  '';
  trustSource = pkgs.runCommand "manage-nix-trust-source" { } ''
    mkdir -p $out/anchors
    ln -s ${
      (pkgs.cacert.override {
        blacklist = config.security.pki.caCertificateBlacklist;
        extraCertificateFiles = config.security.pki.certificateFiles;
        extraCertificateStrings = config.security.pki.certificates;
      }).p11kit
    }/etc/ssl/trust-source/ca-bundle.trust.p11-kit $out/ca-bundle.trust.p11-kit
    ln -s ${trustDir}/root.crt $out/anchors/manage-nix.crt
  '';
in
{
  inherit (common) options;

  config = lib.mkIf cfg.enable {
    inherit (common) assertions;

    environment.systemPackages = [
      cfg.package
    ]
    ++ lib.optional (cfg.cliPackage != null) cfg.cliPackage;

    networking.hosts = lib.mkIf cfg.localHTTPS.enable {
      "127.0.0.1" = [ "manage.nix" ];
    };

    environment.etc = lib.mkIf cfg.localHTTPS.enable {
      "ssl/certs/ca-certificates.crt".source = lib.mkForce "${trustDir}/ca-bundle.crt";
      "ssl/certs/ca-bundle.crt".source = lib.mkForce "${trustDir}/ca-bundle.crt";
      "pki/tls/certs/ca-bundle.crt".source = lib.mkForce "${trustDir}/ca-bundle.crt";
      "ssl/trust-source".source = lib.mkForce trustSource;
    };

    # Populate the runtime bundle before /etc links are activated, including
    # on first installation when Caddy has not generated its root yet.
    system.activationScripts.mnix-local-trust = lib.mkIf cfg.localHTTPS.enable {
      deps = [ "specialfs" ];
      text = "${updateTrust}";
    };
    system.activationScripts.etc.deps = lib.optional cfg.localHTTPS.enable "mnix-local-trust";

    systemd.services.mnix-https-trust = lib.mkIf cfg.localHTTPS.enable {
      description = "Trust the manage.nix local HTTPS certificate authority";
      wantedBy = [ "mnix-https.service" ];
      requires = [ "mnix-https.service" ];
      after = [ "mnix-https.service" ];
      partOf = [ "mnix-https.service" ];
      path = [
        pkgs.coreutils
        pkgs.openssl
      ];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        TimeoutStartSec = 60;
        UMask = "0022";
      };
      script = ''
        set -eu
        root=/var/lib/private/manage-nix-https/pki/authorities/local/root.crt
        until [ -s "$root" ]; do sleep 0.1; done
        # Parse and re-encode only the public certificate from the proxy's
        # private state; never copy arbitrary data from that directory.
        openssl x509 -in "$root" -out ${trustDir}/root.crt.new
        chmod 0644 ${trustDir}/root.crt.new
        mv -f ${trustDir}/root.crt.new ${trustDir}/root.crt
        ${updateTrust}
      '';
    };

    systemd.services.mnix-https = lib.mkIf cfg.localHTTPS.enable {
      description = "manage.nix local HTTPS proxy";
      wantedBy = [ "multi-user.target" ];
      after = [
        "network.target"
        "mnix-server.service"
      ];
      serviceConfig = {
        ExecStart = "${lib.getExe pkgs.caddy} run --config ${common.localHTTPSConfig} --adapter caddyfile";
        DynamicUser = true;
        StateDirectory = "manage-nix-https";
        StateDirectoryMode = "0700";
        AmbientCapabilities = [ "CAP_NET_BIND_SERVICE" ];
        CapabilityBoundingSet = [ "CAP_NET_BIND_SERVICE" ];
        NoNewPrivileges = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        Restart = "on-failure";
        RestartSec = 2;
      };
    };

    users.users.manage-nix = {
      isSystemUser = true;
      group = "manage-nix";
      home = stateDir;
    };
    users.groups.manage-nix = { };

    services.postgresql = lib.mkIf cfg.database.createLocally {
      enable = true;
      ensureDatabases = [ "manage-nix" ];
      ensureUsers = [
        {
          name = "manage-nix";
          ensureDBOwnership = true;
        }
      ];
    };

    systemd.services.mnix-server = {
      description = "manage.nix control plane";
      wantedBy = [ "multi-user.target" ];
      after = [ "network.target" ] ++ lib.optional cfg.database.createLocally "postgresql.target";
      requires = lib.optional cfg.database.createLocally "postgresql.target";
      environment = {
        MNIX_DATABASE_URL = cfg.database.url;
        RUST_LOG = lib.mkDefault "info";
      };
      # Builds run through the Nix daemon; git fetches repositories, over
      # ssh for repositories with an SSH key.
      path = [
        config.nix.package
        pkgs.git
        pkgs.openssh
      ]
      ++ lib.optional (cfg.attic.endpoint != null) pkgs.attic-client;
      serviceConfig = {
        ExecStart = lib.escapeShellArgs common.args;
        EnvironmentFile = lib.mkIf (cfg.environmentFile != null) cfg.environmentFile;
        User = "manage-nix";
        Group = "manage-nix";
        StateDirectory = "manage-nix";
        StateDirectoryMode = "0750";
        UMask = "0077";
        Restart = "on-failure";
        RestartSec = 2;

        NoNewPrivileges = true;
        PrivateTmp = true;
        PrivateDevices = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectControlGroups = true;
        RestrictAddressFamilies = [
          "AF_INET"
          "AF_INET6"
          "AF_UNIX"
        ];
        RestrictNamespaces = true;
        LockPersonality = true;
        MemoryDenyWriteExecute = true;
        SystemCallArchitectures = "native";
      };
    };
  };
}
