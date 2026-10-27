use super::*;

#[test]
fn empty_config_is_all_defaults() {
    let cfg = Config::parse("").unwrap();
    assert!(cfg.reddit.is_empty());
    assert!(cfg.pinboard.token.is_none());
    assert!(!cfg.pinboard.public);
    assert!(cfg.hooks.on_auth_failure.is_none());
    assert!(cfg.pinboard.limit.is_none());
    assert!(cfg.defaults.reddit.toread.is_none());
}

fn assert_error_hides(text: &str, secret: &str, location: &str) {
    let err = Config::parse(text).expect_err("malformed config must not parse");
    for rendered in [format!("{err}"), format!("{err:#}"), format!("{err:?}")] {
        assert!(!rendered.contains(secret), "leaked {secret}: {rendered}");
        assert!(rendered.contains(location), "no {location} in: {rendered}");
    }
}

#[test]
fn syntax_error_on_an_inline_secret_reports_position_not_the_line() {
    assert_error_hides(
        "[hooks]\n\n[pinboard]\ntoken = \"user:SECRET123\ntoread = true\n",
        "SECRET123",
        "line 4, column",
    );
    assert_error_hides(
        "[[reddit]]\nname = \"main\"\ncookie = reddit_session=SECRET456\n",
        "SECRET456",
        "line 3, column",
    );
}

#[test]
fn type_error_on_an_inline_secret_reports_position_not_the_line() {
    assert_error_hides(
        "[pinboard]\ntoken = [\"SECRET789\"]\n",
        "SECRET789",
        "line 2, column 9",
    );
}

#[test]
fn limit_takes_auto_all_or_a_positive_count() {
    let limit = |value: &str| {
        Config::parse(&format!("[pinboard]\nlimit = {value}\n"))
            .unwrap()
            .pinboard
            .limit
    };
    assert_eq!(limit("\"auto\""), Some(SyncLimit::Auto));
    assert_eq!(limit("\"all\""), Some(SyncLimit::All));
    assert_eq!(
        limit("25"),
        Some(SyncLimit::Max(NonZeroUsize::new(25).unwrap()))
    );
    assert_eq!(Config::parse("").unwrap().pinboard.limit, None);
    assert_eq!(SyncLimit::default(), SyncLimit::Auto);
}

#[test]
fn a_bad_limit_names_the_field_not_the_value() {
    for (value, column) in [("0", 9), ("-3", 9), ("\"SECRET42\"", 9), ("1.5", 9)] {
        assert_error_hides(
            &format!("[[hackernews]]\nlimit = {value}\n"),
            "SECRET42",
            &format!("line 2, column {column}: `limit` must be `auto` (no cap;"),
        );
    }
    assert_error_hides(
        "[[hackernews]]\nlimit = [\"SECRET43\"]\n",
        "SECRET43",
        "line 2, column 9: invalid type: sequence, expected `limit` as `auto`",
    );
    assert_error_hides(
        "[[hackernews]]\nlimit = true\n",
        "`true`",
        "line 2, column 9: invalid type: boolean, expected `limit` as `auto`",
    );
}

#[test]
fn secret_under_a_non_string_key_is_not_quoted_back() {
    assert_error_hides(
        "[pinboard]\nrate_limit_secs = \"user:SECRET321\"\n",
        "SECRET321",
        "line 2, column 19: invalid type: string, expected u64",
    );
    assert_error_hides(
        "[[reddit]]\ntoread = \"reddit_session=SECRET654, expected x\"\n",
        "SECRET654",
        "line 2, column 10: invalid type: string, expected a boolean",
    );
    assert_error_hides(
        "[[github]]\ntag_lang_prefix = 5\n",
        "`5`",
        "invalid type: integer, expected a string",
    );
}

