//! `sync` jobs: one per account, each a configured source client plus its auth-failure
//! hook and settings resolved through the flag → account → `[defaults.<source>]` →
//! `[pinboard]` ladder. [`SourceAccount`] is what lets `sync`, `backup` and `doctor` treat
//! the three sources through one generic path.

use anyhow::{bail, Context, Result};
use url::Url;

use crate::backup::{self, BackupSource};
use crate::bookmark::Bookmark;
use crate::cli::SyncFlags;
use crate::config::{self, Config, GitHubAccount, HackerNewsAccount, RedditAccount, SyncLimit};
use crate::github::{GitHubClient, GitHubConfig};
use crate::hackernews::{HackerNewsClient, HackerNewsConfig};
use crate::hook::resolve_hook;
use crate::model::RedditConfig;
use crate::reddit::RedditClient;
use crate::secrets::{
    missing_github_token, resolve_github_token, resolve_reddit_cookie, resolve_secret,
};
use crate::source::{BookmarkDraft, Source, SourceError, UrlKey};
use crate::sync::JobSettings;
use crate::timefmt::DateOpts;
use crate::AllRun;

/// Build one job per configured account across every source for `sync --all`,
/// isolating build failures: an account whose required secret won't resolve is
/// reported and counted rather than aborting the whole run, so the healthy
/// accounts still sync. Returns the buildable jobs and the count of accounts
/// that failed to build, which the caller threads into the run's exit code.
pub fn build_all_jobs(config: &Config, ovr: &SyncOverrides) -> (Vec<SyncJob>, usize) {
    let mut jobs = Vec::new();
    let mut run = AllRun::default();
    build_every_job::<RedditAccount>(config, ovr, &mut jobs, &mut run);
    build_every_job::<GitHubAccount>(config, ovr, &mut jobs, &mut run);
    build_every_job::<HackerNewsAccount>(config, ovr, &mut jobs, &mut run);
    (jobs, run.failed)
}

/// [`build_all_jobs`] for one source: a job for each of its configured accounts, with each
/// account that fails to build recorded in `run` instead.
fn build_every_job<A: SourceAccount>(
    config: &Config,
    ovr: &SyncOverrides,
    jobs: &mut Vec<SyncJob>,
    run: &mut AllRun,
) {
    for account in A::configured(config) {
        match build_job(Some(account), ovr, config) {
            Ok(job) => jobs.push(job),
            Err(e) => run.record(Err(e)),
        }
    }
}

/// Build one job per account: every account when `all`, else the named (or first,
/// or implicit CLI/env) account.
pub fn build_jobs<A: SourceAccount>(
    name: Option<&str>,
    all: bool,
    ovr: &SyncOverrides,
    config: &Config,
) -> Result<Vec<SyncJob>> {
    let accounts = A::configured(config);
    if all {
        if accounts.is_empty() {
            bail!("--all requires a --config with at least one configured account");
        }
        accounts
            .iter()
            .map(|a| build_job(Some(a), ovr, config))
            .collect()
    } else {
        let account = config::select_account(accounts, name)?;
        Ok(vec![build_job(account, ovr, config)?])
    }
}

#[derive(Default)]
pub struct SyncOverrides {
    pub reddit_username: Option<String>,
    pub reddit_cookie: Option<String>,
    pub github_token: Option<String>,
    pub hackernews_username: Option<String>,
    pub on_auth_failure: Option<String>,
    pub flags: SyncFlags,
    pub dry_run: bool,
}

impl SyncOverrides {
    /// Let a `--on-auth-failure` placed before the source subcommand (on
    /// `SyncCmd`) take effect, falling back to the per-source value. Mirrors the
    /// `--dry-run` merge in `run_sync`, which honors either placement too.
    pub fn with_top_level_hook(mut self, on_auth_failure: Option<String>) -> Self {
        self.on_auth_failure = on_auth_failure.or(self.on_auth_failure);
        self
    }
}

