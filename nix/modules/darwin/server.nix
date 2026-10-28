# nix-darwin module for mnix-server: the same options as the NixOS module,
# run as a launchd daemon under its own hidden user, `_mnix`, with its state
# in /var/lib/manage-nix and its log in /var/lib/manage-nix/server.log.
#
# A Mac cannot build NixOS systems without a Linux builder: with
# `buildOn = "machine"` (the usual choice here), each machine builds its own
# system from the source this server sends.
self:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.manage-nix.server;
  stateDir = "/var/lib/manage-nix";
  user = "_mnix";
  common = import ../server-common.nix {
    inherit
      self
      config
      lib
      pkgs
      stateDir
      ;
    isDarwin = true;
  };
in
{
  options = lib.recursiveUpdate common.options {
    services.manage-nix.server = {
      uid = lib.mkOption {
        type = lib.types.int;
        default = 450;
        description = "UID of the server's `${user}` user.";
      };
      gid = lib.mkOption {
        type = lib.types.int;
        default = 450;
        description = "GID of the server's `${user}` group.";
      };
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = common.assertions ++ [
      {
        assertion = !cfg.database.createLocally;
        message = "services.manage-nix.server.database.createLocally is not supported on nix-darwin; use SQLite or an existing Postgres.";
      }
      {
        assertion = cfg.environmentFile == null;
        message = "services.manage-nix.server.environmentFile is not supported on nix-darwin.";
      }
    ];

    environment.systemPackages = [
      cfg.package
    ]
    ++ lib.optional (cfg.cliPackage != null) cfg.cliPackage;

    users.knownUsers = [ user ];
    users.knownGroups = [ user ];
    users.users.${user} = {
      inherit (cfg) uid gid;
      home = stateDir;
      createHome = false;
      shell = "/usr/bin/false";
      isHidden = true;
      description = "manage.nix control plane";
    };
    users.groups.${user} = {
      inherit (cfg) gid;
      members = [ user ];
      description = "manage.nix control plane";
    };

    system.activationScripts.postActivation.text = lib.mkAfter ''
      mkdir -p ${stateDir}
      chown ${user}:${user} ${stateDir}
      chmod 0750 ${stateDir}
    '';

    launchd.daemons.mnix-server.serviceConfig = {
      ProgramArguments = common.args;
      UserName = user;
      GroupName = user;
      WorkingDirectory = stateDir;
      EnvironmentVariables = {
        MNIX_DATABASE_URL = cfg.database.url;
        RUST_LOG = "info";
        # git (and ssh) fetch repositories; nix builds with buildOn = "server".
        PATH = lib.concatStringsSep ":" [
          "${pkgs.git}/bin"
          "${pkgs.openssh}/bin"
          "/nix/var/nix/profiles/default/bin"
          "/run/current-system/sw/bin"
          "/usr/bin"
          "/bin"
        ];
      };
      # 0077
      Umask = 63;
      RunAtLoad = true;
      KeepAlive = true;
      ThrottleInterval = 5;
      StandardOutPath = "${stateDir}/server.log";
      StandardErrorPath = "${stateDir}/server.log";
    };
  };
}