#[test]
fn validation_error_names_the_field_not_the_value() {
    assert_error_hides(
        "[[reddit]]\nusername = \"a\"\n[[reddit]]\ntags = [\"reddit\", \"reddit_session=SECRET1 x\"]\n",
        "SECRET1",
        "`reddit[1].tags[1]` must not contain whitespace",
    );
    assert_error_hides(
        "[[github]]\ntag_lang_prefix = \"ghp SECRET2\"\n",
        "SECRET2",
        "`github[0].tag_lang_prefix` must not contain whitespace",
    );
    assert_error_hides(
        "[[hackernews]]\ntag_link = \"x SECRET3\"\n",
        "SECRET3",
        "`hackernews[0].tag_link` must not contain whitespace",
    );
    assert_error_hides(
        "[[reddit]]\nreddit_domain = \"user:SECRET4/x\"\n",
        "SECRET4",
        "`reddit[0].reddit_domain` contains a `/`",
    );
    assert_error_hides(
        "[[reddit]]\nreddit_domain = \"reddit_session=SECRET5 x\"\n",
        "SECRET5",
        "`reddit[0].reddit_domain` contains whitespace",
    );
    assert_error_hides(
        "[[reddit]]\nusername = \"a\"\n[[reddit]]\nreddit_domain = \"SECRET7.example:8080\"\n",
        "SECRET7",
        "`reddit[1].reddit_domain` contains a `:` (a port is not allowed)",
    );
    for not_a_host in [
        "user@SECRET8.example",
        "SECRET8%2Eexample",
        "[::SECRET8]:8080",
    ] {
        assert_error_hides(
            &format!("[[reddit]]\nreddit_domain = \"{not_a_host}\"\n"),
            "SECRET8",
            "`reddit[0].reddit_domain` is not a valid host",
        );
    }
    assert_error_hides(
        "[[reddit]]\nreddit_domain = \"SECRET9.example.\"\n",
        "SECRET9",
        "`reddit[0].reddit_domain` ends with a `.`",
    );
    assert_error_hides(
        "[[reddit]]\nreddit_domain = \"SECRET10.b\u{fc}cher.example\"\n",
        "SECRET10",
        "`reddit[0].reddit_domain` is not ASCII",
    );
    assert_error_hides(
        "[[reddit]]\nreddit_domain = \"\"\n",
        "SECRET",
        "`reddit[0].reddit_domain` is empty",
    );
    assert_error_hides(
        "[[github]]\nname = \"SECRET6\"\n[[github]]\n[[github]]\nname = \"SECRET6\"\n",
        "SECRET6",
        "github[2] has the same account name as github[0]",
    );
    assert_error_hides(
        "[[github]]\ntoken = \"ghp_SECRET7\"\n[[github]]\nname = \"work\"\n\
         [[github]]\nname = \"\"\ntoken = \"ghp_SECRET8\"\n",
        "SECRET",
        "github[0] and github[2] would both back up as `github-default`",
    );
}

#[test]
fn a_github_name_that_slugs_to_default_counts_as_unnamed() {
    for second_name in ["default", "Default", "!!", " "] {
        let text = format!("[[github]]\n[[github]]\nname = \"{second_name}\"\n");
        let error = format!("{:#}", Config::parse(&text).unwrap_err());
        assert!(
            error.contains("github[0] and github[1] would both back up"),
            "{second_name:?}: {error}"
        );
    }
}

#[test]
fn one_unnamed_github_account_is_fine() {
    let config = Config::parse("[[github]]\n[[github]]\nname = \"work\"\n").unwrap();
    assert_eq!(config.github.len(), 2);
}

#[test]
fn two_reddit_or_hn_accounts_with_neither_name_nor_username_are_rejected() {
    assert_error_hides(
        "[[reddit]]\ncookie = \"reddit_session=SECRET1\"\n[[reddit]]\nusername = \"alice\"\n\
         [[reddit]]\ncookie_file = \"/run/SECRET2\"\n",
        "SECRET",
        "reddit[0] and reddit[2] would both back up as `reddit-default`; give one of them \
         a distinct `name` (or, with no `name`, `username`)",
    );
    assert_error_hides(
        "[[hackernews]]\n[[hackernews]]\ntags = [\"SECRET3\"]\n",
        "SECRET",
        "hackernews[0] and hackernews[1] would both back up as `hackernews-default`",
    );
}

/// The username a selector and a backup stem fall back on has to be in the config: one
/// from `--reddit-username` or `$REDDIT_USERNAME(_FILE)` outranks every account's own, so
/// it is the same for both and tells neither apart.
#[test]
fn reddit_and_hn_accounts_told_apart_by_username_alone_are_fine() {
    for source in ["reddit", "hackernews"] {
        for text in [
            format!("[[{source}]]\nusername = \"alice\"\n[[{source}]]\nusername = \"bob\"\n"),
            format!("[[{source}]]\n[[{source}]]\nusername = \"bob\"\n"),
            format!("[[{source}]]\n[[{source}]]\nname = \"work\"\n"),
        ] {
            Config::parse(&text).unwrap_or_else(|e| panic!("{text:?}: {e:#}"));
        }
    }
}

