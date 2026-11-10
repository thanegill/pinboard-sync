self:
{
  config,
  lib,
  pkgs,
  utils,
  ...
}:
let
  cfg = config.services.pinboard-sync;

  sources = [
    "reddit"
    "github"
    "hackernews"
  ];

  # Each `*File` option's systemd credential and the `<VAR>_FILE` variable naming it.
  credentialFiles = [
    {
      option = "pinboardTokenFile";
      credential = "pinboard-token";
      variable = "PINBOARD_TOKEN_FILE";
    }
    {
      option = "redditUsernameFile";
      credential = "reddit-username";
      variable = "REDDIT_USERNAME_FILE";
    }
    {
      option = "redditCookieFile";
      credential = "reddit-cookie";
      variable = "REDDIT_COOKIE_FILE";
    }
    {
      option = "githubTokenFile";
      credential = "github-token";
      variable = "GITHUB_TOKEN_FILE";
    }
    {
      option = "hnUsernameFile";
      credential = "hn-username";
      variable = "HN_USERNAME_FILE";
    }
  ];
  setCredentialFiles = lib.filter (c: cfg.${c.option} != null) credentialFiles;

  escapeSpecifiers = lib.replaceStrings [ "%" ] [ "%%" ];

  # Drop accounts with `enable = false` and strip the key, which the binary rejects.
  pruneAccounts =
    accounts: map (a: removeAttrs a [ "enable" ]) (lib.filter (a: a.enable or true) accounts);

  tomlFormat = pkgs.formats.toml { };
  # Rendered to the world-readable store, so it must never hold a secret.
  configSettings =
    cfg.settings
    // lib.mapAttrs (_: pruneAccounts) (lib.filterAttrs (name: _: lib.elem name sources) cfg.settings);
  configFile = tomlFormat.generate "pinboard-sync.toml" cfg.renderedSettings;

  # Credential paths in the rendered config. They count toward the credentials
  # assertion, which can't tell whether the DynamicUser can read them.
  settingsCredentialFiles =
    lib.optional (
      (configSettings.pinboard.token_file or null) != null
    ) configSettings.pinboard.token_file
    ++ lib.filter (p: p != null) (
      map (a: a.cookie_file or null) (configSettings.reddit or [ ])
      ++ map (a: a.token_file or null) (configSettings.github or [ ])
    );

  # `backup` needs the Pinboard token itself, not just any credential.
  pinboardTokenConfigured =
    cfg.environmentFile != null
    || cfg.pinboardTokenFile != null
    || (configSettings.pinboard.token_file or null) != null;

  # Must match the backup unit's `StateDirectory`, its only persistent writable path.
  stateDir = "/var/lib/pinboard-sync";

  # A hardened oneshot service and timer running `pinboard-sync <args>`. The unit
  # environment holds the config path, the hook and `<VAR>_FILE` paths, never a
  # credential value. systemd also expands specifiers in `EnvironmentFile=` and
  # `LoadCredential=`, so every user value doubles its `%`. `extraServiceConfig` is
  # merged last, over the hardening.
  mkService =
    {
      description,
      schedule,
      args,
      extraServiceConfig ? { },
    }:
    {
      inherit description;
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
      startAt = schedule;
      environment = {
        PINBOARD_SYNC_CONFIG = toString configFile;
      }
      // lib.optionalAttrs (cfg.onAuthFailure != null) {
        PINBOARD_SYNC_ON_AUTH_FAILURE = escapeSpecifiers cfg.onAuthFailure;
      }
      // lib.listToAttrs (map (c: lib.nameValuePair c.variable "%d/${c.credential}") setCredentialFiles);
      serviceConfig = {
        Type = "oneshot";
        ExecStart = utils.escapeSystemdExecArgs ([ (lib.getExe cfg.package) ] ++ args);
        DynamicUser = true;
        NoNewPrivileges = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        PrivateDevices = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectControlGroups = true;
        RestrictAddressFamilies = [
          "AF_INET"
          "AF_INET6"
        ];
        RestrictNamespaces = true;
        LockPersonality = true;
      }
      // lib.optionalAttrs (cfg.environmentFile != null) {
        EnvironmentFile = escapeSpecifiers (toString cfg.environmentFile);
      }
      // lib.optionalAttrs (setCredentialFiles != [ ]) {
        LoadCredential = map (
          c: "${c.credential}:${escapeSpecifiers (toString cfg.${c.option})}"
        ) setCredentialFiles;
      }
      // extraServiceConfig;
    };
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
      inherit (tomlFormat) type;
      default = { };
      example = lib.literalExpression ''
        {
          pinboard.public = false;
          reddit = [
            { name = "main"; username = "you"; }
            { enable = false; name = "alt"; username = "other"; }  # kept here, not synced
          ];
          hackernews = [ { username = "you"; } ];
        }
      '';
      description = ''
        Config file contents (`[pinboard]`, `[hooks]` and the per-source account
        arrays), rendered to TOML and passed via `--config`. The file lands in the
        world-readable Nix store, so never put a token or cookie here: use
        `environmentFile`, a `*File` option, or a `*_file` path in the config.

        The binary opens a `*_file` path in the config (`[pinboard].token_file`, a
        reddit `cookie_file`, a github `token_file`) as the service's `DynamicUser`, so
        that user must be able to read it (e.g. a sops-nix secret with `mode = "0444"`,
        or a group the service is in). A root-owned `0400` secret passes the assertions
        but fails every run with a missing-credential error. systemd reads
        `environmentFile` and the `*File` options as root; use those for such a secret.

        An account with `enable = false` stays here but is left out of every run.
      '';
    };

    renderedSettings = lib.mkOption {
      inherit (tomlFormat) type;
      default = configSettings;
      defaultText = lib.literalMD "`settings` with disabled accounts dropped and `enable` stripped";
      internal = true;
      readOnly = true;
      description = "`settings` as rendered to the `--config` file.";
    };

    sync = {
      enable = lib.mkEnableOption "the periodic sync timer" // {
        default = true;
      };
      schedule = lib.mkOption {
        type = lib.types.str;
        default = "*:0/30";
        example = "hourly";
        description = "systemd OnCalendar schedule for the sync timer (default: every 30 minutes).";
      };
    };

    cleanup = {
      enable = lib.mkEnableOption "a second timer running `cleanup` over the configured sources";
      schedule = lib.mkOption {
        type = lib.types.str;
        default = "weekly";
        example = "*-*-* 03:00:00";
        description = "systemd OnCalendar schedule for the cleanup timer.";
      };
    };

    backup = {
      enable = lib.mkEnableOption "a timer snapshotting every source and the Pinboard account to disk";
      schedule = lib.mkOption {
        type = lib.types.str;
        default = "daily";
        example = "*-*-* 04:00:00";
        description = "systemd OnCalendar schedule for the backup timer.";
      };
      directory = lib.mkOption {
        type = lib.types.str;
        default = "${stateDir}/backup";
        description = ''
          Where the snapshot is written: `raw/` (API responses, every field kept),
          `normalized/` and `manifest.json`. Each run overwrites the last snapshot in
          place and prunes nothing. Back this path up elsewhere to keep history.

          Must be under `${stateDir}`, the only path the service's `DynamicUser` can
          write. Only root can read the snapshot, since the state directory sits under
          the 0700 `/var/lib/private`.
        '';
      };
    };

    onAuthFailure = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = lib.literalExpression ''
        "''${lib.getExe pkgs.curl} -fsS -d \"pinboard-sync needs re-authentication: $PINBOARD_SYNC_AUTH_ERROR\" https://ntfy.sh/YOUR-LONG-RANDOM-TOPIC"
      '';
      description = ''
        Shell command run when a source needs re-authentication (e.g. an expired Reddit
        cookie), via `/bin/sh -c` with `PINBOARD_SYNC_AUTH_ERROR` and
        `PINBOARD_SYNC_EVENT` set. Exported as `PINBOARD_SYNC_ON_AUTH_FAILURE`, which
        overrides any `on_auth_failure` in `settings`. ntfy.sh topics are public: pick a
        long random one.

        Any hook, here or in `settings`, runs in the unit's sandbox. `PATH` holds only
        the NixOS service defaults (coreutils, findutils, grep, sed, systemd), so call
        other programs by store path. Only IPv4/IPv6 sockets are allowed, so anything
        using a local socket (`systemctl`, `logger`, D-Bus) fails.

        On a host without `/bin/sh` (the bashless profile), the hook runs `sh` from
        `PATH`, which has none by default. Add a package providing `bin/sh` (e.g.
        `pkgs.busybox`; `pkgs.dash` has only `bin/dash`) to the `path` of each enabled
        unit: `systemd.services.pinboard-sync`, `pinboard-sync-cleanup` and
        `pinboard-sync-backup`.
      '';
    };

    environmentFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      example = "/run/secrets/rendered/pinboard-sync-env";
      description = ''
        systemd `EnvironmentFile` with the credentials the sources need:
        `PINBOARD_TOKEN`, `REDDIT_USERNAME`, `REDDIT_COOKIE`, `GITHUB_TOKEN` and
        `HN_USERNAME`, or their `_FILE` forms. systemd reads it as root, so a
        root-owned sops-nix template works. With one sops-nix file per secret, the
        `*File` options need no template.
      '';
    };

    pinboardTokenFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      example = "/run/secrets/pinboard-token";
      description = ''
        File holding the Pinboard API token (`username:TOKEN`). systemd loads it as root
        with `LoadCredential`, keeping the value out of the unit environment and the Nix
        store. The binary reads it through `PINBOARD_TOKEN_FILE`, which beats a
        `*_file` in `settings`.
      '';
    };

    redditUsernameFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      example = "/run/secrets/reddit-username";
      description = "File holding the Reddit username, loaded like `pinboardTokenFile`.";
    };

    redditCookieFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      example = "/run/secrets/reddit-cookie";
      description = ''
        File holding the Reddit session cookie, bare or as `reddit_session=<value>`,
        loaded like `pinboardTokenFile`.
      '';
    };

    githubTokenFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      example = "/run/secrets/github-token";
      description = "File holding the GitHub personal access token, loaded like `pinboardTokenFile`.";
    };

    hnUsernameFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      example = "/run/secrets/hn-username";
      description = "File holding the HackerNews username, loaded like `pinboardTokenFile`.";
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        # sync and cleanup always write to Pinboard, so without a token every run fails.
        assertion =
          pinboardTokenConfigured
          && (cfg.environmentFile != null || setCredentialFiles != [ ] || settingsCredentialFiles != [ ]);
        message = "services.pinboard-sync: a Pinboard token is required: set `pinboardTokenFile`, `environmentFile` (with PINBOARD_TOKEN), or `[pinboard].token_file`. Provide source credentials the same way, or as `*_file` paths in the config's account tables.";
      }
      {
        assertion = !cfg.backup.enable || pinboardTokenConfigured;
        message = "services.pinboard-sync: backup.enable needs a Pinboard token: set `pinboardTokenFile`, `environmentFile` (with PINBOARD_TOKEN), or `[pinboard].token_file`. Without one, the timer's `backup --all` leaves the Pinboard account out of every snapshot and fails every run.";
      }
      {
        assertion = !cfg.backup.enable || lib.hasPrefix "${stateDir}/" cfg.backup.directory;
        message = "services.pinboard-sync: backup.directory must be under ${stateDir}, the only path the service's DynamicUser can write.";
      }
    ];

    systemd = {
      services = {
        # PINBOARD_SYNC_CONFIG supplies `--config`; `--all` runs every enabled account.
        pinboard-sync = lib.mkIf cfg.sync.enable (mkService {
          description = "Sync saved/favorited items to Pinboard";
          schedule = cfg.sync.schedule;
          args = [
            "sync"
            "--all"
          ];
        });

        pinboard-sync-cleanup = lib.mkIf cfg.cleanup.enable (mkService {
          description = "Normalize existing Pinboard bookmarks";
          schedule = cfg.cleanup.schedule;
          args = [
            "cleanup"
            "--all"
          ];
        });

        pinboard-sync-backup = lib.mkIf cfg.backup.enable (mkService {
          description = "Back up every source and the Pinboard account to disk";
          schedule = cfg.backup.schedule;
          # `--out` wins over any `settings.backup.directory`.
          args = [
            "backup"
            "--all"
            "--out"
            cfg.backup.directory
          ];
          extraServiceConfig.StateDirectory = "pinboard-sync";
        });
      };

      # Catch up at boot on a run missed while the host was off.
      timers = {
        pinboard-sync = lib.mkIf cfg.sync.enable {
          timerConfig.Persistent = true;
        };
        pinboard-sync-cleanup = lib.mkIf cfg.cleanup.enable {
          timerConfig.Persistent = true;
        };
        pinboard-sync-backup = lib.mkIf cfg.backup.enable {
          timerConfig.Persistent = true;
        };
      };
    };
  };
}
