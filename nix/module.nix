self:
{ config, lib, pkgs, ... }:
let
  cfg = config.services.pinboard-sync;

  tomlFormat = pkgs.formats.toml { };
  # Non-secret settings rendered to the store and passed via --config. Secrets are
  # NOT placed here (it lands in the world-readable nix store) — they come from the
  # *File env vars below.
  configFile = tomlFormat.generate "pinboard-sync.toml" cfg.settings;

  # Non-secret values + secret-file *paths* exported to the unit environment. Secret
  # values themselves stay in the referenced files (sops-nix), never in the env.
  environment =
    { PINBOARD_SYNC_CONFIG = toString configFile; }
    // lib.optionalAttrs (cfg.pinboardTokenFile != null) {
      PINBOARD_TOKEN_FILE = toString cfg.pinboardTokenFile;
    }
    // lib.optionalAttrs (cfg.reddit.username != null) { REDDIT_USERNAME = cfg.reddit.username; }
    // lib.optionalAttrs (cfg.reddit.cookieFile != null) {
      REDDIT_COOKIE_FILE = toString cfg.reddit.cookieFile;
    }
    // lib.optionalAttrs (cfg.github.tokenFile != null) {
      GITHUB_TOKEN_FILE = toString cfg.github.tokenFile;
    }
    // lib.optionalAttrs (cfg.hackernews.username != null) { HN_USERNAME = cfg.hackernews.username; }
    // lib.optionalAttrs (cfg.onAuthFailure != null) {
      PINBOARD_SYNC_ON_AUTH_FAILURE = cfg.onAuthFailure;
    };

  hardening = {
    DynamicUser = true;
    NoNewPrivileges = true;
    ProtectSystem = "strict";
    ProtectHome = true;
    PrivateTmp = true;
    PrivateDevices = true;
    ProtectKernelTunables = true;
    ProtectKernelModules = true;
    ProtectControlGroups = true;
    RestrictAddressFamilies = [ "AF_INET" "AF_INET6" ];
    RestrictNamespaces = true;
    LockPersonality = true;
  };

  # Build a oneshot service + timer running `pinboard-sync <args>`.
  mkService = description: args: {
    inherit description;
    after = [ "network-online.target" ];
    wants = [ "network-online.target" ];
    startAt = cfg.schedule;
    inherit environment;
    serviceConfig = {
      Type = "oneshot";
      ExecStart = "${lib.getExe cfg.package} ${lib.escapeShellArgs args}";
    } // hardening // lib.optionalAttrs (cfg.environmentFile != null) {
      EnvironmentFile = cfg.environmentFile;
    };
  };

  # `--config` is implied by PINBOARD_SYNC_CONFIG in the environment, so the verb
  # args are all that's needed. `mode = "all"` runs every configured account.
  syncArgs =
    if cfg.mode == "all" then
      [ "sync" "--all" ]
    else
      [ "sync" cfg.source ] ++ lib.optional (cfg.account != null) cfg.account;
  cleanupArgs =
    if cfg.mode == "all" then
      [ "cleanup" "--all" ]
    else
      [ "cleanup" cfg.source ] ++ lib.optional (cfg.account != null) cfg.account;
in
{
  options.services.pinboard-sync = {
    enable = lib.mkEnableOption "pinboard-sync: sync saved/favorited items to Pinboard";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalMD "the `pinboard-sync` package from this flake";
      description = "The pinboard-sync package to run.";
    };

    settings = lib.mkOption {
      type = tomlFormat.type;
      default = { };
      example = lib.literalExpression ''
        {
          pinboard.public = false;
          reddit = [ { name = "main"; username = "you"; } ];
          hackernews = [ { username = "you"; } ];
        }
      '';
      description = ''
        Non-secret config rendered to a TOML file and passed via `--config`. Mirrors
        the config schema (`[pinboard]`, `[hooks]`, and per-source account arrays),
        minus secrets — never put tokens/cookies here, as the file lands in the
        world-readable Nix store. Provide those via the `*File` options below, or as
        sops-nix `*_file` *paths* inside account tables (paths are not secret).
      '';
    };

    mode = lib.mkOption {
      type = lib.types.enum [ "all" "source" ];
      default = "all";
      description = ''
        `all` runs every configured account across every source. `source` runs the
        single `source` (optionally a named `account`).
      '';
    };

    source = lib.mkOption {
      type = lib.types.enum [ "reddit" "github" "hackernews" ];
      default = "reddit";
      description = "Source to run when `mode = \"source\"`.";
    };

    account = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = "Account name to select when `mode = \"source\"` (default: the first).";
    };

    cleanup = lib.mkEnableOption "a second timer running `cleanup` (reddit + hackernews)";

    schedule = lib.mkOption {
      type = lib.types.str;
      default = "hourly";
      example = "*-*-* 03:00:00";
      description = "systemd OnCalendar schedule for the timer(s).";
    };

    onAuthFailure = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = ''notify-send "pinboard-sync needs a fresh cookie: $PINBOARD_SYNC_AUTH_ERROR"'';
      description = ''
        Shell command run when a source needs re-authentication (e.g. an expired
        Reddit cookie). Runs via `sh -c` with `PINBOARD_SYNC_AUTH_ERROR` and
        `PINBOARD_SYNC_EVENT` in the environment. Exported as
        `PINBOARD_SYNC_ON_AUTH_FAILURE`.
      '';
    };

    environmentFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      example = "/run/secrets/rendered/pinboard-sync-env";
      description = ''
        Optional systemd `EnvironmentFile` providing any of `PINBOARD_TOKEN`,
        `REDDIT_USERNAME`/`REDDIT_COOKIE`, `GITHUB_TOKEN`, `HN_USERNAME` (or their
        `_FILE` variants). Read by systemd as root, so it works with `DynamicUser`
        and a sops-nix rendered template — an alternative to the `*File` options.
      '';
    };

    pinboardTokenFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      description = ''Path to a file with the Pinboard API token ("user:TOKEN"). Exported as PINBOARD_TOKEN_FILE.'';
    };

    reddit = {
      username = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "Reddit username (not secret). Exported as REDDIT_USERNAME.";
      };
      cookieFile = lib.mkOption {
        type = lib.types.nullOr lib.types.path;
        default = null;
        description = ''Path to a file with the Reddit session cookie ("reddit_session=…"). Exported as REDDIT_COOKIE_FILE.'';
      };
    };

    github.tokenFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      description = "Path to a file with a GitHub personal access token. Exported as GITHUB_TOKEN_FILE.";
    };

    hackernews.username = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = "HackerNews username (public). Exported as HN_USERNAME.";
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = cfg.environmentFile != null || cfg.pinboardTokenFile != null;
        message = "services.pinboard-sync: set `pinboardTokenFile` or `environmentFile` for the Pinboard token.";
      }
    ];

    systemd.services.pinboard-sync = mkService "Sync saved/favorited items to Pinboard" syncArgs;

    systemd.services.pinboard-sync-cleanup =
      lib.mkIf cfg.cleanup (mkService "Normalize existing Pinboard bookmarks" cleanupArgs);
  };
}
