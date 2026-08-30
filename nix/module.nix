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
  #
  # Unlike the `*File` options (systemd LoadCredential) and `environmentFile`, which
  # systemd reads as root before dropping privileges, these paths are opened by the
  # binary itself as the transient DynamicUser, so the secret they point at must be
  # readable by that user (e.g. sops mode 0444) — a root-owned 0400 secret passes the
  # assertion but fails to read at runtime. See the `settings` option description.
  settingsCredentialFiles =
    lib.optional ((configSettings.pinboard.token_file or null) != null) configSettings.pinboard.token_file
    ++ lib.filter (p: p != null) (
      map (a: a.cookie_file or null) (configSettings.reddit or [ ])
      ++ map (a: a.token_file or null) (configSettings.github or [ ])
    );

  # `backup` needs the Pinboard token specifically (not just any source credential), so
  # it's asserted separately from the general credentials check below.
  pinboardTokenConfigured =
    cfg.environmentFile != null
    || cfg.pinboardTokenFile != null
    || (configSettings.pinboard.token_file or null) != null;

  # The service runs as a transient `DynamicUser`, which can only write to its own
  # `StateDirectory`. `backup.path` must live under this dir; the snapshot is then
  # retrievable by root (the state dir sits under the 0700 `/var/lib/private`).
  stateDir = "/var/lib/pinboard-sync";

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
  #
  # `extraServiceConfig` is merged over the hardening last, so a unit that needs more
  # (the backup timer's `StateDirectory`) adds it without reaching around the factory.
  mkService = { description, schedule, args, extraServiceConfig ? { } }: {
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
    } // extraServiceConfig;
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

        Caveat for the in-table `*_file` form (`[pinboard].token_file`, a reddit
        account `cookie_file`, a github account `token_file`): the binary opens that
        path itself, running as the service's transient `DynamicUser`, so the secret
        it points at must be *readable by that user* (e.g. a sops-nix secret with
        `mode = "0444"`, or an owning group the service belongs to). A default
        `0400`/root-owned secret is unreadable this way: the config still builds and
        the assertions still pass, but every timer fire fails to read it and surfaces
        as a missing-credential error. This differs from `environmentFile` and the
        dedicated `*File` options below, which systemd reads *as root* (via
        `EnvironmentFile`/`LoadCredential`) before dropping privileges — for a
        root-owned `0400` sops secret, prefer those.

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

    backup = {
      enable = lib.mkEnableOption "a timer backing up all Pinboard bookmarks to a file";
      schedule = lib.mkOption {
        type = lib.types.str;
        default = "daily";
        example = "*-*-* 04:00:00";
        description = ''
          systemd OnCalendar schedule for the backup timer (default: daily). Only used
          when `backup.enable = true`.
        '';
      };
      path = lib.mkOption {
        type = lib.types.str;
        default = "${stateDir}/pinboard-backup.json";
        description = ''
          File the backup is written to (replaced atomically each run), as raw Pinboard
          `posts/all` JSON. Must live under `${stateDir}` — the service runs as a
          transient `DynamicUser` and can only write to its `StateDirectory`. The
          snapshot is readable by root (retrieve it with a root-run job); the state dir
          sits under the 0700 `/var/lib/private`.
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
        # sync/cleanup always write to the Pinboard destination, so a resolvable Pinboard
        # token is required regardless of which source credentials are set — otherwise the
        # service enables and then fails on every timer fire with a missing-token error.
        assertion =
          pinboardTokenConfigured
          && (cfg.environmentFile != null || setCredentialFiles != [ ] || settingsCredentialFiles != [ ]);
        message = "services.pinboard-sync: a Pinboard token is required — set `pinboardTokenFile`, `environmentFile` (with PINBOARD_TOKEN), or `[pinboard].token_file`. Provide source credentials the same way (or as `*_file` paths in the config account tables).";
      }
      {
        assertion = !cfg.backup.enable || pinboardTokenConfigured;
        message = "services.pinboard-sync: backup.enable needs a Pinboard token — set `pinboardTokenFile`, `environmentFile` (with PINBOARD_TOKEN), or `[pinboard].token_file`. Source-only credentials (e.g. a reddit cookie) don't satisfy `backup`.";
      }
      {
        assertion = !cfg.backup.enable || lib.hasPrefix "${stateDir}/" cfg.backup.path;
        message = "services.pinboard-sync: backup.path must be under ${stateDir} — the service runs as a transient DynamicUser and can only write to its StateDirectory.";
      }
    ];

    # `--config` (PINBOARD_SYNC_CONFIG) is in the environment, so the verb is all that's
    # needed; `--all` runs every account left in the generated config (disabled
    # accounts were pruned above).
    systemd.services.pinboard-sync =
      lib.mkIf cfg.sync.enable (mkService {
        description = "Sync saved/favorited items to Pinboard";
        schedule = cfg.sync.schedule;
        args = [ "sync" "--all" ];
      });

    systemd.services.pinboard-sync-cleanup =
      lib.mkIf cfg.cleanup.enable (mkService {
        description = "Normalize existing Pinboard bookmarks";
        schedule = cfg.cleanup.schedule;
        args = [ "cleanup" "--all" ];
      });

    # `StateDirectory` gives the hardened service its one writable location
    # (`/var/lib/pinboard-sync`), which `backup.path` is asserted to live under.
    systemd.services.pinboard-sync-backup =
      lib.mkIf cfg.backup.enable (mkService {
        description = "Back up all Pinboard bookmarks";
        schedule = cfg.backup.schedule;
        args = [ "backup" cfg.backup.path ];
        extraServiceConfig.StateDirectory = "pinboard-sync";
      });

    # Fire a missed run on next boot (the machine may be asleep/off at the scheduled
    # instant — especially for the weekly cleanup).
    systemd.timers.pinboard-sync = lib.mkIf cfg.sync.enable {
      timerConfig.Persistent = true;
    };
    systemd.timers.pinboard-sync-cleanup = lib.mkIf cfg.cleanup.enable {
      timerConfig.Persistent = true;
    };
    systemd.timers.pinboard-sync-backup = lib.mkIf cfg.backup.enable {
      timerConfig.Persistent = true;
    };
  };
}
