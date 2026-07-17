self:
{ config, lib, pkgs, ... }:
let
  cfg = config.services.pinboard-sync;

  sources = [ "reddit" "github" "hackernews" ];

  # Per-credential `*File` options: each maps a nullable-path option to the systemd
  # credential name (loaded into the tmpfs credentials dir under a transient DynamicUser)
  # and the `<VAR>_FILE` env var the binary reads the trimmed value from.
  credentialFiles = [
    { option = "pinboardTokenFile"; credential = "pinboard-token"; variable = "PINBOARD_TOKEN_FILE"; }
    { option = "redditUsernameFile"; credential = "reddit-username"; variable = "REDDIT_USERNAME_FILE"; }
    { option = "redditCookieFile"; credential = "reddit-cookie"; variable = "REDDIT_COOKIE_FILE"; }
    { option = "githubTokenFile"; credential = "github-token"; variable = "GITHUB_TOKEN_FILE"; }
    { option = "hnUsernameFile"; credential = "hn-username"; variable = "HN_USERNAME_FILE"; }
  ];
  setCredentialFiles = lib.filter (c: cfg.${c.option} != null) credentialFiles;

  # Honor a per-account `enable` (default true) in each source's account array: drop
  # the disabled accounts and strip the `enable` key — the binary's config rejects
  # unknown fields — so the plain `sync --all` / `cleanup --all` run covers exactly the
  # enabled accounts.
  pruneAccounts = accounts: map (a: removeAttrs a [ "enable" ]) (lib.filter (a: a.enable or true) accounts);

  tomlFormat = pkgs.formats.toml { };
  # Non-secret settings rendered to the store and passed via --config. Secrets are
  # NOT placed here (it lands in the world-readable nix store) — they come from the
  # `environmentFile` (a sops-nix rendered template) or the per-credential `*File`
  # options (systemd `LoadCredential`).
  configSettings = cfg.settings // lib.mapAttrs (_: pruneAccounts) (
    lib.filterAttrs (name: _: lib.elem name sources) cfg.settings
  );
  configFile = tomlFormat.generate "pinboard-sync.toml" configSettings;

  # Credential file paths the rendered config table supplies itself: the `[pinboard]`
  # destination `token_file`, a reddit account `cookie_file`, or a github account
  # `token_file` (HackerNews is public). These satisfy the binary's `resolve_secret`
  # ladder without `environmentFile` or a `*File` option, so they count toward the
  # credentials assertion. Only the enabled/rendered accounts (post-`pruneAccounts`).
  settingsCredentialFiles =
    lib.optional ((configSettings.pinboard.token_file or null) != null) configSettings.pinboard.token_file
    ++ lib.filter (p: p != null) (
      map (a: a.cookie_file or null) (configSettings.reddit or [ ])
      ++ map (a: a.token_file or null) (configSettings.github or [ ])
    );

  # Build a oneshot service + timer running `pinboard-sync <args>` on `schedule`. The
  # generated config path and the optional hook are the only things in the unit
  # environment; credentials (incl. usernames) come from `environmentFile` (a sops-nix
  # rendered template, read by systemd as root) and/or the per-credential `*File`
  # options loaded through systemd `LoadCredential`, never the nix store. The service
  # is hardened under a transient `DynamicUser`.
  #
  # `%d` in the `<VAR>_FILE` values expands to the unit's credentials directory
  # ($CREDENTIALS_DIRECTORY), a private tmpfs the DynamicUser can read even though it
  # can't read the root-owned source path directly.
  mkService = description: schedule: args: {
    inherit description;
    after = [ "network-online.target" ];
    wants = [ "network-online.target" ];
    startAt = schedule;
    environment =
      { PINBOARD_SYNC_CONFIG = toString configFile; }
      // lib.optionalAttrs (cfg.onAuthFailure != null) {
        PINBOARD_SYNC_ON_AUTH_FAILURE = cfg.onAuthFailure;
      }
      // lib.listToAttrs (map (c: lib.nameValuePair c.variable "%d/${c.credential}") setCredentialFiles);
    serviceConfig = {
      Type = "oneshot";
      ExecStart = "${lib.getExe cfg.package} ${lib.escapeShellArgs args}";
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
    } // lib.optionalAttrs (cfg.environmentFile != null) {
      EnvironmentFile = cfg.environmentFile;
    } // lib.optionalAttrs (setCredentialFiles != [ ]) {
      LoadCredential = map (c: "${c.credential}:${toString cfg.${c.option}}") setCredentialFiles;
    };
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
      type = tomlFormat.type;
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
        Non-secret config rendered to a TOML file and passed via `--config`. Mirrors
        the config schema (`[pinboard]`, `[hooks]`, and per-source account arrays),
        minus secrets — never put tokens/cookies here, as the file lands in the
        world-readable Nix store. Provide those via `environmentFile`, the
        per-credential `*File` options below, or as sops-nix `*_file` *paths* inside
        account tables (paths are not secret).

        Each account may carry `enable = false` (default `true`) to keep it in the
        config but leave it out of the sync/cleanup runs; the flag is stripped before
        the config is rendered.
      '';
    };

    sync = {
      enable = lib.mkEnableOption "the periodic sync timer" // { default = true; };
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
        description = ''
          systemd OnCalendar schedule for the cleanup timer (default: weekly). Only
          used when `cleanup.enable = true`.
        '';
      };
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
        Systemd `EnvironmentFile` providing every credential the configured
        source(s) need — `PINBOARD_TOKEN`, `REDDIT_USERNAME`/`REDDIT_COOKIE`,
        `GITHUB_TOKEN`, `HN_USERNAME` (or their `_FILE` variants). Read by systemd as
        root, so it works with the hardened `DynamicUser` and a sops-nix rendered
        template, keeping everything out of the nix store.

        For per-secret sops-nix files, prefer the individual `*File` options below —
        each loads one secret via systemd `LoadCredential`, so you don't have to
        render a combined env template.
      '';
    };

    pinboardTokenFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      example = "/run/secrets/pinboard-token";
      description = ''
        Path to a file containing the Pinboard API token (`username:TOKEN`). Loaded
        into the unit's credentials directory via systemd `LoadCredential` and read by
        the binary through `PINBOARD_TOKEN_FILE`, so the value never enters the unit
        environment or the nix store. Works with the hardened `DynamicUser`.
      '';
    };

    redditUsernameFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      example = "/run/secrets/reddit-username";
      description = ''
        Path to a file containing the Reddit username. Loaded via systemd
        `LoadCredential` and read through `REDDIT_USERNAME_FILE`.
      '';
    };

    redditCookieFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      example = "/run/secrets/reddit-cookie";
      description = ''
        Path to a file containing the Reddit session cookie, in the full
        `reddit_session=<value>` form (matching `REDDIT_COOKIE` in `.env.example`).
        Loaded via systemd `LoadCredential` and read through `REDDIT_COOKIE_FILE`.
      '';
    };

    githubTokenFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      example = "/run/secrets/github-token";
      description = ''
        Path to a file containing the GitHub personal access token. Loaded via systemd
        `LoadCredential` and read through `GITHUB_TOKEN_FILE`.
      '';
    };

    hnUsernameFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      example = "/run/secrets/hn-username";
      description = ''
        Path to a file containing the HackerNews username. Loaded via systemd
        `LoadCredential` and read through `HN_USERNAME_FILE`.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = cfg.environmentFile != null || setCredentialFiles != [ ] || settingsCredentialFiles != [ ];
        message = "services.pinboard-sync: provide credentials via `environmentFile` (e.g. a sops-nix rendered template with PINBOARD_TOKEN), the per-credential `*File` options (e.g. `pinboardTokenFile`), or `*_file` paths inside the config account tables (e.g. `[pinboard].token_file`).";
      }
    ];

    # `--config` (PINBOARD_SYNC_CONFIG) is in the environment, so the verb is all that's
    # needed; `--all` runs every account left in the generated config (disabled
    # accounts were pruned above).
    systemd.services.pinboard-sync =
      lib.mkIf cfg.sync.enable (
        mkService "Sync saved/favorited items to Pinboard" cfg.sync.schedule [ "sync" "--all" ]
      );

    systemd.services.pinboard-sync-cleanup =
      lib.mkIf cfg.cleanup.enable (
        mkService "Normalize existing Pinboard bookmarks" cfg.cleanup.schedule [ "cleanup" "--all" ]
      );

    # Fire a missed run on next boot (the machine may be asleep/off at the scheduled
    # instant — especially for the weekly cleanup).
    systemd.timers.pinboard-sync = lib.mkIf cfg.sync.enable {
      timerConfig.Persistent = true;
    };
    systemd.timers.pinboard-sync-cleanup = lib.mkIf cfg.cleanup.enable {
      timerConfig.Persistent = true;
    };
  };
}