/// A configured source client ready to fetch, plus its auth-failure hook and the
/// account's resolved write settings.
pub struct SyncJob {
    pub client: SourceClient,
    /// Human label for logs, e.g. `reddit[alice]`.
    pub label: String,
    pub hook: Option<String>,
    pub settings: JobSettings,
}

impl SyncJob {
    /// The source's drafts: read incrementally against `existing` under
    /// [`SyncLimit::Auto`], else in full.
    pub async fn fetch(&self, existing: &[Bookmark]) -> Result<Vec<BookmarkDraft>, SourceError> {
        match self.settings.limit {
            SyncLimit::Auto => self.client.fetch_incremental(existing).await,
            SyncLimit::All | SyncLimit::Max(_) => self.client.fetch().await,
        }
    }
}

/// A log/display label for a job: `source[account]`, or `source[default]` when no
/// named account (an implicit CLI/env invocation).
fn job_label<A: SourceAccount>(account: Option<&A>) -> String {
    format!(
        "{}[{}]",
        A::SOURCE,
        account.and_then(|a| a.account_name()).unwrap_or("default")
    )
}

/// Resolve a setting through its precedence tiers, highest first: the CLI flag, else
/// the account override, else the per-source default, else the resolved `[pinboard]`
/// global. The first present (`Some`) value wins; the global is the guaranteed fallback.
fn resolve_setting<T>(flag: Option<T>, account: Option<T>, source: Option<T>, global: T) -> T {
    flag.or(account).or(source).unwrap_or(global)
}

/// CLI-flag overrides for the date settings, sitting at the top of the tier
/// (flag → account → per-source default → `[pinboard]` global). All-`None` means
/// "no flag given", so resolution falls through to the existing tiers unchanged.
#[derive(Default, Clone)]
pub struct DateOverrides {
    pub use_post_date: Option<bool>,
    pub max_age_days: Option<u64>,
    pub stale_to_now: Option<bool>,
}

impl DateOverrides {
    /// Let a date flag placed before the source subcommand (on `CleanupCmd`) take
    /// effect, falling back to this per-source value. Mirrors sync's
    /// `SyncOverrides::with_top_level_hook`.
    pub fn with_top_level(self, top: &DateOverrides) -> DateOverrides {
        DateOverrides {
            use_post_date: top.use_post_date.or(self.use_post_date),
            max_age_days: top.max_age_days.or(self.max_age_days),
            stale_to_now: top.stale_to_now.or(self.stale_to_now),
        }
    }
}

/// The `use_post_date` trio for an account, each resolved through [`resolve_setting`]
/// (flag → account → per-source default → `[pinboard]` global, with `max_age_days`
/// defaulting to [`config::DEFAULT_MAX_AGE_DAYS`]). Shared by every `sync`/`cleanup`
/// builder.
pub fn resolve_dates(
    over: &DateOverrides,
    account: Option<&impl config::Account>,
    src: &config::SourceDefaults,
    config: &Config,
) -> DateOpts {
    DateOpts {
        use_post_date: resolve_setting(
            over.use_post_date,
            account.and_then(|a| a.use_post_date()),
            src.use_post_date,
            config.pinboard.use_post_date,
        ),
        max_age_days: resolve_setting(
            over.max_age_days,
            account.and_then(|a| a.max_age_days()),
            src.post_date_max_age_days,
            config
                .pinboard
                .post_date_max_age_days
                .unwrap_or(config::DEFAULT_MAX_AGE_DAYS),
        ),
        stale_to_now: resolve_setting(
            over.stale_to_now,
            account.and_then(|a| a.stale_to_now()),
            src.cleanup_stale_to_now,
            config.pinboard.cleanup_stale_to_now,
        ),
    }
}

/// One of the concrete source clients, unified behind the `Source` port so `--all`
/// can fetch them concurrently and the dispatch can treat them uniformly.
pub enum SourceClient {
    Reddit(RedditClient),
    GitHub(GitHubClient),
    HackerNews(HackerNewsClient),
}