#[test]
fn account_names_that_slug_alike_are_rejected_without_quoting_them() {
    for (source, field) in [
        ("github", "name"),
        ("reddit", "username"),
        ("hackernews", "name"),
    ] {
        for (first, second) in [
            ("secret.a", "secret/a"),
            ("Secret", "secret"),
            ("secret-x", "secret x!"),
        ] {
            assert_error_hides(
                &format!(
                    "[[{source}]]\n{field} = \"{first}\"\n[[{source}]]\nname = \"other\"\n\
                     [[{source}]]\n{field} = \"{second}\"\n"
                ),
                "ecret",
                &format!(
                    "{source}[0] and {source}[2] would back up to the same files, since \
                     their account names make the same file name"
                ),
            );
        }
    }
    assert!(Config::parse("[[reddit]]\nusername = \"a.b\"\n[[reddit]]\nname = \"a.c\"\n").is_ok());
}

#[test]
fn a_reddit_name_that_slugs_to_default_hides_its_username() {
    let error =
        Config::parse("[[reddit]]\n[[reddit]]\nname = \"\"\nusername = \"bob\"\n").unwrap_err();
    assert!(
        format!("{error:#}").contains("reddit[0] and reddit[1] would both back up"),
        "{error:#}"
    );
}

#[test]
fn debug_of_a_parsed_config_redacts_inline_secrets() {
    let cfg = Config::parse(
        "[pinboard]\ntoken = \"user:PINSECRET\"\n\
         [[reddit]]\ncookie = \"reddit_session=REDDITSECRET\"\ncookie_file = \"/run/cookie\"\n\
         [[reddit]]\nusername = \"alt\"\n\
         [[github]]\ntoken = \"ghp_GITHUBSECRET\"\n",
    )
    .unwrap();
    for rendered in [format!("{cfg:?}"), format!("{cfg:#?}")] {
        for secret in ["PINSECRET", "REDDITSECRET", "GITHUBSECRET"] {
            assert!(!rendered.contains(secret), "leaked {secret}: {rendered}");
        }
        assert!(rendered.contains("/run/cookie"), "{rendered}");
    }
    assert_eq!(
        format!("{:?}", Redacted(&cfg.pinboard.token)),
        "Some(<redacted>)"
    );
    assert_eq!(format!("{:?}", Redacted(&cfg.reddit[1].cookie)), "None");
    assert!(format!("{:?}", cfg.reddit[0]).contains("cookie: Some(<redacted>)"));
    assert!(format!("{:?}", cfg.reddit[1]).contains("cookie: None"));
    assert!(format!("{:?}", cfg.github[0]).contains("token: Some(<redacted>)"));
}

#[test]
fn spanless_toml_error_still_drops_the_value() {
    let error = <toml::de::Error as serde::de::Error>::custom(
        "invalid type: string \"SECRET987\", expected usize",
    );
    assert_eq!(
        describe_toml_error(&error, ""),
        "invalid type: string, expected usize"
    );
}

#[test]
fn toml_error_column_counts_characters() {
    assert_eq!(line_and_column("a = \"é\"\nb", 9), (2, 1));
    assert_eq!(line_and_column("ab", 1), (1, 2));
    assert_eq!(line_and_column("é", 1), (1, 1));
    assert_eq!(line_and_column("ab", 99), (1, 3));
}

#[test]
fn parses_per_source_defaults_and_account_overrides() {
    let cfg = Config::parse(
        r#"
            [pinboard]
            toread = true

            [defaults.reddit]
            toread = false
            limit = 7

            [[reddit]]
            name = "main"
            toread = true

            [[reddit]]
            name = "alt"
            "#,
    )
    .unwrap();

    assert!(cfg.pinboard.toread);
    assert_eq!(cfg.defaults.reddit.toread, Some(false));
    assert_eq!(
        cfg.defaults.reddit.limit,
        Some(SyncLimit::Max(NonZeroUsize::new(7).unwrap()))
    );
    assert_eq!(cfg.reddit[0].toread, Some(true));
    assert_eq!(cfg.reddit[1].toread, None);
    // An unknown key inside the per-source table is still rejected.
    assert!(Config::parse("[defaults.reddit]\nnonsense = 1").is_err());
}

