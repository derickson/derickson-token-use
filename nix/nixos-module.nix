# NixOS module for the token-use collector.
#
# Defines token-use as a systemd *user* service at the system level. This is the
# alternative to the home-manager module (token-use.homeModules.default) — use
# one or the other, not both. Prefer the home-manager module if your per-user
# config already flows through home-manager.
#
#   imports = [ token-use.nixosModules.default ];
#   services.token-use.enable = true;
#   services.token-use.lingerUsers = [ "dave" ];   # run without an active login
self:
{ config, lib, pkgs, ... }:

let
  cfg = config.services.token-use;
in
{
  options.services.token-use = {
    enable = lib.mkEnableOption "the token-use AI token-usage collector (systemd user service)";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.token-use;
      defaultText = lib.literalExpression "token-use.packages.\${system}.token-use";
      description = "The token-use package to run.";
    };

    logLevel = lib.mkOption {
      type = lib.types.str;
      default = "info";
      description = "Operational log level on stderr (RUST_LOG).";
    };

    lingerUsers = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      example = [ "dave" ];
      description = ''
        Users for whom to enable systemd lingering, so the per-user token-use
        service runs without an active login session. The service itself is
        registered for every user's systemd instance; lingering just keeps it
        alive at boot for these users.
      '';
    };

    extraEnvironment = lib.mkOption {
      type = lib.types.attrsOf lib.types.str;
      default = { };
      description = "Extra environment variables for the daemon (e.g. TOKEN_USE_DEBOUNCE_MS, TOKEN_USE_TICK_SECS).";
    };
  };

  config = lib.mkIf cfg.enable {
    # %h / %S expand per-user at runtime, so this one unit works for any user.
    systemd.user.services.token-use = {
      description = "token-use: local AI token-usage collector (NDJSON for Elasticsearch)";
      documentation = [ "https://github.com/derickson/derickson-token-use" ];
      after = [ "default.target" ];
      wantedBy = [ "default.target" ];
      serviceConfig = {
        Type = "simple";
        ExecStart = lib.getExe cfg.package;
        Restart = "on-failure";
        RestartSec = 5;
      };
      environment = {
        TOKEN_USE_OUT_DIR = "%h/.local/share/token-use/logs";
        TOKEN_USE_STATE_DIR = "%h/.local/state/token-use";
        RUST_LOG = cfg.logLevel;
      } // cfg.extraEnvironment;
    };

    users.users = lib.genAttrs cfg.lingerUsers (_: { linger = true; });
  };
}
