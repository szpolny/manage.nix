{
  self,
  pkgs,
  nix-darwin,
}:
let
  inherit (pkgs) lib;
  evaluate =
    system: serverEnabled: httpsEnabled:
    (nix-darwin.lib.darwinSystem {
      inherit system;
      modules = [
        self.darwinModules.server
        {
          system.stateVersion = 6;
          services.manage-nix.server = {
            enable = serverEnabled;
            localHTTPS.enable = httpsEnabled;
          };
        }
      ];
    }).config;
  enabled = evaluate "aarch64-darwin" true true;
  disabled = evaluate "aarch64-darwin" true false;
  serverDisabled = evaluate "aarch64-darwin" false true;
  proxy = enabled.launchd.daemons.mnix-https.serviceConfig;
  # Render the same shared config with the check's host packages, so Linux
  # CI does not need a Darwin builder just to produce this text file.
  localHTTPSConfig =
    (import ../modules/server-common.nix {
      inherit self pkgs lib;
      config = enabled;
      stateDir = "/var/lib/manage-nix";
      isDarwin = true;
    }).localHTTPSConfig;
in
# Force full system evaluation: unknown options must fail even with HTTPS off.
builtins.deepSeq
  (map (c: c.system.build.toplevel.drvPath) [
    enabled
    disabled
    serverDisabled
  ])
  (
    assert enabled.services.manage-nix.server.secureCookies;
    assert !(disabled.launchd.daemons ? mnix-https);
    assert !(serverDisabled.launchd.daemons ? mnix-https);
    assert lib.hasInfix "local-https-trust.sh" enabled.system.activationScripts.postActivation.text;
    assert !(lib.hasInfix "local-https-trust.sh" disabled.system.activationScripts.postActivation.text);
    assert
      !(lib.hasInfix "local-https-trust.sh" serverDisabled.system.activationScripts.postActivation.text);
    pkgs.runCommand "manage-nix-darwin-local-https"
      {
        nativeBuildInputs = [
          pkgs.caddy
          pkgs.jq
        ];
      }
      ''
        # Parsing only: this does not start a listener or modify any trust store.
        caddy adapt --config ${localHTTPSConfig} --adapter caddyfile > proxy.json
        jq -e '.apps.http.servers[].listen == ["127.0.0.1:443"]' proxy.json
        jq -e '.apps.pki.certificate_authorities.local.install_trust == false' proxy.json

        printf '%s\n' '127.0.0.1 localhost' '192.0.2.1 example.test # keep me' > original
        cp original hosts
        chmod 0640 hosts
        ${pkgs.runtimeShell} ${../modules/darwin/local-https-hosts.sh} hosts true
        grep -Fx '127.0.0.1 manage.nix # manage-nix local HTTPS' hosts
        test "$(stat -c %a hosts)" = 640
        cp hosts once
        test -z "$(${pkgs.runtimeShell} ${../modules/darwin/local-https-hosts.sh} hosts true)"
        cmp hosts once
        ${pkgs.runtimeShell} ${../modules/darwin/local-https-hosts.sh} hosts false
        cmp hosts original
        test -z "$(${pkgs.runtimeShell} ${../modules/darwin/local-https-hosts.sh} hosts false)"

        # Unmarked user entries must survive both enabling and disabling HTTPS.
        printf '%s\n' '127.0.0.1 manage.nix custom-alias # user entry' >> hosts
        cp hosts original
        ${pkgs.runtimeShell} ${../modules/darwin/local-https-hosts.sh} hosts true
        ${pkgs.runtimeShell} ${../modules/darwin/local-https-hosts.sh} hosts false
        cmp hosts original
        # Simulate a certificate present in the Keychain but lacking trust.
        # The installer must repair admin SSL trust, and fail on any rejection.
        export trust_state="$PWD/trusted"
        export trust_calls="$PWD/trust-calls"
        cat > security <<'SECURITY'
        #!${pkgs.runtimeShell}
        set -eu
        case "$1" in
          verify-cert)
            test -f "$trust_state" && test -z "''${reject_verify:-}"
            ;;
          add-trusted-cert)
            printf '%s\n' "$*" >> "$trust_calls"
            test -z "''${reject_install:-}"
            touch "$trust_state"
            ;;
          *) exit 2 ;;
        esac
        SECURITY
        chmod +x security
        echo 'public test certificate' > root.crt
        ${pkgs.runtimeShell} ${../modules/darwin/local-https-trust.sh} "$PWD/security" root.crt
        grep -Fx 'add-trusted-cert -d -r trustRoot -p ssl -k /Library/Keychains/System.keychain root.crt' "$trust_calls"
        cp "$trust_calls" calls-once
        ${pkgs.runtimeShell} ${../modules/darwin/local-https-trust.sh} "$PWD/security" root.crt
        cmp "$trust_calls" calls-once
        rm "$trust_state"
        if reject_install=1 ${pkgs.runtimeShell} ${../modules/darwin/local-https-trust.sh} "$PWD/security" root.crt; then
          echo 'Trust installation rejection was ignored' >&2; exit 1
        fi
        if reject_verify=1 ${pkgs.runtimeShell} ${../modules/darwin/local-https-trust.sh} "$PWD/security" root.crt; then
          echo 'Trust verification rejection was ignored' >&2; exit 1
        fi
        if ${pkgs.runtimeShell} ${../modules/darwin/local-https-trust.sh} "$PWD/security" missing.crt 0; then
          echo 'Missing CA root was ignored' >&2; exit 1
        fi
        touch $out
      ''
  )