#[test]
fn parses_hooks_pinboard_and_multiple_reddit_accounts() {
    let cfg = Config::parse(
        r#"
            [hooks]
            on_auth_failure = "notify"

            [pinboard]
            token = "user:TOK"
            public = true

            [[reddit]]
            name = "main"
            username = "alice"
            cookie_file = "/run/secrets/cookie"
            reddit_domain = "www.reddit.com"
            tag_media_types = ["image"]
            tags = ["reddit", "account:main"]

            [[reddit]]
            name = "alt"
            username = "bob"
            "#,
    )
    .unwrap();

    assert_eq!(cfg.hooks.on_auth_failure.as_deref(), Some("notify"));
    assert_eq!(cfg.pinboard.token.as_deref(), Some("user:TOK"));
    assert!(cfg.pinboard.public);
    assert_eq!(cfg.reddit.len(), 2);

    let main = &cfg.reddit[0];
    assert_eq!(main.username.as_deref(), Some("alice"));
    let rc = RedditConfig::from(main);
    assert_eq!(rc.domain, "www.reddit.com");
    assert_eq!(rc.tags, vec!["reddit", "account:main"]);
    assert_eq!(rc.media_types, vec!["image"]);
    // Unset tag fields keep their defaults; an unset `tags` defaults to ["reddit"].
    assert_eq!(rc.subreddit_prefix, "subreddit:");
    assert_eq!(RedditConfig::from(&cfg.reddit[1]).tags, vec!["reddit"]);
}

#[test]
fn rejects_unknown_keys() {
    assert!(Config::parse("[[reddit]]\nnonsense = 1").is_err());
}

#[test]
fn rejects_whitespace_in_tags() {
    assert!(Config::parse("[[reddit]]\ntags = [\"a b\"]").is_err());
    assert!(Config::parse("[[github]]\ntag_lang_prefix = \"lang :\"").is_err());
    assert!(Config::parse("[[hackernews]]\ntag_link = \"find hn\"").is_err());
    // No whitespace → fine (hyphens/colons allowed).
    assert!(Config::parse("[[reddit]]\ntags = [\"reddit\", \"a-b\"]").is_ok());
}

#[test]
fn rejects_commas_in_tags_without_quoting_them() {
    assert_error_hides(
        "[[reddit]]\ntags = [\"reddit\", \"SECRET7,x\"]\n",
        "SECRET7",
        "`reddit[0].tags[1]` must not contain a comma",
    );
    assert_error_hides(
        "[[reddit]]\ntag_flair_prefix = \"SECRET8,\"\n",
        "SECRET8",
        "`reddit[0].tag_flair_prefix` must not contain a comma",
    );
    assert_error_hides(
        "[[github]]\ntags = [\"SECRET9,x\"]\n",
        "SECRET9",
        "`github[0].tags[0]` must not contain a comma",
    );
    assert_error_hides(
        "[[hackernews]]\ntag_link = \"find,SECRET10\"\n",
        "SECRET10",
        "`hackernews[0].tag_link` must not contain a comma",
    );
}

#[test]
fn rejects_duplicate_account_names() {
    let dup = r#"
            [[reddit]]
            name = "main"
            [[reddit]]
            name = "main"
        "#;
    assert!(Config::parse(dup).is_err());
    // Distinct names (and unnamed accounts) are fine.
    let ok = r#"
            [[reddit]]
            name = "main"
            [[reddit]]
            name = "alt"
            [[reddit]]
        "#;
    assert!(Config::parse(ok).is_ok());
}

