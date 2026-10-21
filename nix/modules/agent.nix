# NixOS module for mnix-agent. The agent daemon, health checks, and
# enrollment are added from M1 on.
self:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.manage-nix.agent;
in
{
  options.services.manage-nix.agent = {
    enable = lib.mkEnableOption "the manage.nix agent";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.mnix-agent;
      defaultText = lib.literalExpression "manage-nix.packages.\${system}.mnix-agent";
      description = "The mnix-agent package to use.";
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [ cfg.package ];
  };
}
