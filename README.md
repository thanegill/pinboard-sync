# pinboard-sync

> **AI Disclosure:** This project falls under [Level 5: Bots coded, human understands
> completely](https://www.visidata.org/blog/2026/ai/). Exact model used varies by commit
> and is in the commit message. This is disclosed as useful context for anyone depending
> on, auditing, or forking this code, not because I think it lowers the bar on the
> quality. Reach out or report bugs the way you would for any other project.

A Rust CLI that copies what you save on the web into one [Pinboard](https://pinboard.in)
account, then tidies those bookmarks. It reads:

- **Reddit**: your saved posts and comments.
- **GitHub**: your starred repositories and starred gists.
- **HackerNews**: your favorited stories and comments.

Each run reads a source, skips what's already on Pinboard, and writes the rest. Pinboard
is the only state, with no local database, so runs are idempotent and safe to schedule.

## Install

With Nix, which also installs bash/zsh/fish completions, the man page and the example
config (`share/pinboard-sync/config.example.toml`):

```sh
nix build            # ./result/bin/pinboard-sync
nix run . -- --help
```

Or with Cargo, which needs OpenSSL and pkg-config on Linux:

```sh
cargo build --release
```

## Authentication

Every command that touches Pinboard needs your Pinboard API token (`username:TOKEN`,
from your [settings](https://pinboard.in/settings/password)). Each source adds its own:

- **Reddit** needs your **username** and a **`reddit_session` cookie**, since Reddit has
  no self-serve API and its edge blocks cookieless requests. Copy `reddit_session` from
  your browser's DevTools (Application → Cookies → reddit.com); its bare value and
  `reddit_session=<value>` both work. It lasts about a year. When it expires the run
  exits non-zero and runs the [auth-failure hook](#auth-failure-hook).
- **GitHub** needs a **personal access token**. It only reads: your starred repos and
  gists, and in `cleanup` each repo and gist they name. A classic token needs no scope
  (add `repo` to include private repos you starred). A fine-grained token needs the
  account permission **Starring: read**.
- **HackerNews** needs only your **username**; favorites are public.

### Secrets and environment variables

Each value comes from the first non-blank of, in order: a CLI flag, the environment
variable, the variable's `_FILE` form, the config file's inline value, then its `*_file`
path. Values are trimmed. A `_FILE` variable holds a **path whose trimmed contents are
the value**, so the secret itself never sits in the environment or the Nix store:

```sh
export PINBOARD_TOKEN='user:abcdef0123456789'
export PINBOARD_TOKEN_FILE=/run/secrets/pinboard-token   # same, read from a file
```

| Variable (each also as `<VAR>_FILE`) | Sets | Secret |
| --- | --- | --- |
| `PINBOARD_TOKEN` | Pinboard API token (`user:TOKEN`), shared by every source | yes |
| `REDDIT_USERNAME` | Reddit user whose saved items to read | no |
| `REDDIT_COOKIE` | Reddit `reddit_session` cookie, bare or as `reddit_session=…` | yes |
| `GITHUB_TOKEN` | GitHub personal access token | yes |
| `HN_USERNAME` | HackerNews user whose favorites to read | no |

Two variables take a direct value and have no `_FILE` form: `PINBOARD_SYNC_CONFIG`, the
path to the config file (same as `--config`), and `PINBOARD_SYNC_ON_AUTH_FAILURE`.

### Auth-failure hook

A shell command run when a source needs re-authentication (an expired Reddit cookie, a
GitHub 401), on `sync`, `cleanup` and `backup`. It runs via `/bin/sh -c` (or `sh` from
`PATH` when there is no `/bin/sh`) with `PINBOARD_SYNC_AUTH_ERROR` and
`PINBOARD_SYNC_EVENT` set. It resolves `--on-auth-failure` (on `sync` and `backup`;
`cleanup` has none) or `$PINBOARD_SYNC_ON_AUTH_FAILURE`, then the account's
`on_auth_failure`, then `[defaults.<source>]`, then `[hooks]`. It does not run for a
rate limit, which only waiting clears, nor for Reddit's "blocked by network security"
page, which refuses the request whatever cookie it carries.

## Usage

The examples assume `PINBOARD_TOKEN` is set. `pinboard-sync <command> <source> --help`
lists every flag, and `pinboard-sync man` prints the man page.

```sh
pinboard-sync sync reddit --reddit-username you --reddit-cookie 'reddit_session=…'
pinboard-sync sync github                     # GITHUB_TOKEN
pinboard-sync sync hackernews --hn-username you
pinboard-sync sync reddit --dry-run           # preview without writing

pinboard-sync cleanup reddit                  # URLs → configured domain, tags, NSFW, titles
pinboard-sync cleanup github                  # canonicalize repo + gist URLs, follow renames
pinboard-sync cleanup hackernews              # rewrite HN item URLs to the linked article

pinboard-sync doctor                          # check the token and every account

pinboard-sync backup --all --out ~/snapshots/pinboard-sync
pinboard-sync backup pinboard --out DIR       # just the Pinboard account
pinboard-sync backup reddit main --out DIR    # just one source account
```

The per-run settings (`--limit`, `--toread`, `--public`, `--use-post-date`,
`--post-date-max-age-days`, `--cleanup-stale-to-now`) are flags or config keys, never
environment variables. They resolve flag → account → `[defaults.<source>]` →
`[pinboard]` → built-in default. A boolean flag alone means `true`, and `--toread=false`
turns off a config-set `true`.

A flag a command shares with its subcommand, such as `--dry-run`, `--out` or a `cleanup`
date flag, can go on either side: `cleanup --dry-run reddit` equals
`cleanup reddit --dry-run`. Giving it on both sides is an error, as is giving `--config`
twice at any level. `-v` adds up wherever it appears: `-v sync reddit -v` logs at trace.

**Exit status** is 0 on success and non-zero on failure. `sync` fails when a write or an
account fails, `doctor` when any check fails, and `backup` when any target fails.
`cleanup` fails when a write fails, when a source stops the pass (a dead credential, a
rate limit, a block), or when most of its lookups fail. A blocked Reddit `/api/info`
falls back to the saved listing instead (see [Troubleshooting](#troubleshooting)). A few
failed lookups only log a warning, so one dead URL can't fail a scheduled run for good,
and a refused rewrite doesn't fail the run.

### Logging

Progress goes to **stderr** at `info` (counts per account, source and run), so stdout
stays clean for output like `--dry-run` listings. `-v` is debug, `-vv` trace and `-vvv`
adds dependency logs; `RUST_LOG` (for example `RUST_LOG=pinboard_sync=debug`) overrides
`-v`. An error quotes at most 300 bytes of a server's response, on one line. Debug level
logs the whole body, with the Pinboard token, GitHub token and Reddit cookie redacted.
Under the NixOS service the log is in the journal (`journalctl -u pinboard-sync`). To
debug a unit (`pinboard-sync`, `pinboard-sync-cleanup` or `pinboard-sync-backup`), set
`RUST_LOG` in its environment, for example:

```nix
systemd.services.pinboard-sync-cleanup.environment.RUST_LOG = "pinboard_sync=debug";
```

## What `sync` does

Each saved item becomes one bookmark:

| Source | URL | Title | Tags (defaults) |
| --- | --- | --- | --- |
| Reddit | the `old.reddit.com` permalink | post/comment title | `reddit`, `subreddit:<sub>`, `reddit-comment`, `nsfw`, `author:reddit:<u>`, `reddit-flair:<f>`, `type:image`/`type:video` |
| GitHub | the repo URL | `owner/repo` | `github-star`, `lang:<language>`, `author:github:<owner>` |
| GitHub gist | `gist.github.com/<owner>/<id>` | the description, else the first filename | `github-star`, `github-gist`, `lang:<language>` per file language, `author:github:<owner>` |
| HackerNews | the article (or HN permalink) | story title | `hackernews`, `hackernews-comment`, `author:hackernews:<u>`, `hackernews:show-hn` (and `ask-hn`/`tell-hn`/`launch-hn`) |

A favorited HN story is bookmarked as its article, with the discussion in the notes; a
favorited comment keeps its HN permalink. A gist's notes quote its description and list
its files; `gists = false` on a `[[github]]` account skips gists. Dedup is by URL, so an
item already bookmarked, under any Reddit subdomain or from elsewhere, is left alone.
Reddit caps any listing at about 1000 items.

`--limit` (or `limit` in the config) takes `auto`, `all` or a number. `auto`, the
default, stops reading a source at the first page with nothing new, so a caught-up run
reads one page per source (one each for HN stories and comments, and for GitHub stars
and gists). `all` reads every page. A number `N` caps the run at N new bookmarks and
reads every page; `0` is rejected. New items are written oldest first, so the next run
finishes one that was interrupted. `auto` has known limits, all fixed by one
`sync <source> --limit all`:

- An older item you deleted from Pinboard is not added back.
- If a write fails and a page's worth of newer items in the same run succeed, later runs
  don't reach the failed item.
- A page whose items are all on Pinboard already stops the read, even when older items
  below it are missing. That happens on a first sync whose newest items you saved by
  hand or with another tool, or when such items move above an unwritten one.
- GitHub doesn't document the order of starred gists, so they are assumed newest first.
  Under 100 gists fit on one page, where order doesn't matter.

`backup`, `doctor` and `cleanup github` with `use_post_date` always read every page.

## What `cleanup` does

`cleanup` repairs the bookmarks already on Pinboard: URLs, tags and titles. It touches
only bookmarks it recognizes as that source's, rewrites one only when a field changes,
and takes `--dry-run` to preview. `cleanup --all` runs github, reddit and hackernews in
turn over the shared bookmark set.

**It can merge two of your bookmarks into one.** Pinboard keeps one record per URL, so
when cleanup moves a bookmark onto a URL you saved separately (an HN story whose article
you also bookmarked, a repo starred under its old and new names), the two are merged:
tags are unioned, notes concatenated, and the bookmark already there keeps its title,
date and to-read state. **The absorbed bookmark is then deleted**, and when the merge
adds nothing, that delete is all it does. Before your first real run, read the
`absorb <url>` lines `--dry-run` prints, one per bookmark a merge would delete.

**Some rewrites are refused** and reported, leaving the bookmark and any duplicate
untouched. Cleanup won't write over a bookmark it couldn't read this run, merge
bookmarks that disagree about being public, or merge one whose notes the merged record
has no room for, or whose tags would crowd out the note already there. A bookmark it
couldn't read is tried again next run; resolve the others by hand.

**Long notes are cut.** Pinboard's API carries the note in the request URL, so a note
too long for it is cut, with a marker, when written (and logged). Tags and other fields
are always sent whole, which leaves the note less room; when they leave none, the note
is cut to Pinboard's own limit or dropped. `cleanup` doesn't rewrite a note already
stored cut, or cut a long one for a change past the cut. If Pinboard still rejects a
`cleanup` write as too long, nothing is written or deleted for that bookmark and it
counts as refused; `--dry-run` can't foresee this. `sync` retries a new bookmark with a
shorter note instead. A bookmark too long even with no note is never sent: `sync` fails
it, and `cleanup` refuses it before writing anything, which `cleanup --dry-run` shows
(`sync`'s dry run can't).

- **Reddit**: moves each Reddit bookmark to your `reddit_domain` (default
  `old.reddit.com`), unwrapping `over18` redirects. A bookmark already on some
  `[[reddit]]` account's domain stays there, with that account's tag settings, and a
  post stored on several collapses onto the first listed. It normalizes tags (every
  `tags` entry and a correctly-cased `subreddit:<sub>`, dropping legacy duplicates) and
  tags comments `reddit-comment`. Through Reddit's `/api/info`, which needs the cookie,
  it tags `over_18` posts `nsfw`, replaces placeholder titles, and adds the author,
  flair and media tags `sync` writes, each only when no tag with that prefix exists yet.
  A comment is tagged from its own entry, never its post's. With `--no-nsfw --no-titles`
  and no `use_post_date`, it skips `/api/info` and needs no cookie.
- **GitHub** (needs the GitHub token): canonicalizes repo URLs to
  `https://github.com/<owner>/<repo>` (https, lowercase host, no `.git`, trailing slash,
  query or fragment), leaving deeper links like `/issues` alone, and follows renames and
  transfers through the API. It refreshes only what `sync github` wrote: notes when
  empty or in sync's shape; base, `lang:` and owner tags on bookmarks carrying an
  account's first non-empty `tags` entry (`github-star`), with that account's settings;
  the title while it is still `owner/repo`; and, under `use_post_date`, the running
  account's star date while title and notes are still the tool's. So a repo saved by
  hand or by `sync hackernews` keeps its title, tags and date. A repo gone (404),
  blocked (DMCA 451, ToS 403) or behind SSO the token can't use keeps only the URL fix,
  with a warning for the last two.

  Gists get the same treatment in a second pass, unless the running account has
  `gists = false`. Each is canonicalized to `https://gist.github.com/<owner>/<id>`,
  owner-less links included. Its tags, and its title while sync's, are refreshed only
  when it carries an account's markers: the first `gist_tags` entry and the first one
  `tags` lacks (`github-star` and `github-gist` by default), so a hand-saved gist tagged
  `github-star` keeps its title and tags. When every `gist_tags` entry is also in
  `tags`, the first alone is the marker, so a hand-saved gist with that tag counts as
  synced. Under `use_post_date` a gist is re-dated by `gist_post_date` while its title
  and notes are still the tool's. A deleted gist keeps its URL.
- **HackerNews**: rewrites favorited story bookmarks on `news.ycombinator.com/item?id=…`
  to the linked article, re-deriving title, `HN Link:` notes and tags; comments keep
  their permalink. It needs only the Pinboard token. `--link-discussions` goes the other
  way: for each article bookmark you tagged with the marker (`find-hn`, or `--tag-link`
  / `tag_link`), it looks the URL up on HN and, if there is a discussion, adds an
  `HN Link:` line and swaps the marker for the HN base tags.

`cleanup reddit` also moves, re-tags and (under `use_post_date`) re-dates an HN story
that links a Reddit post, and `cleanup github` canonicalizes and follows renames for one
linking a repo. `sync hackernews` still recognizes those, except a renamed repo's: a
sync reading that far back re-adds the story at the old URL, and the next
`cleanup github` merges it back.

### Several accounts of one source

`cleanup` runs as one account per source: the one named, else the first (each source's
first under `cleanup --all`). Its dating settings apply to the whole source, and its tag
settings can rewrite what another account's `sync` wrote:

- **HackerNews** adds the running account's tags to every favorited comment and text
  post. Keep tag settings the same across HackerNews accounts.
- **GitHub** uses the settings of the account whose base tag a star carries (the running
  account's when two share one), so accounts sharing `github-star` add their tags to
  each other's stars, and cleanup run as an account whose prefix starts another's
  (`lang:` and `lang:work:`) drops the other's prefixed tags. Gists behave the same by
  their `gist_tags` markers. Give each account its own base tag and a first `gist_tags`
  entry no other account holds (never `github-gist`), or the same tag settings.
- **Reddit** shapes a bookmark with the tags of the account whose `reddit_domain` it is
  on, the running account's when several share a domain, so accounts sharing a domain
  add their tags to each other's bookmarks. Give each account its own `reddit_domain`,
  or the same tag settings.

## What `backup` does

`backup` only reads. It snapshots every service to a directory, so your data doesn't
depend on Pinboard or the sources keeping it. It also keeps what `sync` drops: every API
field, not only the few `sync` uses, and, in each source's snapshot, the full text of
notes Pinboard stores cut.

```
<dir>/raw/pinboard.json                      # posts/all with meta=yes, every field
<dir>/raw/reddit-main.json                   # every saved.json page envelope
<dir>/raw/github-personal.json               # every /user/starred entry
<dir>/raw/github-personal-gists.json         # every /gists/starred entry
<dir>/raw/hackernews-me.json                 # every Algolia response
<dir>/raw/hackernews-me-favorites.html       # the scraped favorites pages
<dir>/normalized/<same stems>.json           # the same items as domain bookmarks
<dir>/manifest.json                          # run timestamp, version, per-file counts
```

- `raw/` and `normalized/` come from one read, so they describe the same moment. `raw/`
  keeps every field but isn't byte-for-byte: a service's pages are merged into one file,
  which sorts object keys. A JSON-array response flattens into one array (GitHub,
  Pinboard); anything else is one element per page (Reddit, Algolia). HackerNews
  favorites are HTML pages joined behind marker comments.
- `normalized/` has one shape for every service: `url`, `title`, `note`, `tags`,
  `timestamp`, `public`, `read_later` and the source's `dedup_key`.
- Each run replaces the last snapshot in place, with no history or pruning; point a
  backup tool (git, borg, Time Machine) at the directory for that. Files are written
  atomically at mode 0600, and a body that doesn't parse never overwrites a good file.
  Run one backup at a time per directory.
- `manifest.json` is written last. If a target fails it has `complete: false` and lists
  the `failed` targets. Each file's entry has its own `generated_at` and counts `items`,
  or `pages` for page envelopes. A narrowed run such as `backup pinboard` updates only
  its own entries. A file that isn't a trustworthy snapshot gets an `unusable` reason.
- Each source stops at a page cap against runaway paging. Hitting it fails the target
  rather than overwriting a full snapshot with part of one.
- The directory comes from `--out DIR` or `[backup].directory`, with no default, and an
  empty one is an error. `backup <target>` runs one service or account, and `--all` the
  Pinboard account plus every configured account. Backing up a source needs no Pinboard
  token. `--dry-run` prints the file plan but still fetches everything.

## What `doctor` does

`doctor` checks the Pinboard token, the backup directory (when one is configured) and
every configured account, by fetching each source. It prints a ✓ or ✗ line per check and
exits non-zero if any failed. It checks only the accounts in the config, so an account
given only by flags or environment variables isn't checked.

## Config file

A TOML file passed with `--config` (or `$PINBOARD_SYNC_CONFIG`) holds one `[pinboard]`
destination, optional `[hooks]`, `[backup]` and `[defaults.<source>]` tables, and an
array of accounts per source. `pinboard-sync config example` prints every key with its
default and a note on it, so start from it. Its source is
[`src/config.example.toml`](src/config.example.toml).

```sh
pinboard-sync config example > pinboard-sync.toml
$EDITOR pinboard-sync.toml
pinboard-sync config check pinboard-sync.toml
pinboard-sync --config pinboard-sync.toml sync --all --dry-run
```

```toml
[pinboard]
token_file = "/run/secrets/pinboard-token"
use_post_date = true          # global default

[defaults.reddit]
use_post_date = false         # ...but not for reddit accounts

[[reddit]]
name = "main"
username = "you"
cookie_file = "/run/secrets/reddit-cookie"
use_post_date = true          # ...except this one

[[github]]
name = "personal"
token_file = "/run/secrets/github-token"

[[hackernews]]
username = "you"
```

Every key is optional, and an unknown key is an error. Settings resolve flag → account →
`[defaults.<source>]` → `[pinboard]` (or `[hooks]`) → built-in default.

**Selecting accounts.** A command uses the account named, else the first of that source:

```sh
pinboard-sync --config ./config.toml sync github personal   # one named account
pinboard-sync --config ./config.toml sync github --all      # every github account
pinboard-sync --config ./config.toml sync --all             # every account, every source
pinboard-sync --config ./config.toml cleanup --all          # github, reddit, then hackernews
```

An account's selector is its `name`, else its `username` (Reddit and HackerNews), so
`username = "alice"` is `sync reddit alice`; a GitHub account needs a `name`. At most
one account per source may lack a selector, and no two selectors may make the same
backup filename (`a.b` and `a/b`, `Alice` and `alice`, or `default` and no selector). A
username from a flag or the environment doesn't count, as it applies to every account.

**Tags are config-only**; there are no tag flags. Setting `tags` replaces the default
list, so include the base tag yourself. Setting a prefixed or conditional tag to `""`
disables it. A tag or prefix with whitespace or a comma is rejected, and values placed
in a prefix are slugged (`Jupyter Notebook` → `lang:jupyter-notebook`). `reddit_domain`
is a bare host, with no scheme, path or port.

**Dating by the source post date.** With `use_post_date`, a bookmark is dated to when
the item was posted (Reddit `created_utc`, HackerNews `created_at`) or starred (GitHub).
A gist has no star date, so `gist_post_date` picks `now` (the default), `created` or
`updated`. `cleanup reddit` reads the date from `/api/info` and so needs the cookie;
`cleanup github` reads the star list. `post_date_max_age_days` (default 30) bounds how
far back: an older post gets "now" from `sync`, while `cleanup` keeps the bookmark's
date unless `cleanup_stale_to_now` re-dates it to now, at most once per cap period. A
cap of `0` with `cleanup_stale_to_now` is rejected. When the source gives no date, the
bookmark's date is left alone.

## Utility commands

- `config example` prints the commented template above; it is built into the binary, so
  it always matches the version you run.
- `config check <file>` validates a config as a run would: unknown keys, bad values,
  whitespace or commas in tags, a malformed `reddit_domain` and duplicate account names.
  It opens no `*_file` path and contacts no service; `doctor` does that. An error exits
  non-zero and names the key, plus the line for a syntax or type error, never the value:

  ```console
  $ pinboard-sync config check pinboard-sync.toml
  pinboard-sync.toml: OK (1 reddit, 0 github, 1 hackernews account(s))
  $ pinboard-sync config check typo.toml
  ERROR parsing config: line 3, column 1: unknown field `tokn`, expected one of `token`, …
  ```

- `completions <shell>` prints a script for `bash`, `zsh`, `fish`, `elvish` or
  `powershell`. Re-run it after upgrading, and reload the shell (`compinit` for zsh):

  ```sh
  pinboard-sync completions bash > ~/.local/share/bash-completion/completions/pinboard-sync
  pinboard-sync completions zsh > ~/.zfunc/_pinboard-sync     # any dir on $fpath
  pinboard-sync completions fish > ~/.config/fish/completions/pinboard-sync.fish
  ```

- `man` prints a roff man page: `pinboard-sync man | man -l -`.

## Running as a NixOS service

The flake exports `nixosModules.pinboard-sync` (alias `nixosModules.default`). It
renders `settings` to a TOML file in the Nix store, so keep secrets out of it, and runs
each timer as a hardened `DynamicUser`:

```nix
{
  imports = [ inputs.pinboard-sync.nixosModules.default ];

  # A sops-nix template that renders every credential into a root-only env file.
  sops.templates."pinboard-sync.env".content = ''
    PINBOARD_TOKEN=${config.sops.placeholder.pinboard-token}
    REDDIT_USERNAME=${config.sops.placeholder.reddit-username}
    REDDIT_COOKIE=${config.sops.placeholder.reddit-cookie}
  '';

  services.pinboard-sync = {
    enable = true;
    settings.reddit = [
      { name = "main"; }
      { enable = false; name = "alt"; }        # configured but skipped
    ];
    environmentFile = config.sops.templates."pinboard-sync.env".path;
    # sync.schedule = "*:0/30";               # sync timer; default every 30 minutes
    cleanup.enable = true;                   # cleanup --all on its own timer…
    # cleanup.schedule = "weekly";           # …default weekly
    backup.enable = true;                    # backup --all on its own timer…
    # backup.schedule = "daily";             # …default daily
    # backup.directory = "/var/lib/pinboard-sync/backup";  # default location
  };
}
```

An account with `enable = false` stays configured but is left out of the runs.
`backup.directory` must stay under the `StateDirectory` (`/var/lib/pinboard-sync`), the
only place the `DynamicUser` can write. The snapshot is readable by root only, since the
state directory is under the 0700 `/var/lib/private`, so fetch it with a root job.

Instead of one `environmentFile`, you can point a per-credential option at each secret:
`pinboardTokenFile`, `redditUsernameFile`, `redditCookieFile`, `githubTokenFile` and
`hnUsernameFile`. Each is loaded with systemd `LoadCredential` and read through its
`_FILE` variable, so the value stays out of the unit environment and the store, and the
`DynamicUser` can read it even when the source file is root-only:

```nix
services.pinboard-sync = {
  enable = true;
  pinboardTokenFile = config.sops.secrets.pinboard-token.path;
  redditUsernameFile = config.sops.secrets.reddit-username.path;
  redditCookieFile = config.sops.secrets.reddit-cookie.path;  # bare or reddit_session=…
};
```

The two can be combined, and credentials can also be `*_file` paths in `settings`. The
binary opens those itself as the `DynamicUser`, so each must be readable by it (for
example sops-nix `mode = "0444"`); a root-only `0400` secret fails every run. The module
requires a Pinboard token from one of these.

`onAuthFailure` sets the [auth-failure hook](#auth-failure-hook) for every unit. The
hook runs inside the unit's sandbox: its `PATH` has only the service defaults, so call
other programs by store path (`${lib.getExe pkgs.curl}`), and only IPv4/IPv6 sockets
work, so `systemctl`, `logger` and desktop notifiers fail. A host without `/bin/sh`
needs a package providing `bin/sh` on each unit's `path`.

## Troubleshooting

- **Start with `doctor`**, which checks the Pinboard token and every account in the
  config, and `config check` for the config. Add `-v` to see response bodies.
- **Reddit cookie expired**: the run exits non-zero and the hook fires. Copy a fresh
  `reddit_session` (see [Authentication](#authentication)).
- **"blocked by network security"**: Reddit's edge refuses some requests even with a
  valid cookie ([#78](https://github.com/thanegill/pinboard-sync/issues/78)). No hook
  fires, since a new cookie won't help. When it blocks `/api/info`
  ([#89](https://github.com/thanegill/pinboard-sync/issues/89)), `cleanup reddit` reads
  the account's saved listing instead, which needs its `username` (or
  `$REDDIT_USERNAME`). A post in the listing gets every fix; a comment gets its tags
  when it is listed, and its NSFW and title fixes when its thread is. Other bookmarks
  get only URL and tag normalization. The run exits 0 with one warning saying how many
  bookmarks took each path. It fails if the listing can't be read or no username is set,
  and a refused cookie still fires the hook.
- **GitHub SSO or blocked-repo warnings**: the repo gets only its URL fix. For SSO,
  authorize the token for that organization.
- **A refused cleanup rewrite** leaves the bookmark and its duplicate as they are, for
  you to resolve by hand. A bookmark refused because its source couldn't be read is
  retried on the next run.

## Roadmap

- **Tag bundles**, which need Pinboard's API v2 (a 2021 draft, not yet deployed).
- **Prune mode**, opt-in and dry-run first, removing bookmarks a source no longer has.
- **More sources**: RSS/Atom, Mastodon bookmarks, Lobsters.
- **A `--format json` run summary** and a general post-run hook.

## Development

Run everything through the Nix dev shell so versions match the flake:

```sh
nix develop --command cargo test
nix develop --command cargo clippy --all-targets -- -D warnings
nix develop --command cargo fmt --check
nix flake check        # builds the package (runs the hermetic tests) + evaluates the module and config-checks its TOML + lints the Nix files
nix fmt                # formats the Nix files
```

CI also runs a `lint` job; [CLAUDE.md](CLAUDE.md) lists its commands and describes the
architecture.