impl Source for SourceClient {
    async fn fetch(&self) -> Result<Vec<BookmarkDraft>, SourceError> {
        match self {
            SourceClient::Reddit(c) => c.fetch().await,
            SourceClient::GitHub(c) => c.fetch().await,
            SourceClient::HackerNews(c) => c.fetch().await,
        }
    }

    async fn fetch_incremental(
        &self,
        existing: &[Bookmark],
    ) -> Result<Vec<BookmarkDraft>, SourceError> {
        match self {
            SourceClient::Reddit(c) => c.fetch_incremental(existing).await,
            SourceClient::GitHub(c) => c.fetch_incremental(existing).await,
            SourceClient::HackerNews(c) => c.fetch_incremental(existing).await,
        }
    }
}

impl BackupSource for SourceClient {
    async fn dump(&self) -> Result<backup::BackupDump, SourceError> {
        match self {
            SourceClient::Reddit(c) => c.dump().await,
            SourceClient::GitHub(c) => c.dump().await,
            SourceClient::HackerNews(c) => c.dump().await,
        }
    }
}

impl UrlKey for SourceClient {
    fn dedup_key(&self, url: &Url) -> Option<String> {
        match self {
            SourceClient::Reddit(c) => c.dedup_key(url),
            SourceClient::GitHub(c) => c.dedup_key(url),
            SourceClient::HackerNews(c) => c.dedup_key(url),
        }
    }
}

/// The per-account settings every `build_*_job` resolves the same way: the
/// `limit`/`toread`/`shared` trio through [`resolve_setting`] plus the dates through
/// [`resolve_dates`], each on the flag → account → per-source default → `[pinboard]`
/// global ladder. Leaves client/secret/hook construction to each source builder.
fn resolve_job_settings(
    account: Option<&impl config::Account>,
    ovr: &SyncOverrides,
    src: &config::SourceDefaults,
    config: &Config,
) -> JobSettings {
    JobSettings {
        limit: resolve_setting(
            ovr.flags.limit,
            account.and_then(|a| a.limit()),
            src.limit,
            config.pinboard.limit.unwrap_or_default(),
        ),
        toread: resolve_setting(
            ovr.flags.toread,
            account.and_then(|a| a.toread()),
            src.toread,
            config.pinboard.toread,
        ),
        shared: resolve_setting(
            ovr.flags.public,
            account.and_then(|a| a.public()),
            src.public,
            config.pinboard.public,
        ),
        dates: resolve_dates(&DateOverrides::from(&ovr.flags), account, src, config),
    }
}

/// What `sync`, `backup` and `doctor` need from one source's account type, so each
/// handles the three sources through one generic path: where its accounts and per-source
/// defaults live in the config, how its client is built from an account plus the CLI
/// overrides, and which auth-failure hook its jobs carry.
pub trait SourceAccount: config::Account + Sized {
    /// The source's name in job labels, backup file stems and `doctor` lines.
    const SOURCE: &'static str;

    /// The source's configured accounts (`[[reddit]]` and so on).
    fn configured(config: &Config) -> &[Self];

    /// The source's `[defaults.<source>]` tier.
    fn defaults(config: &Config) -> &config::SourceDefaults;

    /// The account's own `on_auth_failure` override.
    fn on_auth_failure(&self) -> Option<&str>;

    /// The client for `account`, or for the implicit CLI/env account when `None`, with
    /// its credentials resolved through the secret ladder.
    fn client(account: Option<&Self>, ovr: &SyncOverrides) -> Result<SourceClient>;

    /// The auth-failure hook for `account`: `top` (the flag or env value the command
    /// resolved), else the account's, else the per-source default, else `[hooks]`. Both
    /// `sync` jobs and `cleanup` resolve it here, so a source that never re-authenticates
    /// can rule a hook out for every command at once.
    fn hook(account: Option<&Self>, top: Option<String>, config: &Config) -> Option<String> {
        resolve_hook(
            top,
            account.and_then(Self::on_auth_failure),
            Self::defaults(config).on_auth_failure.as_deref(),
            config,
        )
    }
}

