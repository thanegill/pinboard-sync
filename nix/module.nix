self:
{ config, lib, pkgs, ... }:
let
  cfg = config.services.pinboard-sync;
in
{
  options.services.pinboard-sync = {
    enable = lib.mkEnableOption "pinboard-sync: sync Reddit saved items to Pinboard";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalMD "the `pinboard-sync` package from this flake";
      description = "The pinboard-sync package to run.";
    };

    # Credentials can be supplied two ways: a single environmentFile (read by
    # systemd as root, so it works with the hardened DynamicUser and a sops-nix
    # rendered template), or the per-secret *File options below (when the files
    # are readable by the service).
    environmentFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      example = "/run/secrets/rendered/pinboard-sync-env";
      description = ''
        Path to a systemd `EnvironmentFile` providing the credentials as
        `REDDIT_USERNAME`, `REDDIT_COOKIE`, and `PINBOARD_TOKEN`. Read by systemd
        as root, so it works with the `DynamicUser` and a sops-nix rendered
        template. Use this *or* the `username` + `*File` options.
      '';
    };

    username = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = "Reddit username whose saved items to sync (not secret). Exported as REDDIT_USERNAME.";
    };

    cookieFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      description = ''
        Path to a file with the Reddit session cookie ("reddit_session=<value>").
        Exported as REDDIT_COOKIE_FILE.
      '';
    };

    pinboardTokenFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      description = ''Path to a file with the Pinboard API token ("user:TOKEN"). Exported as PINBOARD_TOKEN_FILE.'';
    };

    schedule = lib.mkOption {
      type = lib.types.str;
      default = "hourly";
      example = "*-*-* 03:00:00";
      description = "systemd OnCalendar schedule for the sync timer.";
    };

    onAuthFailure = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = ''notify-send "pinboard-sync needs a fresh cookie: $PINBOARD_SYNC_AUTH_ERROR"'';
      description = ''
        Shell command run when Reddit rejects the request (the `reddit_session`
        cookie expired or was reset). Runs via `sh -c` with
        `PINBOARD_SYNC_AUTH_ERROR` and `PINBOARD_SYNC_EVENT` in the
        environment (plus anything from `environmentFile`).
      '';
    };

    extraArgs = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      example = [ "--public" "--limit" "100" ];
      description = "Extra arguments passed to `pinboard-sync sync`.";
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion =
          cfg.environmentFile != null
          || (cfg.username != null && cfg.cookieFile != null && cfg.pinboardTokenFile != null);
        message = ''
          services.pinboard-sync: provide credentials via `environmentFile`, or set
          all of `username`, `cookieFile`, and `pinboardTokenFile`.
        '';
      }
    ];

    systemd.services.pinboard-sync = {
      description = "Sync saved/favorited items to Pinboard";
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
      startAt = cfg.schedule;

      environment =
        lib.optionalAttrs (cfg.username != null) { REDDIT_USERNAME = cfg.username; }
        // lib.optionalAttrs (cfg.cookieFile != null) {
          REDDIT_COOKIE_FILE = toString cfg.cookieFile;
        }
        // lib.optionalAttrs (cfg.pinboardTokenFile != null) {
          PINBOARD_TOKEN_FILE = toString cfg.pinboardTokenFile;
        }
        // lib.optionalAttrs (cfg.onAuthFailure != null) {
          PINBOARD_SYNC_ON_AUTH_FAILURE = cfg.onAuthFailure;
        };

      serviceConfig = {
        Type = "oneshot";
        DynamicUser = true;
        ExecStart = "${lib.getExe cfg.package} sync ${lib.escapeShellArgs cfg.extraArgs}";

        # Hardening. The auth-failure hook runs via `sh -c`, so keep it modest.
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
      } // lib.optionalAttrs (cfg.environmentFile != null) {
        EnvironmentFile = cfg.environmentFile;
      };
    };
  };
}
