# NixOS module for mnix-server. The service unit, database options, and
# `database.createLocally` are added in M2.
self:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.manage-nix.server;
in
{
  options.services.manage-nix.server = {
    enable = lib.mkEnableOption "the manage.nix control plane";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.mnix-server;
      defaultText = lib.literalExpression "manage-nix.packages.\${system}.mnix-server";
      description = "The mnix-server package to use.";
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [ cfg.package ];
  };
}