impl SourceAccount for RedditAccount {
    const SOURCE: &'static str = "reddit";

    fn configured(config: &Config) -> &[Self] {
        &config.reddit
    }

    fn defaults(config: &Config) -> &config::SourceDefaults {
        &config.defaults.reddit
    }

    fn on_auth_failure(&self) -> Option<&str> {
        self.on_auth_failure.as_deref()
    }

    fn client(account: Option<&Self>, ovr: &SyncOverrides) -> Result<SourceClient> {
        let username = resolve_secret(
            ovr.reddit_username.clone(),
            "REDDIT_USERNAME",
            account.and_then(|a| a.username.clone()),
            None,
        )
        .context("missing Reddit username (set --reddit-username, REDDIT_USERNAME, or `username` in the config)")?;
        let cookie = resolve_reddit_cookie(ovr.reddit_cookie.clone(), account);
        let reddit_config = account.map(RedditConfig::from).unwrap_or_default();
        Ok(SourceClient::Reddit(RedditClient::for_user(
            username,
            cookie,
            reddit_config,
        )?))
    }
}

impl SourceAccount for GitHubAccount {
    const SOURCE: &'static str = "github";

    fn configured(config: &Config) -> &[Self] {
        &config.github
    }

    fn defaults(config: &Config) -> &config::SourceDefaults {
        &config.defaults.github
    }

    fn on_auth_failure(&self) -> Option<&str> {
        self.on_auth_failure.as_deref()
    }

    fn client(account: Option<&Self>, ovr: &SyncOverrides) -> Result<SourceClient> {
        let token = resolve_github_token(ovr.github_token.clone(), account)
            .with_context(missing_github_token)?;
        let github_config = account.map(GitHubConfig::from).unwrap_or_default();
        Ok(SourceClient::GitHub(GitHubClient::new(
            token,
            github_config,
        )?))
    }
}

impl SourceAccount for HackerNewsAccount {
    const SOURCE: &'static str = "hackernews";

    fn configured(config: &Config) -> &[Self] {
        &config.hackernews
    }

    fn defaults(config: &Config) -> &config::SourceDefaults {
        &config.defaults.hackernews
    }

    fn on_auth_failure(&self) -> Option<&str> {
        None
    }

    fn client(account: Option<&Self>, ovr: &SyncOverrides) -> Result<SourceClient> {
        let username = resolve_secret(
            ovr.hackernews_username.clone(),
            "HN_USERNAME",
            account.and_then(|a| a.username.clone()),
            None,
        )
        .context(
            "missing HackerNews username (set --username, HN_USERNAME, or `username` in the config)",
        )?;
        let hn_config = account.map(HackerNewsConfig::from).unwrap_or_default();
        Ok(SourceClient::HackerNews(HackerNewsClient::new(
            username, hn_config,
        )?))
    }

    /// None: HackerNews favorites are public, so nothing can need re-authenticating.
    fn hook(_: Option<&Self>, _: Option<String>, _: &Config) -> Option<String> {
        None
    }
}