#[test]
fn rejects_a_zero_cap_with_stale_to_now_as_each_account_resolves_it() {
    let rejected = |text: &str| Config::parse(text).unwrap_err().to_string();

    // Both in one tier, and split across the account, per-source and global tiers.
    assert_eq!(
        rejected("[pinboard]\npost_date_max_age_days = 0\ncleanup_stale_to_now = true"),
        "config: reddit (implicit CLI/env account) resolves `post_date_max_age_days` to 0 \
         with `cleanup_stale_to_now` on, so cleanup would re-date its stale bookmarks on \
         every run; raise the cap or turn `cleanup_stale_to_now` off"
    );
    assert!(rejected(
        "[pinboard]\ncleanup_stale_to_now = true\n\
         [defaults.github]\npost_date_max_age_days = 0\n[[github]]"
    )
    .starts_with("config: github[0] resolves "));
    assert!(rejected(
        "[defaults.hackernews]\ncleanup_stale_to_now = true\n\
         [[hackernews]]\n[[hackernews]]\nusername = \"u\"\npost_date_max_age_days = 0"
    )
    .starts_with("config: hackernews[1] resolves "));

    // An account overriding either half resolves to a combination that settles.
    assert!(Config::parse(
        "[pinboard]\npost_date_max_age_days = 0\ncleanup_stale_to_now = true\n\
         [[reddit]]\npost_date_max_age_days = 1\n\
         [[github]]\ncleanup_stale_to_now = false\n\
         [[hackernews]]\npost_date_max_age_days = 7"
    )
    .is_ok());
    assert!(Config::parse("[pinboard]\npost_date_max_age_days = 0").is_ok());

    // A source with no accounts still has its implicit CLI/env account to check.
    assert!(rejected(
        "[pinboard]\npost_date_max_age_days = 0\ncleanup_stale_to_now = true\n\
         [[reddit]]\npost_date_max_age_days = 1"
    )
    .starts_with("config: github (implicit CLI/env account) resolves "));
}

#[test]
fn rejects_malformed_reddit_domain() {
    assert!(Config::parse("[[reddit]]\nreddit_domain = \"https://old.reddit.com\"").is_err());
    assert!(Config::parse("[[reddit]]\nreddit_domain = \"old.reddit.com/\"").is_err());
    assert!(Config::parse("[[reddit]]\nreddit_domain = \"\"").is_err());
    assert!(Config::parse("[[reddit]]\nreddit_domain = \"www.reddit.com\"").is_ok());
}

#[test]
fn accepts_any_host_cleanup_can_set_and_reddit_key_matches() {
    for domain in [
        "Old.Reddit.com",
        "Reddit.Mirror.Example",
        "xn--bcher-kva.example",
        "127.0.0.1",
        "[::1]",
    ] {
        let config = Config::parse(&format!("[[reddit]]\nreddit_domain = \"{domain}\""))
            .unwrap_or_else(|e| panic!("{domain}: {e:#}"));
        let moved = crate::reddit::cleanup::normalize_url(
            &crate::test_support::url("https://www.reddit.com/r/rust/comments/abc/x/"),
            &config.reddit_domains()[0],
        )
        .unwrap_or_else(|| panic!("{domain}: cleanup could not set the host"));
        assert_eq!(
            crate::reddit::model::reddit_key(&moved, domain).as_deref(),
            Some("/r/rust/comments/abc/x"),
            "{domain}"
        );
    }
}

#[test]
fn shipped_example_config_parses() {
    // The `config example` template must stay a valid Config.
    let cfg = Config::parse(include_str!("../config.example.toml")).unwrap();
    assert_eq!(cfg.reddit.len(), 1);
    assert_eq!(cfg.github.len(), 1);
    assert_eq!(cfg.hackernews.len(), 1);
}

/// True if `key` appears as a TOML key in the template — either active
/// (`key = …`) or commented out (`# key = …`).
fn documents_key(example: &str, key: &str) -> bool {
    example.lines().any(|line| {
        let line = line.trim_start();
        let line = line.strip_prefix('#').map(str::trim_start).unwrap_or(line);
        line.strip_prefix(key)
            .map(str::trim_start)
            .is_some_and(|rest| rest.starts_with('='))
    })
}

