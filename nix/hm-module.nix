# home-manager module for the token-use collector.
#
# Registers token-use as a systemd *user* service — the NixOS equivalent of the
# systemd user unit that install.sh drops into ~/.config/systemd/user. Because
# it runs as your user it can read ~/.claude and write into ~/.local/share.
#
# Wire it up in your home-manager config:
#   imports = [ token-use.homeModules.default ];
#   services.token-use.enable = true;
self:
{ config, lib, pkgs, ... }:

let
  cfg = config.services.token-use;
  home = config.home.homeDirectory;
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

    outDir = lib.mkOption {
      type = lib.types.str;
      default = "${home}/.local/share/token-use/logs";
      defaultText = lib.literalExpression ''"''${config.home.homeDirectory}/.local/share/token-use/logs"'';
      description = "Directory the NDJSON output is written to (TOKEN_USE_OUT_DIR). A shipper (Filebeat/Elastic Agent) tails this.";
    };

    stateDir = lib.mkOption {
      type = lib.types.str;
      default = "${home}/.local/state/token-use";
      defaultText = lib.literalExpression ''"''${config.home.homeDirectory}/.local/state/token-use"'';
      description = "Durable checkpoint directory (TOKEN_USE_STATE_DIR): per-file offsets + recent-id guard.";
    };

    logLevel = lib.mkOption {
      type = lib.types.str;
      default = "info";
      description = "Operational log level on stderr (RUST_LOG).";
    };

    extraEnvironment = lib.mkOption {
      type = lib.types.attrsOf lib.types.str;
      default = { };
      example = lib.literalExpression ''{ TOKEN_USE_DEBOUNCE_MS = "1500"; TOKEN_USE_TICK_SECS = "300"; }'';
      description = "Extra environment variables for the daemon (e.g. TOKEN_USE_DEBOUNCE_MS, TOKEN_USE_TICK_SECS, TOKEN_USE_HOME).";
    };
  };

  config = lib.mkIf cfg.enable {
    systemd.user.services.token-use = {
      Unit = {
        Description = "token-use: local AI token-usage collector (NDJSON for Elasticsearch)";
        Documentation = "https://github.com/derickson/derickson-token-use";
        After = [ "default.target" ];
      };

      Service = {
        Type = "simple";
        ExecStart = lib.getExe cfg.package;
        Restart = "on-failure";
        RestartSec = 5;
        Environment =
          lib.mapAttrsToList (n: v: "${n}=${v}") ({
            TOKEN_USE_OUT_DIR = cfg.outDir;
            TOKEN_USE_STATE_DIR = cfg.stateDir;
            RUST_LOG = cfg.logLevel;
          } // cfg.extraEnvironment);
      };

      Install.WantedBy = [ "default.target" ];
    };
  };
}