/// The `sync` job for `account` (or the implicit CLI/env account): its client, label and
/// hook, and its settings resolved through [`resolve_job_settings`].
pub fn build_job<A: SourceAccount>(
    account: Option<&A>,
    ovr: &SyncOverrides,
    config: &Config,
) -> Result<SyncJob> {
    Ok(SyncJob {
        client: A::client(account, ovr)?,
        label: job_label(account),
        hook: A::hook(account, ovr.on_auth_failure.clone(), config),
        settings: resolve_job_settings(account, ovr, A::defaults(config), config),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroUsize;

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

    #[test]
    fn date_settings_stale_to_now_flag_tops_the_tier() {
        let config = Config::default(); // global cleanup_stale_to_now = false
        let src = &config.defaults.reddit;

        // flag forces it on against a false global
        let over = DateOverrides {
            stale_to_now: Some(true),
            ..Default::default()
        };
        let d = resolve_dates(&over, None::<&RedditAccount>, src, &config);
        assert!(d.stale_to_now);

        // flag forces it off against a true global
        let mut config = Config::default();
        config.pinboard.cleanup_stale_to_now = true;
        let src = &config.defaults.reddit;
        let over = DateOverrides {
            stale_to_now: Some(false),
            ..Default::default()
        };
        let d = resolve_dates(&over, None::<&RedditAccount>, src, &config);
        assert!(!d.stale_to_now);
    }

    #[test]
    fn date_settings_max_age_days_flag_tops_the_tier() {
        let mut config = Config::default();
        config.pinboard.post_date_max_age_days = Some(10); // global
        let src = &config.defaults.reddit; // per-source default None

        // flag wins
        let over = DateOverrides {
            max_age_days: Some(99),
            ..Default::default()
        };
        let d = resolve_dates(&over, None::<&RedditAccount>, src, &config);
        assert_eq!(d.max_age_days, 99);

        // no flag → global
        let d = resolve_dates(
            &DateOverrides::default(),
            None::<&RedditAccount>,
            src,
            &config,
        );
        assert_eq!(d.max_age_days, 10);

        // global unset → built-in default
        let config = Config::default();
        let src = &config.defaults.reddit;
        let d = resolve_dates(
            &DateOverrides::default(),
            None::<&RedditAccount>,
            src,
            &config,
        );
        assert_eq!(d.max_age_days, config::DEFAULT_MAX_AGE_DAYS);
    }

    /// Override values chosen so each setting is distinct from the others and from the
    /// `Config::default()` globals — so a builder that wires a flag into the *wrong*
    /// `SyncJob` field (e.g. `public` into the `toread` slot) fails the assertions.
    fn wiring_overrides() -> SyncOverrides {
        SyncOverrides {
            flags: SyncFlags {
                limit: Some(SyncLimit::Max(NonZeroUsize::new(7).unwrap())),
                toread: Some(true),        // global default is false
                public: Some(false),       // distinct from toread, so a swap is caught
                use_post_date: Some(true), // global default is false
                max_age_days: Some(99),    // global default is 30
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn assert_wired(job: &SyncJob) {
        let settings = &job.settings;
        assert_eq!(
            settings.limit,
            SyncLimit::Max(NonZeroUsize::new(7).unwrap()),
            "limit"
        );
        assert!(settings.toread, "toread");
        assert!(!settings.shared, "shared (from --public=false)");
        assert!(settings.dates.use_post_date, "use_post_date");
        assert_eq!(settings.dates.max_age_days, 99, "max_age_days");
    }

    /// The write settings of a job a test only fetches through: everything off but `limit`.
    pub(super) fn fetch_only(limit: SyncLimit) -> JobSettings {
        JobSettings {
            limit,
            toread: false,
            shared: false,
            dates: DateOpts {
                use_post_date: false,
                max_age_days: 30,
                stale_to_now: false,
            },
        }
    }

    #[test]
    fn build_job_wires_reddit_overrides_to_job_fields() {
        let ovr = SyncOverrides {
            reddit_username: Some("alice".into()),
            reddit_cookie: Some("reddit_session=x".into()),
            ..wiring_overrides()
        };
        let job = build_job::<RedditAccount>(None, &ovr, &Config::default()).expect("builds");
        assert_wired(&job);
    }

    #[test]
    fn build_job_wires_github_overrides_to_job_fields() {
        let ovr = SyncOverrides {
            github_token: Some("ghp_test".into()),
            ..wiring_overrides()
        };
        let job = build_job::<GitHubAccount>(None, &ovr, &Config::default()).expect("builds");
        assert_wired(&job);
    }

    #[test]
    fn build_job_wires_hackernews_overrides_to_job_fields() {
        let ovr = SyncOverrides {
            hackernews_username: Some("alice".into()),
            ..wiring_overrides()
        };
        let job = build_job::<HackerNewsAccount>(None, &ovr, &Config::default()).expect("builds");
        assert_wired(&job);
    }

    /// `sync --all`'s build phase must isolate a per-account build failure so the
    /// healthy accounts still yield jobs. Here a GitHub account with no resolvable
    /// token (no inline token, no `token_file`, and this test sets no fake `$GITHUB_TOKEN`)
    /// fails to build; it must be counted, not abort the whole run.
    #[test]
    fn build_all_jobs_isolates_a_failing_account() {
        let config = Config {
            reddit: vec![RedditAccount {
                username: Some("alice".into()),
                cookie: Some("reddit_session=x".into()),
                ..Default::default()
            }],
            github: vec![GitHubAccount::default()],
            ..Default::default()
        };
        let (jobs, prebuilt_failures) = build_all_jobs(&config, &SyncOverrides::default());
        assert_eq!(jobs.len(), 1, "healthy reddit account still builds");
        assert_eq!(jobs[0].label, "reddit[alice]");
        assert!(
            prebuilt_failures >= 1,
            "the tokenless github account is counted, not aborted"
        );
    }
}

/// Tests that need a socket; the flake skips `net_tests` in the Nix build sandbox.
#[cfg(test)]
mod net_tests {
    use super::tests::fetch_only;
    use super::*;
    use serde_json::json;
    use std::num::NonZeroUsize;
    use wiremock::matchers::{method, path, query_param, query_param_is_missing};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Fetches through a HackerNews `SyncJob` capped at `limit`, against favorites whose
    /// first page (both streams) is already synced and links to a second page that is
    /// expected `second_page_hits` times.
    async fn fetch_hackernews_job(limit: SyncLimit, second_page_hits: u64) {
        let hn = MockServer::start().await;
        let page = |rows: &str| {
            ResponseTemplate::new(200).set_body_string(format!(
                r#"<html op="favorites"><body><table>{rows}</table></body></html>"#
            ))
        };
        Mock::given(method("GET"))
            .and(path("/favorites"))
            .and(query_param_is_missing("p"))
            .respond_with(page(
                r#"<tr class="athing" id="1"><td>x</td></tr>
                   <tr><td><a class="morelink" href="favorites?id=u&amp;p=2">More</a></td></tr>"#,
            ))
            .expect(2)
            .mount(&hn)
            .await;
        Mock::given(method("GET"))
            .and(path("/favorites"))
            .and(query_param("p", "2"))
            .respond_with(page(r#"<tr class="athing" id="2"><td>x</td></tr>"#))
            .expect(second_page_hits)
            .mount(&hn)
            .await;
        let algolia = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "hits": [
                { "objectID": "1", "title": "Ask HN: one", "author": "a", "_tags": ["story"] },
                { "objectID": "2", "title": "Ask HN: two", "author": "a", "_tags": ["story"] },
            ] })))
            .mount(&algolia)
            .await;
        let job = SyncJob {
            client: SourceClient::HackerNews(HackerNewsClient::with_base_urls(
                "u".into(),
                HackerNewsConfig::default(),
                hn.uri(),
                algolia.uri(),
            )),
            label: "hackernews[u]".into(),
            hook: None,
            settings: fetch_only(limit),
        };
        let existing = [Bookmark {
            url: Url::parse("https://news.ycombinator.com/item?id=1").unwrap(),
            title: String::new(),
            note: String::new(),
            tags: Vec::new(),
            timestamp: None,
            public: false,
            read_later: false,
        }];

        crate::http::recording_waits(job.fetch(&existing))
            .await
            .0
            .unwrap();
    }

    #[tokio::test]
    async fn an_auto_sync_job_fetches_hackernews_incrementally() {
        fetch_hackernews_job(SyncLimit::Auto, 0).await;
    }

    #[tokio::test]
    async fn an_all_sync_job_reads_every_favorites_page() {
        fetch_hackernews_job(SyncLimit::All, 2).await;
    }

    #[tokio::test]
    async fn a_capped_sync_job_reads_every_favorites_page() {
        fetch_hackernews_job(SyncLimit::Max(NonZeroUsize::new(5).unwrap()), 2).await;
    }

    /// Under each limit, fetches through a job built by `client` against two mocked
    /// pages whose first is already on Pinboard (`existing`), and asserts how often the
    /// second page was read: never under `auto`, once under `all` and a count.
    async fn assert_second_page_reads(
        mount: impl AsyncFn(&MockServer),
        client: impl Fn(&MockServer) -> SourceClient,
        is_second_page: impl Fn(&wiremock::Request) -> bool,
        existing: &[Bookmark],
    ) {
        let limits = [
            (SyncLimit::Auto, 0),
            (SyncLimit::All, 1),
            (SyncLimit::Max(NonZeroUsize::new(5).unwrap()), 1),
        ];
        for (limit, expected_reads) in limits {
            let server = MockServer::start().await;
            mount(&server).await;
            let job = SyncJob {
                client: client(&server),
                label: "test".into(),
                hook: None,
                settings: fetch_only(limit),
            };

            crate::http::recording_waits(job.fetch(existing))
                .await
                .0
                .unwrap();
            let requests = server.received_requests().await.unwrap();
            let second_page_reads = requests.iter().filter(|r| is_second_page(r)).count();
            assert_eq!(second_page_reads, expected_reads, "under {limit:?}");
        }
    }

    #[tokio::test]
    async fn only_an_auto_sync_job_stops_reddit_at_a_synced_page() {
        let mount = async |server: &MockServer| {
            for (after, id, next) in [(None, "n", Some("t3_n")), (Some("t3_n"), "o", None)] {
                let saved = Mock::given(method("GET")).and(path("/user/u/saved.json"));
                let saved = match after {
                    None => saved.and(query_param_is_missing("after")),
                    Some(after) => saved.and(query_param("after", after)),
                };
                saved
                    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                        "kind": "Listing",
                        "data": { "after": next, "children": [
                            { "kind": "t3", "data": { "name": format!("t3_{id}"),
                              "subreddit": "rust", "title": id,
                              "permalink": format!("/r/rust/comments/{id}/x/") } }
                        ] }
                    })))
                    .mount(server)
                    .await;
            }
        };
        assert_second_page_reads(
            mount,
            |server| {
                SourceClient::Reddit(RedditClient::for_test(None, Some("u".into()), server.uri()))
            },
            |request| request.url.query_pairs().any(|(key, _)| key == "after"),
            &[crate::test_support::stored_at(
                "https://old.reddit.com/r/rust/comments/n/x/",
            )],
        )
        .await;
    }

    #[tokio::test]
    async fn only_an_auto_sync_job_stops_github_at_a_synced_page() {
        let mount = async |server: &MockServer| {
            for (page, name) in [(1, "s/n"), (2, "s/o")] {
                let mut response = ResponseTemplate::new(200).set_body_json(json!([
                    { "starred_at": "2023-01-02T03:04:05Z",
                      "repo": { "full_name": name,
                                "html_url": format!("https://github.com/{name}") } }
                ]));
                if page == 1 {
                    let next = format!("<{}/user/starred?page=2>; rel=\"next\"", server.uri());
                    response = response.insert_header("link", next.as_str());
                }
                Mock::given(method("GET"))
                    .and(path("/user/starred"))
                    .and(query_param("page", page.to_string().as_str()))
                    .respond_with(response)
                    .mount(server)
                    .await;
            }
        };
        assert_second_page_reads(
            mount,
            |server| {
                SourceClient::GitHub(GitHubClient::with_base_url(
                    "tok".into(),
                    GitHubConfig::default(),
                    server.uri(),
                ))
            },
            |request| {
                request
                    .url
                    .query_pairs()
                    .any(|(key, value)| key == "page" && value == "2")
            },
            &[crate::test_support::stored_at("https://github.com/s/n")],
        )
        .await;
    }
}