#[test]
fn example_config_documents_every_field() {
    // The macro destructures each struct with no `..`, so adding a field is a
    // *compile* error until it's listed here — and `stringify!` then derives the
    // key list from that same list, so the new key is checked against the
    // template below. Net effect: a new config field fails the build until it is
    // documented in config.example.toml (commented-out lines count).
    macro_rules! documented_fields {
        ($ty:path { $($field:ident),* $(,)? }) => {{
            let $ty { $($field: _),* } = <$ty>::default();
            [$(stringify!($field)),*]
        }};
    }

    let example = include_str!("../config.example.toml");
    let hooks = documented_fields!(Hooks { on_auth_failure });
    let pinboard = documented_fields!(PinboardConfig {
        token,
        token_file,
        public,
        toread,
        rate_limit_secs,
        limit,
        use_post_date,
        post_date_max_age_days,
        cleanup_stale_to_now,
    });
    // Note this list does not destructure `Config` itself, so a *new top-level table*
    // (like `[backup]`) has to be added here deliberately — only its fields are
    // compile-checked, not its presence.
    let backup = documented_fields!(BackupConfig { directory });
    // The per-source defaults tier (`[defaults.<source>]`).
    let source_defaults = documented_fields!(SourceDefaults {
        toread,
        public,
        limit,
        on_auth_failure,
        use_post_date,
        post_date_max_age_days,
        cleanup_stale_to_now,
    });
    let reddit = documented_fields!(RedditAccount {
        name,
        username,
        cookie,
        cookie_file,
        on_auth_failure,
        limit,
        toread,
        public,
        use_post_date,
        post_date_max_age_days,
        cleanup_stale_to_now,
        reddit_domain,
        tag_subreddit_prefix,
        tag_comment,
        tag_nsfw,
        tag_author_prefix,
        tag_flair_prefix,
        tag_media_prefix,
        tag_media_types,
        tags,
    });
    let github = documented_fields!(GitHubAccount {
        name,
        token,
        token_file,
        on_auth_failure,
        limit,
        toread,
        public,
        use_post_date,
        post_date_max_age_days,
        cleanup_stale_to_now,
        tag_lang_prefix,
        tags,
    });
    let hackernews = documented_fields!(HackerNewsAccount {
        name,
        username,
        limit,
        toread,
        public,
        use_post_date,
        post_date_max_age_days,
        cleanup_stale_to_now,
        tag_comment,
        tag_author_prefix,
        tag_special_prefix,
        tag_link,
        tags,
    });

    for &key in hooks
        .iter()
        .chain(&pinboard)
        .chain(&backup)
        .chain(&source_defaults)
        .chain(&reddit)
        .chain(&github)
        .chain(&hackernews)
    {
        assert!(
            documents_key(example, key),
            "config.example.toml does not document the `{key}` config field"
        );
    }
}

#[test]
fn select_account_by_name_or_first_or_error() {
    let accounts = vec![
        RedditAccount {
            name: Some("a".into()),
            ..Default::default()
        },
        RedditAccount {
            name: Some("b".into()),
            ..Default::default()
        },
    ];
    assert_eq!(
        select_account(&accounts, None)
            .unwrap()
            .unwrap()
            .name
            .as_deref(),
        Some("a")
    );
    assert_eq!(
        select_account(&accounts, Some("b"))
            .unwrap()
            .unwrap()
            .name
            .as_deref(),
        Some("b")
    );
    assert!(select_account(&accounts, Some("missing")).is_err());
    // No accounts + no name → None (caller uses flags/env).
    let empty: Vec<RedditAccount> = vec![];
    assert!(select_account(&empty, None).unwrap().is_none());
}

#[test]
fn account_name_falls_back_to_username() {
    // Reddit/HN: an unnamed account is selectable by its username; an explicit
    // name still wins.
    let by_username = RedditAccount {
        username: Some("alice".into()),
        ..Default::default()
    };
    assert_eq!(by_username.account_name(), Some("alice"));
    let named = RedditAccount {
        name: Some("main".into()),
        username: Some("alice".into()),
        ..Default::default()
    };
    assert_eq!(named.account_name(), Some("main"));
    // GitHub has no username, so only an explicit name selects it.
    assert_eq!(
        GitHubAccount {
            token: Some("ghp_x".into()),
            ..Default::default()
        }
        .account_name(),
        None
    );
}

#[test]
fn resolve_setting_picks_the_highest_present_tier() {
    // flag → account → source → global, first `Some` wins.
    assert_eq!(resolve_setting(Some(1), Some(2), Some(3), 5), 1); // flag tops all
    assert_eq!(resolve_setting(None, Some(2), Some(3), 5), 2); // account over source
    assert_eq!(resolve_setting(None, None, Some(3), 5), 3); // source over global
    assert_eq!(resolve_setting::<usize>(None, None, None, 5), 5); // global fallback

    // A `Some(false)` flag is a real value, so it forces a `true` lower tier off —
    // the behavior a bare clap flag could not express (this is why `--public[=BOOL]`).
    assert!(!resolve_setting(Some(false), Some(true), Some(true), true));
    assert!(resolve_setting(Some(true), None, None, false));
    assert!(resolve_setting(None, None, None, true)); // global fallback
}
