//! The `backup` driver: one snapshot directory holding, for every service, the API
//! responses with **every field** intact (`raw/`) and the same items as domain bookmarks
//! (`normalized/`).
//!
//! "Every field", not "every byte": merging the pages of one service into a single file
//! means re-serializing, and `serde_json` here has no `preserve_order`, so object keys
//! come out sorted. Nothing is dropped — but the file is not the server's bytes, and the
//! docs must not claim it is.
//!
//! Three stages, deliberately separated so only the last touches the filesystem:
//! a client [`dump`](BackupSource::dump)s (async), [`layout`] turns that payload into
//! named files (pure), and [`write_files`] puts them on disk. The clients therefore
//! know how to produce their data and nothing about where it goes — the same split
//! `cleanup_pass::run_pass` makes for `cleanup`.
//!
//! Raw fidelity matters because every wire struct in this crate deserializes only the
//! fields it needs, with no catch-all: `GitHubStarredRepo` keeps a handful of GitHub's
//! ~80 repo fields, and `HackerNewsItem` is already a normalized view of an Algolia hit.
//! So `raw/` is captured as response *text*, before any parsing, by a [`RawSink`] the
//! clients push to as they walk their existing pagination — one traversal feeds both
//! halves, and the two can never disagree about a point in time.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use log::warn;
use serde::Serialize;
use serde_json::Value;

use crate::bookmark::Bookmark;
use crate::source::{BookmarkDraft, SourceError};

/// The media kind of a captured body, which picks the backup file's extension and the
/// sanity check applied before it may replace a good snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawKind {
    Json,
    Html,
}

impl RawKind {
    fn extension(self) -> &'static str {
        match self {
            RawKind::Json => "json",
            RawKind::Html => "html",
        }
    }
}

/// One verbatim response body, tagged with the stream it belongs to (which becomes part
/// of the filename, so a service with two streams gets two files) and its media kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawPage {
    pub stream: &'static str,
    pub kind: RawKind,
    pub body: String,
}

/// Collects response bodies as a client walks its pagination, so `backup` gets the raw
/// responses and the normalized items out of a single traversal.
///
/// `sync` and `cleanup` pass a disabled sink: [`push`](Self::push) is then one branch
/// that retains nothing, leaving those paths doing exactly the work they did before.
#[derive(Debug, Default)]
pub struct RawSink {
    enabled: bool,
    pages: Vec<RawPage>,
    truncated: Option<String>,
}

impl RawSink {
    /// A sink that discards everything — for the `sync`/`cleanup` read paths.
    pub fn disabled() -> Self {
        Self::default()
    }

    /// A sink that retains every body, in the order the client pushes them.
    pub fn collecting() -> Self {
        Self {
            enabled: true,
            pages: Vec::new(),
            truncated: None,
        }
    }

    /// Retain `body` under `stream`, if collecting.
    pub fn push(&mut self, stream: &'static str, kind: RawKind, body: &str) {
        if self.enabled {
            self.pages.push(RawPage {
                stream,
                kind,
                body: body.to_string(),
            });
        }
    }

    /// Record that the walk stopped early, so what was captured is *not* the whole
    /// account. For `sync` a page cap is a warning; for a backup it means a truncated
    /// snapshot would replace a complete one, which the driver must refuse to do quietly.
    pub fn mark_truncated(&mut self, reason: impl Into<String>) {
        if self.enabled && self.truncated.is_none() {
            self.truncated = Some(reason.into());
        }
    }

    pub fn into_parts(self) -> (Vec<RawPage>, Option<String>) {
        (self.pages, self.truncated)
    }
}

/// One normalized bookmark as written to `normalized/*.json`.
///
/// Deliberately separate from the domain [`Bookmark`]: this is the backup's own output
/// contract, and renaming a domain field must not silently change the shape of an
/// operator's snapshots. Plain `String`s also mean `url` and `time` need no serde
/// features in the shipped binary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExportBookmark {
    pub url: String,
    pub title: String,
    pub note: String,
    pub tags: Vec<String>,
    /// RFC 3339, omitted when the bookmark carries no creation time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    pub public: bool,
    pub read_later: bool,
    /// The key sync dedups on. Absent for the Pinboard dump, which has no source.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dedup_key: Option<String>,
}

impl From<&Bookmark> for ExportBookmark {
    fn from(b: &Bookmark) -> Self {
        Self {
            url: b.url.to_string(),
            title: b.title.clone(),
            note: b.note.clone(),
            tags: b.tags.clone(),
            timestamp: b.timestamp.and_then(crate::timefmt::to_rfc3339),
            public: b.public,
            read_later: b.read_later,
            dedup_key: None,
        }
    }
}

impl From<&BookmarkDraft> for ExportBookmark {
    fn from(d: &BookmarkDraft) -> Self {
        Self {
            dedup_key: Some(d.dedup_key.clone()),
            ..Self::from(&d.bookmark)
        }
    }
}

/// One account's payload before it is laid out on disk: the verbatim bodies in fetch
/// order, and the normalized records built from that same traversal.
pub struct BackupDump {
    pub raw: Vec<RawPage>,
    /// `Err` when the source's shape broke so no bookmark could be built. The raw half is
    /// still written — a schema break is exactly when the verbatim body is worth having —
    /// the normalized half is skipped rather than emptied, and the job is reported failed.
    pub records: Result<Vec<ExportBookmark>>,
    /// Set when pagination stopped at a page cap, so this dump is only part of the
    /// account and must not pass as a complete snapshot.
    pub truncated: Option<String>,
}

/// A service that can dump what it holds. Implemented by the three `Source` clients and
/// by the Pinboard client, so `backup` covers the destination as well as the sources.
/// (Crate-internal, never spawned across threads, so the missing `Send` bound from
/// `async fn` in a trait is irrelevant here.)
#[allow(async_fn_in_trait)]
pub trait BackupSource {
    async fn dump(&self) -> Result<BackupDump, SourceError>;
}

/// How many things a file holds, for the manifest and the dry-run line. The two are
/// deliberately distinct: an envelope stream's element count is a page count, not an
/// item count, and conflating them makes a manifest diff useless for spotting data loss.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Count {
    Items(usize),
    Pages(usize),
}

impl Count {
    /// `(label, value)` for the manifest field and the dry-run line.
    fn parts(self) -> (&'static str, usize) {
        match self {
            Count::Items(n) => ("items", n),
            Count::Pages(n) => ("pages", n),
        }
    }
}

/// One file to write, with its path relative to the snapshot directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupFile {
    pub path: String,
    pub kind: RawKind,
    pub body: String,
    pub count: Count,
}

/// A filename-safe slug for an account name: lowercased, with every run of characters
/// outside `[a-z0-9_-]` — `.` included, so `a.b` and `a/b` slug alike and
/// [`check_stem_collisions`] has to catch the clash — collapsed to a single `-` and the
/// ends trimmed. An unnamed
/// account (or one that slugs to nothing) becomes `default`, matching `job_label`'s
/// `source[default]`. Because `/` never survives and a bare `..` cannot be produced, a
/// config-supplied name can't escape the snapshot directory.
pub fn slug(name: Option<&str>) -> String {
    let mut out = String::new();
    for ch in name.unwrap_or("").chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "default".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Merge one stream's captured JSON bodies into a single value: a body that is itself a
/// JSON **array** is flattened into the output (GitHub's `/user/starred`, Pinboard's
/// `posts/all`), any other body is pushed as one element (Reddit's `{"kind":"Listing"}`
/// envelope, Algolia's `{"hits":[…]}`).
///
/// So GitHub and Pinboard produce a flat array of items while Reddit and HackerNews
/// produce an array of page envelopes. Always pushing envelopes would make GitHub an
/// array-of-arrays, which is worse to consume; the asymmetry is documented in the README.
///
/// **Every field survives, but the bytes are not the server's.** Merging means
/// re-serializing, and `serde_json` is built without `preserve_order`, so object keys come
/// back out sorted. Two JSON documents with the same fields, not the same file — say
/// "every field preserved", never "byte-for-byte".
///
/// Returns the [`Count`] alongside, since the classification (were all the bodies arrays?)
/// falls out of the same walk — recomputing it would mean parsing every body twice, and
/// Pinboard's `meta=yes` export is large enough for that to matter.
pub fn merge_json_pages(bodies: &[&str]) -> Result<(Value, Count)> {
    let mut merged = Vec::new();
    let mut all_arrays = true;
    for (i, body) in bodies.iter().enumerate() {
        let value: Value = serde_json::from_str(body)
            .with_context(|| format!("parsing captured response page {}", i + 1))?;
        match value {
            Value::Array(items) => merged.extend(items),
            other => {
                all_arrays = false;
                merged.push(other);
            }
        }
    }
    let count = if all_arrays {
        Count::Items(merged.len())
    } else {
        Count::Pages(merged.len())
    };
    Ok((Value::Array(merged), count))
}

/// Concatenate one stream's captured HTML pages in fetch order, each behind a marker
/// comment.
///
/// One file per stream rather than one per page, because the page count varies between
/// runs: with a snapshot directory overwritten in place and never pruned, a
/// `-page-03.html` left by last week's run would survive a two-page run and silently
/// corrupt the snapshot. Keeping the file set a pure function of the config is what
/// makes overwrite-without-pruning safe. HTML comments are inert, so each page's bytes
/// are still verbatim.
pub fn merge_html_pages(bodies: &[&str]) -> String {
    let mut out = String::new();
    for (i, body) in bodies.iter().enumerate() {
        out.push_str(&format!("<!-- pinboard-sync page {} -->\n", i + 1));
        out.push_str(body);
        if !body.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// Lay one account's payload out as named files, relative to the snapshot directory.
/// Pure: no filesystem access, so the naming and merging rules are unit-testable on
/// their own.
///
/// The account's primary JSON stream lands at `<source>-<slug>`; any other stream adds
/// its own suffix. `stem` is `pinboard` for the destination, which has no accounts.
pub fn layout(stem: &str, dump: &BackupDump) -> Result<Vec<BackupFile>> {
    let mut files = Vec::new();

    // Group by stream, preserving the order each stream was first pushed in. The primary
    // JSON stream ("") is always emitted, even with no pages: a source that legitimately
    // returns nothing this run (an emptied favorites list makes zero Algolia requests)
    // must still overwrite its file with `[]`, or last run's data would survive next to a
    // freshly emptied normalized half — the exact disagreement the pairing rules out.
    let mut streams: Vec<&'static str> = vec![""];
    for page in &dump.raw {
        if !streams.contains(&page.stream) {
            streams.push(page.stream);
        }
    }

    for stream in streams {
        let pages: Vec<&RawPage> = dump.raw.iter().filter(|p| p.stream == stream).collect();
        let kind = pages.first().map_or(RawKind::Json, |p| p.kind);
        let bodies: Vec<&str> = pages.iter().map(|p| p.body.as_str()).collect();
        // The primary stream carries the bare stem; a second stream is suffixed.
        let name = if stream.is_empty() {
            stem.to_string()
        } else {
            format!("{stem}-{stream}")
        };
        let (body, count) = match kind {
            RawKind::Json => {
                let (merged, count) = merge_json_pages(&bodies)
                    .with_context(|| format!("merging {stream} pages for {stem}"))?;
                (serde_json::to_string_pretty(&merged)?, count)
            }
            RawKind::Html => (merge_html_pages(&bodies), Count::Pages(bodies.len())),
        };
        files.push(BackupFile {
            path: format!("raw/{name}.{}", kind.extension()),
            kind,
            body,
            count,
        });
    }

    // Skipped, not emptied, when normalization failed — see `BackupDump::records`.
    if let Ok(records) = &dump.records {
        files.push(BackupFile {
            path: normalized_path(stem),
            kind: RawKind::Json,
            body: serde_json::to_string_pretty(records)
                .with_context(|| format!("serializing normalized {stem} bookmarks"))?,
            count: Count::Items(records.len()),
        });
    }

    Ok(files)
}

/// Reject a body that must not be allowed to replace a good snapshot. `posts/all` and
/// the source listings all return arrays, and a 2xx interstitial, proxy page, or a
/// connection dropped mid-array can pass a status check while producing nothing usable.
fn check_writable(file: &BackupFile) -> Result<()> {
    match file.kind {
        RawKind::Json => {
            if serde_json::from_str::<Vec<Value>>(&file.body).is_err() {
                bail!(
                    "{} is not a JSON array ({} bytes); refusing to overwrite the previous snapshot",
                    file.path,
                    file.body.len()
                );
            }
        }
        RawKind::Html => {
            if file.body.trim().is_empty() {
                bail!(
                    "{} is empty; refusing to overwrite the previous snapshot",
                    file.path
                );
            }
        }
    }
    Ok(())
}

/// Bail if two jobs would write the same files. `config::check_unique_names` guards
/// account *names*, not slugs — `a.b` and `a/b` both slug to `a-b`, and the second job
/// would silently overwrite the first. Checked before any write, not during.
pub fn check_stem_collisions(stems: &[String]) -> Result<()> {
    let mut seen = HashSet::new();
    for stem in stems {
        if !seen.insert(stem) {
            bail!(
                "two backup targets both write '{stem}' — rename one account so their \
                 backup filenames differ"
            );
        }
    }
    Ok(())
}

/// One written file, recorded in the manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct ManifestEntry {
    pub path: String,
    /// Either `items` or `pages` — see [`Count`]; never both, so a manifest never implies
    /// an item count it doesn't have.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pages: Option<usize>,
    pub bytes: usize,
    /// When *this file* was written, which is not the run's timestamp once a narrowed
    /// run (`backup pinboard`) has refreshed only part of the directory.
    #[serde(default)]
    pub generated_at: String,
    /// Why this file isn't a trustworthy snapshot, when it isn't. Carried on the entry
    /// rather than only in the run's `failed` list, because a later clean run of a
    /// *different* target rewrites `failed` and would otherwise erase the only evidence
    /// that this file is partial. `generated_at` answers "when", not "trustworthy".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unusable: Option<String>,
}

impl From<&BackupFile> for ManifestEntry {
    fn from(f: &BackupFile) -> Self {
        let (items, pages) = match f.count {
            Count::Items(n) => (Some(n), None),
            Count::Pages(n) => (None, Some(n)),
        };
        Self {
            path: f.path.clone(),
            items,
            pages,
            bytes: f.body.len(),
            generated_at: String::new(),
            unusable: None,
        }
    }
}

/// Fail before the fetch if the snapshot directory's parent is missing, so a bad path
/// doesn't waste a rate-limited run. The `raw/`/`normalized/` subdirectories are created
/// by the writer.
pub fn check_backup_dir(dir: &Path) -> Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    match dir.parent() {
        Some(parent) if parent.as_os_str().is_empty() || parent.is_dir() => Ok(()),
        Some(parent) => bail!(
            "backup directory {} cannot be created: {} does not exist",
            dir.display(),
            parent.display()
        ),
        None => Ok(()),
    }
}

/// Whether a snapshot could actually be written to `dir`, as a `doctor` check. Probes by
/// creating and removing a file rather than reading permission bits, which get
/// `DynamicUser`, ACLs and read-only mounts wrong.
///
/// Deliberately creates nothing. A probe that ran `mkdir -p` would report ✓ for a typo'd
/// path — it would simply create it — and, run as root against the NixOS default, would
/// leave a real root-owned `/var/lib/pinboard-sync` where systemd expects to manage a
/// symlink into `/var/lib/private`, breaking the very setup the check exists to validate.
pub fn probe_writable(dir: &Path) -> Result<DirProbe> {
    if !dir.is_dir() {
        // Not yet created is not a fault: `backup` makes the directory on its first run.
        // Only the parent has to be usable now, which is what catches a typo'd path.
        check_backup_dir(dir)?;
        return Ok(DirProbe::WillBeCreated);
    }
    let (tmp, _file) = create_backup_tmp(dir, "doctor")?;
    std::fs::remove_file(&tmp)
        .with_context(|| format!("removing the probe file {}", tmp.display()))?;
    Ok(DirProbe::Writable)
}

/// The outcome of [`probe_writable`]. A directory that doesn't exist yet is reported
/// separately rather than as an error: `doctor` shouldn't fail a healthy setup just
/// because `backup` hasn't run yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirProbe {
    Writable,
    WillBeCreated,
}

/// What one target actually did: the files it wrote, the files it deleted, and — when the
/// target wrote something it can't vouch for — why.
///
/// `written`/`removed` are reported even when `unusable` is set, because the manifest
/// merges over the previous run: leaving them out would let the *old* entry survive and
/// describe files this run just replaced, which is worse than no entry at all.
#[derive(Debug, Default)]
pub struct JobOutcome {
    pub written: Vec<ManifestEntry>,
    /// Paths (relative) whose files this run deleted, so their manifest entries go too.
    pub removed: Vec<String>,
    /// Set when the files landed but are not a trustworthy snapshot of the target: a
    /// truncated fetch, a shape break, or a write that failed partway. Recorded on each
    /// entry too, so the flag survives a later run of a *different* target.
    pub unusable: Option<String>,
}

impl JobOutcome {
    /// Add a reason the snapshot can't be trusted, keeping any already recorded — two
    /// problems describe different damage, and dropping one costs the diagnosis.
    fn note_unusable(&mut self, reason: String) {
        self.unusable = Some(match self.unusable.take() {
            Some(prior) => format!("{prior}; {reason}"),
            None => reason,
        });
    }
}

/// Back up one target: dump it, lay the payload out, and write it under `dir` — or, under
/// `dry_run`, render what would be written without touching the filesystem.
///
/// `Err` means nothing was written (the fetch, layout, or a pre-write check failed).
/// A target that wrote files it can't vouch for returns `Ok` with `unusable` set, so the
/// caller can record both what landed and that it isn't good.
pub async fn run_job<S: BackupSource>(
    source: &S,
    stem: &str,
    dir: &Path,
    dry_run: bool,
) -> Result<JobOutcome, SourceError> {
    let dump = source.dump().await?;
    let files = layout(stem, &dump)?;
    // Checked in both modes, so a dry run can't report a plan the real run would refuse.
    for file in &files {
        check_writable(file)?;
    }

    // Decided before the dry-run branch, for the same reason: a dry run that reported a
    // clean plan for a target the real run flags would be worse than useless — it is what
    // an operator validates a new config with. A schema break is exactly when the verbatim
    // body is worth keeping, and a truncated snapshot still beats none, so neither is an
    // `Err`; they ride back so the caller records what landed *and* that it isn't good.
    let mut unusable: Vec<String> = Vec::new();
    if let Err(e) = &dump.records {
        unusable.push(format!("{e:#}"));
    }
    if let Some(reason) = &dump.truncated {
        // A page cap is a warning for `sync`, which is additive and idempotent. For a
        // backup it means a partial snapshot just replaced a complete one.
        unusable.push(format!(
            "{reason} — the snapshot for this target is incomplete"
        ));
    }
    let mut outcome = JobOutcome {
        written: files.iter().map(ManifestEntry::from).collect(),
        // Both reasons, not the first: they describe different damage, and losing one
        // costs the operator the diagnosis.
        unusable: (!unusable.is_empty()).then(|| unusable.join("; ")),
        ..JobOutcome::default()
    };

    if dry_run {
        for file in &files {
            println!("[dry-run] {}", file_path(dir, file).display());
            let (label, value) = file.count.parts();
            println!("          {label:<6}-> {value}");
            println!("          {:<6}-> {}", "bytes", file.body.len());
        }
        return Ok(outcome);
    }

    // `written` is replaced by what the writer actually got onto disk: a failure partway
    // through still replaced the earlier files, and dropping their entries would let the
    // merged manifest keep the *previous* run's entry describing what is no longer there.
    let mut written = Vec::new();
    let write_result = write_files(dir, &files, &mut written);
    outcome.written = written;
    if let Err(e) = write_result {
        outcome.note_unusable(format!("{e:#}"));
        return Ok(outcome);
    }

    // A normalized half this run couldn't produce must not be left as last run's file
    // beside a freshly written raw half — the two would silently disagree. Absent says
    // "not established", which a stale file cannot.
    if dump.records.is_err() {
        let relative = normalized_path(stem);
        let stale = dir.join(&relative);
        if stale.exists() {
            // Not `?`: this is past the write, so failing here would discard the entries
            // for files already on disk and leave the merged manifest describing the
            // previous run's versions of them. Every post-write problem rides back on the
            // outcome, which is what keeps "`Err` means nothing was written" true.
            if let Err(e) = std::fs::remove_file(&stale) {
                outcome.note_unusable(format!("removing stale {}: {e}", stale.display()));
                return Ok(outcome);
            }
        }
        // Recorded even when the file was already absent: the manifest may still carry an
        // entry for it from an earlier run, and that entry has to go either way.
        outcome.removed.push(relative);
    }

    Ok(outcome)
}

/// The normalized half's path for `stem`, relative to the snapshot directory. Shared so
/// [`layout`] and the stale-file removal in [`run_job`] cannot name it differently.
fn normalized_path(stem: &str) -> String {
    format!("normalized/{stem}.json")
}

/// Write the run's manifest last, so a reader can tell a complete snapshot from one left
/// half-written by a failed run.
///
/// Per-file atomicity can't give run atomicity: a run whose third target fails leaves a
/// directory mixing this run's files with the last run's. `failed` names those targets and
/// `complete` is false, so a stale file can't hide behind a fresh `generated_at` — without
/// them a reader would see today's timestamp over week-old data and believe it.
///
/// This is a read-modify-write with no locking, so two runs writing the *same* directory
/// concurrently lose each other's entries. That is not new — they would already be racing
/// on the data files themselves, and a snapshot directory is single-writer by design — but
/// the merge is what makes `manifest.json` shared state, so it is worth stating.
pub fn write_manifest(
    dir: &Path,
    entries: &[ManifestEntry],
    removed: &[String],
    failed: &[String],
    generated_at: &str,
) -> Result<()> {
    #[derive(Serialize, serde::Deserialize)]
    struct Manifest {
        generated_at: String,
        version: String,
        /// Whether the run that wrote this manifest finished every target it attempted.
        /// It says nothing about targets the run didn't attempt — for that, compare each
        /// entry's own `generated_at`.
        complete: bool,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        failed: Vec<String>,
        files: Vec<ManifestEntry>,
    }

    // Merge over the previous manifest rather than replacing it. A narrowed run
    // (`backup pinboard`) refreshes part of the directory; replacing the manifest would
    // leave the other targets' files present but undescribed — the stale-file-behind-a-
    // fresh-timestamp problem the manifest exists to prevent. Each entry carries its own
    // `generated_at`, so a file older than the run is visible as such.
    let path = dir.join("manifest.json");
    let mut files: Vec<ManifestEntry> = match std::fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str::<Manifest>(&text) {
            Ok(m) => m.files,
            // Never silent: discarding the previous entries here would quietly undo the
            // merge and let a narrowed run write a manifest describing only its own
            // target — the very failure merging exists to prevent.
            Err(e) => {
                warn!(
                    "{} exists but could not be parsed ({e}); its entries are being \
                     dropped and the manifest rebuilt from this run only",
                    path.display()
                );
                Vec::new()
            }
        },
        Err(_) => Vec::new(),
    };
    files.retain(|f| !removed.contains(&f.path));
    for entry in entries {
        let mut entry = entry.clone();
        entry.generated_at = generated_at.to_string();
        match files.iter_mut().find(|f| f.path == entry.path) {
            Some(existing) => *existing = entry,
            None => files.push(entry),
        }
    }
    // Drop entries whose file is gone: an account renamed or dropped from the config would
    // otherwise be described forever, with an ever-older `generated_at`. `try_exists`, not
    // `exists`: the latter reports `false` for a permission or I/O error too, so one
    // unreadable moment would permanently prune entries for files that are still there.
    files.retain(|f| dir.join(&f.path).try_exists().unwrap_or(true));
    files.sort_by(|a, b| a.path.cmp(&b.path));

    let body = serde_json::to_string_pretty(&Manifest {
        generated_at: generated_at.to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        complete: failed.is_empty(),
        failed: failed.to_vec(),
        files,
    })
    .context("serializing backup manifest")?;
    // On a first run where every target failed, nothing else has created `dir` yet;
    // without this the manifest write fails with ENOENT and hides the real failures.
    create_private_dir(dir)?;
    write_atomically(&path, &body)
}

/// The absolute path each file is written to.
pub fn file_path(dir: &Path, file: &BackupFile) -> PathBuf {
    dir.join(&file.path)
}

/// Write every file under `dir`, each atomically. Every file is checked before anything
/// is written, so a bad body in the last target can't leave the snapshot half-replaced.
pub fn write_files(
    dir: &Path,
    files: &[BackupFile],
    written: &mut Vec<ManifestEntry>,
) -> Result<()> {
    for file in files {
        check_writable(file)?;
    }
    // Each success is pushed as it happens, so a failure partway through still tells the
    // caller which files were already replaced. Without that the manifest would keep the
    // previous run's entries for files that no longer match them.
    for file in files {
        let path = file_path(dir, file);
        let parent = path
            .parent()
            .with_context(|| format!("{} has no parent directory", path.display()))?;
        create_private_dir(parent)?;
        write_atomically(&path, &file.body)?;
        written.push(ManifestEntry::from(file));
    }
    Ok(())
}

/// Create `dir` (and its parents) mode 0700. A snapshot holds every private bookmark, so
/// the directory is no more readable than the files in it.
fn create_private_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    if dir.is_dir() {
        return Ok(());
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))
}

/// Atomically replace `path` with `body`: write a private, fsync'd temp file next to it
/// and rename it over the target. A partial or crashed write leaves the previous snapshot
/// intact rather than truncating it. Snapshots hold every private bookmark, so the file
/// is created mode 0600.
fn write_atomically(path: &Path, body: &str) -> Result<()> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("pinboard-backup");
    let (tmp, mut file) = create_backup_tmp(&dir, file_name)?;
    write_backup_tmp(&tmp, &mut file, body)
        .and_then(|()| {
            std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
            sync_dir(&dir);
            Ok(())
        })
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
}

/// Open `tmp` as a brand-new mode-0600 regular file. `create_new` (O_EXCL) never follows a
/// pre-existing symlink and never reuses an existing file's contents or permissions, so the
/// 0600 promise holds; a path that already exists errors with `AlreadyExists` instead.
fn open_new_private(tmp: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(tmp)
}

/// Create a fresh, private temp file next to the backup target and return it with its path.
/// Each attempt mixes fresh entropy into the name, so a leftover temp from a crashed run
/// (even one with this pid) can't be reused; `open_new_private` guarantees the file is new.
fn create_backup_tmp(dir: &Path, file_name: &str) -> Result<(PathBuf, std::fs::File)> {
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let pid = std::process::id();

    create_new_temp(dir, || {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let nonce = nanos ^ u128::from(COUNTER.fetch_add(1, Ordering::Relaxed));
        format!(".{file_name}.tmp.{pid}.{nonce:x}")
    })
}

/// Open a fresh private temp file in `dir`, drawing candidate names from `next_name` and
/// retrying whenever the chosen path already exists (a leftover temp, a squatter). Bounded
/// so a name generator that keeps yielding a colliding name can't spin forever.
fn create_new_temp(
    dir: &Path,
    mut next_name: impl FnMut() -> String,
) -> Result<(PathBuf, std::fs::File)> {
    let mut last_err = None;
    for _ in 0..100 {
        let tmp = dir.join(next_name());
        match open_new_private(&tmp) {
            Ok(file) => return Ok((tmp, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last_err = Some(e),
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("creating backup temp file in {}", dir.display()));
            }
        }
    }
    Err(last_err.expect("loop ran at least once"))
        .with_context(|| format!("creating a unique backup temp file in {}", dir.display()))
}

/// Write `body` to the already-opened temp `file` and fsync it, so the bytes are on disk
/// before the caller renames it over the real target.
fn write_backup_tmp(tmp: &Path, file: &mut std::fs::File, body: &str) -> Result<()> {
    use std::io::Write;

    file.write_all(body.as_bytes())
        .with_context(|| format!("writing backup to {}", tmp.display()))?;
    file.sync_all()
        .with_context(|| format!("flushing backup to {}", tmp.display()))
}

/// Best-effort fsync of the directory so the rename itself survives a crash.
fn sync_dir(dir: &Path) {
    if let Ok(handle) = std::fs::File::open(dir) {
        let _ = handle.sync_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::OffsetDateTime;
    use url::Url;

    fn page(stream: &'static str, kind: RawKind, body: &str) -> RawPage {
        RawPage {
            stream,
            kind,
            body: body.to_string(),
        }
    }

    fn bookmark() -> Bookmark {
        Bookmark {
            url: Url::parse("https://example.com/a").unwrap(),
            title: "Title".into(),
            note: "Note".into(),
            tags: vec!["reddit".into()],
            timestamp: OffsetDateTime::from_unix_timestamp(1_600_000_000).ok(),
            public: false,
            read_later: true,
        }
    }

    #[test]
    fn disabled_sink_retains_nothing() {
        let mut sink = RawSink::disabled();
        sink.push("saved", RawKind::Json, "[]");
        assert!(sink.into_parts().0.is_empty());
    }

    #[test]
    fn collecting_sink_preserves_order_and_stream() {
        let mut sink = RawSink::collecting();
        sink.push("saved", RawKind::Json, "[1]");
        sink.push("saved", RawKind::Json, "[2]");
        assert_eq!(
            sink.into_parts().0,
            vec![
                page("saved", RawKind::Json, "[1]"),
                page("saved", RawKind::Json, "[2]"),
            ]
        );
    }

    #[test]
    fn merging_preserves_every_field_but_reorders_keys() {
        // `serde_json` is built without `preserve_order`, so a `Value::Object` is a
        // BTreeMap and merging re-serializes with keys sorted. Pinned here because the
        // docs promise *fields*, not bytes — see `merge_json_pages`.
        let (merged, _) = merge_json_pages(&[r#"[{"zeta":1,"alpha":2,"href":"x"}]"#]).unwrap();
        assert_eq!(
            serde_json::to_string(&merged).unwrap(),
            r#"[{"alpha":2,"href":"x","zeta":1}]"#
        );
        assert_eq!(merged[0]["zeta"], 1, "no field is lost");
    }

    #[test]
    fn merge_flattens_arrays_and_keeps_envelopes_whole() {
        let (flat, count) = merge_json_pages(&["[1,2]", "[3]"]).unwrap();
        assert_eq!(flat, serde_json::json!([1, 2, 3]));
        assert_eq!(count, Count::Items(3), "array bodies flatten into items");

        let (enveloped, count) = merge_json_pages(&[r#"{"a":1}"#, r#"{"a":2}"#]).unwrap();
        assert_eq!(enveloped, serde_json::json!([{"a":1},{"a":2}]));
        assert_eq!(
            count,
            Count::Pages(2),
            "envelopes stay one element per page"
        );
    }

    #[test]
    fn merge_reports_which_page_failed_to_parse() {
        let err = merge_json_pages(&["[]", "<html>nope</html>"]).unwrap_err();
        assert!(
            format!("{err:#}").contains("page 2"),
            "error should name the failing page: {err:#}"
        );
    }

    #[test]
    fn merged_html_keeps_every_page_verbatim_behind_a_marker() {
        let merged = merge_html_pages(&["<a>one</a>", "<a>two</a>"]);
        assert!(merged.contains("<!-- pinboard-sync page 1 -->"));
        assert!(merged.contains("<!-- pinboard-sync page 2 -->"));
        assert!(merged.contains("<a>one</a>"));
        assert!(merged.contains("<a>two</a>"));
    }

    #[test]
    fn slugs_are_filename_safe_and_default_when_absent() {
        assert_eq!(slug(Some("Main Account")), "main-account");
        assert_eq!(slug(Some("MAIN")), "main");
        assert_eq!(slug(None), "default");
        assert_eq!(slug(Some("")), "default");
        assert_eq!(slug(Some("///")), "default");
        // A traversal attempt cannot survive: no `/` and no bare `..`.
        let escaped = slug(Some("../../etc/passwd"));
        assert!(!escaped.contains('/'), "{escaped}");
        assert_eq!(escaped, "etc-passwd");
        // The documented collision: distinct names, one slug.
        assert_eq!(slug(Some("a.b")), slug(Some("a/b")));
    }

    #[test]
    fn collisions_are_caught_before_any_write() {
        let ok = vec!["reddit-a".to_string(), "reddit-b".to_string()];
        assert!(check_stem_collisions(&ok).is_ok());

        let clash = vec!["reddit-a-b".to_string(), "reddit-a-b".to_string()];
        let err = check_stem_collisions(&clash).unwrap_err();
        assert!(format!("{err:#}").contains("reddit-a-b"), "{err:#}");
    }

    #[test]
    fn export_bookmark_is_the_on_disk_contract() {
        let exported = ExportBookmark::from(&bookmark());
        let json = serde_json::to_value(&exported).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "url": "https://example.com/a",
                "title": "Title",
                "note": "Note",
                "tags": ["reddit"],
                "timestamp": "2020-09-13T12:26:40Z",
                "public": false,
                "read_later": true,
            })
        );
    }

    #[test]
    fn export_omits_absent_timestamp_and_carries_a_draft_dedup_key() {
        let mut b = bookmark();
        b.timestamp = None;
        let json = serde_json::to_value(ExportBookmark::from(&b)).unwrap();
        assert!(
            json.get("timestamp").is_none(),
            "an absent timestamp is omitted, not null: {json}"
        );
        assert!(json.get("dedup_key").is_none());

        let draft = BookmarkDraft {
            bookmark: bookmark(),
            dedup_key: "reddit:/r/x/".into(),
        };
        let json = serde_json::to_value(ExportBookmark::from(&draft)).unwrap();
        assert_eq!(json["dedup_key"], "reddit:/r/x/");
    }

    #[test]
    fn layout_names_one_file_per_stream_plus_the_normalized_half() {
        let dump = BackupDump {
            raw: vec![
                page("", RawKind::Json, r#"{"hits":[1]}"#),
                page("", RawKind::Json, r#"{"hits":[2]}"#),
                page("favorites", RawKind::Html, "<a>one</a>"),
            ],
            records: Ok(vec![ExportBookmark::from(&bookmark())]),
            truncated: None,
        };
        let files = layout("hackernews-me", &dump).unwrap();
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                "raw/hackernews-me.json",
                "raw/hackernews-me-favorites.html",
                "normalized/hackernews-me.json",
            ]
        );
        // Both envelopes survive whole in the merged raw file.
        assert_eq!(files[0].count, Count::Pages(2), "envelopes count as pages");
        assert_eq!(files[2].count, Count::Items(1));
    }

    #[test]
    fn layout_flattens_a_source_whose_pages_are_arrays() {
        let dump = BackupDump {
            raw: vec![
                page("", RawKind::Json, r#"[{"full_name":"a/b"}]"#),
                page("", RawKind::Json, r#"[{"full_name":"c/d"}]"#),
            ],
            records: Ok(vec![]),
            truncated: None,
        };
        let files = layout("github-main", &dump).unwrap();
        assert_eq!(files[0].path, "raw/github-main.json");
        assert_eq!(
            files[0].count,
            Count::Items(2),
            "array bodies flatten, so this is an item count"
        );
    }

    #[test]
    fn a_stream_that_produced_nothing_still_overwrites_its_file() {
        // An emptied HackerNews favorites list makes zero Algolia requests. Without an
        // unconditional primary stream, `raw/*.json` would keep last run's hits next to a
        // freshly emptied `normalized/*.json` — the two halves silently disagreeing.
        let dump = BackupDump {
            raw: vec![page("favorites", RawKind::Html, "<a>none</a>")],
            records: Ok(vec![]),
            truncated: None,
        };
        let files = layout("hackernews-me", &dump).unwrap();
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert!(
            paths.contains(&"raw/hackernews-me.json"),
            "the primary stream is always written: {paths:?}"
        );
        let primary = files.iter().find(|f| f.path.ends_with("me.json")).unwrap();
        assert_eq!(primary.body, "[]");
        assert!(check_writable(primary).is_ok(), "an empty array is valid");
    }

    #[test]
    fn a_failed_normalization_keeps_the_raw_half_and_drops_the_normalized_one() {
        let dump = BackupDump {
            raw: vec![page("", RawKind::Json, "[1,2]")],
            records: Err(anyhow::anyhow!("the response shape changed")),
            truncated: None,
        };
        let files = layout("reddit-main", &dump).unwrap();
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            vec!["raw/reddit-main.json"],
            "normalized/ is skipped, not emptied — an empty file would look like real data"
        );
    }

    /// Put a real file behind a manifest entry: `write_manifest` reconciles against the
    /// directory, so an entry with no file on disk is (correctly) dropped.
    fn touch(dir: &Path, rel: &str) {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "[]").unwrap();
    }

    #[test]
    fn a_partial_run_is_marked_incomplete_in_the_manifest() {
        let dir = scratch_dir("backup-manifest");
        touch(&dir, "raw/pinboard.json");
        let entries = vec![ManifestEntry {
            path: "raw/pinboard.json".into(),
            items: Some(3),
            pages: None,
            bytes: 10,
            generated_at: String::new(),
            unusable: None,
        }];

        write_manifest(&dir, &entries, &[], &[], "2026-08-29T00:00:00Z").unwrap();
        let ok: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).unwrap())
                .unwrap();
        assert_eq!(ok["complete"], true);
        assert!(ok.get("failed").is_none());
        assert_eq!(ok["files"][0]["items"], 3);
        assert!(ok["files"][0].get("pages").is_none(), "never both counts");

        // A failed target left its previous files in place; without this the fresh
        // `generated_at` would vouch for them.
        write_manifest(
            &dir,
            &entries,
            &[],
            &["github[work]".to_string()],
            "2026-08-29T00:00:00Z",
        )
        .unwrap();
        let partial: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).unwrap())
                .unwrap();
        assert_eq!(partial["complete"], false);
        assert_eq!(partial["failed"][0], "github[work]");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A fresh, empty temp directory unique to `label`, cleaned up by the caller.
    fn scratch_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pinboard-sync-test-{label}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn json_file(path: &str, body: &str) -> BackupFile {
        BackupFile {
            path: path.into(),
            kind: RawKind::Json,
            body: body.into(),
            count: Count::Items(0),
        }
    }

    #[test]
    fn write_files_creates_the_tree_and_replaces_atomically() {
        let dir = scratch_dir("backup-write-ok");
        let target = dir.join("raw/pinboard.json");
        write_files(
            &dir,
            &[json_file("raw/pinboard.json", "[1]")],
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "[1]");

        // A second run overwrites in place and leaves no temp behind.
        write_files(
            &dir,
            &[json_file("raw/pinboard.json", "[2]")],
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "[2]");
        assert_eq!(
            std::fs::read_dir(dir.join("raw")).unwrap().count(),
            1,
            "no temp left"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn written_snapshots_are_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;

        let dir = scratch_dir("backup-write-perms");
        write_files(
            &dir,
            &[json_file("raw/pinboard.json", "[]")],
            &mut Vec::new(),
        )
        .unwrap();

        // A snapshot holds every private bookmark — file and directory alike stay private.
        let mode = |p: PathBuf| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(dir.join("raw/pinboard.json")), 0o600);
        assert_eq!(mode(dir.join("raw")), 0o700);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_bad_body_anywhere_leaves_every_previous_file_intact() {
        let dir = scratch_dir("backup-write-bad");
        write_files(
            &dir,
            &[json_file("raw/pinboard.json", "[1]")],
            &mut Vec::new(),
        )
        .unwrap();

        // A 200 that isn't a JSON array (proxy page, empty body) or a connection dropped
        // mid-array must not clobber a good snapshot — and because every file is checked
        // before any is written, a bad *second* file can't leave the first replaced.
        for bad in [
            "",
            "  ",
            "<html>Back off</html>",
            r#"[{"href":"https://x/"}"#,
        ] {
            let err = write_files(
                &dir,
                &[
                    json_file("raw/pinboard.json", "[2]"),
                    json_file("raw/reddit-main.json", bad),
                ],
                &mut Vec::new(),
            )
            .unwrap_err();
            assert!(
                format!("{err:#}").contains("refusing to overwrite"),
                "{err:#}"
            );
        }
        assert_eq!(
            std::fs::read_to_string(dir.join("raw/pinboard.json")).unwrap(),
            "[1]",
            "the good file from the previous run survives"
        );
        assert!(!dir.join("raw/reddit-main.json").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A source that hands back a fixed payload, so the driver can be exercised without
    /// any network.
    struct FakeSource(&'static str);

    impl BackupSource for FakeSource {
        async fn dump(&self) -> Result<BackupDump, SourceError> {
            Ok(BackupDump {
                raw: vec![page("", RawKind::Json, self.0)],
                records: Ok(vec![]),
                truncated: None,
            })
        }
    }

    #[tokio::test]
    async fn dry_run_writes_nothing_and_refuses_what_the_real_run_would() {
        let dir = scratch_dir("backup-dry-run");

        run_job(&FakeSource("[1]"), "pinboard", &dir, true)
            .await
            .unwrap();
        assert!(!dir.join("raw").exists(), "a dry run touches no files");

        // A dry run must not bless a plan the real run would abort on — an operator
        // validating a config behind a captive portal would otherwise see "would be
        // written" and conclude the setup is good. (A non-JSON body is rejected by the
        // merge before `check_writable` sees it; either way the run fails.)
        let err = run_job(&FakeSource("<html>portal</html>"), "pinboard", &dir, true)
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("parsing captured response page 1"),
            "{err:#}"
        );
        assert!(!dir.join("raw").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A source whose pagination stopped at a page cap.
    struct TruncatedSource;

    impl BackupSource for TruncatedSource {
        async fn dump(&self) -> Result<BackupDump, SourceError> {
            Ok(BackupDump {
                raw: vec![page("", RawKind::Json, "[1]")],
                records: Ok(vec![]),
                truncated: Some("stopped at the 100-page cap".into()),
            })
        }
    }

    #[tokio::test]
    async fn a_truncated_dump_is_written_but_reported_failed() {
        let dir = scratch_dir("backup-truncated");

        // The files are still written — a partial snapshot beats none — but the target
        // is reported failed so it lands in the manifest's `failed` list. Silently
        // replacing a complete snapshot with a truncated one is the failure mode here.
        let outcome = run_job(&TruncatedSource, "github-main", &dir, false)
            .await
            .unwrap();
        let msg = format!("{:#}", outcome.unusable.expect("reported unusable"));
        assert!(msg.contains("100-page cap"), "{msg}");
        assert!(msg.contains("incomplete"), "{msg}");
        assert!(dir.join("raw/github-main.json").exists());
        // The entries come back even though the target is unusable, so the caller can
        // record what actually landed. Dropping them would leave the merged manifest
        // describing the *previous* run's file at this same path.
        assert!(
            outcome
                .written
                .iter()
                .any(|e| e.path == "raw/github-main.json"),
            "a written-but-untrustworthy target still reports its files"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_failed_target_updates_its_manifest_entry_rather_than_keeping_the_old_one() {
        let dir = scratch_dir("backup-manifest-failed");
        touch(&dir, "raw/github-main.json");
        let big = ManifestEntry {
            path: "raw/github-main.json".into(),
            items: Some(3000),
            pages: None,
            bytes: 12_000_000,
            generated_at: String::new(),
            unusable: None,
        };
        write_manifest(&dir, &[big], &[], &[], "2026-08-01T00:00:00Z").unwrap();

        // The next run hits a page cap: the file is replaced by a much smaller one and
        // the target is reported failed. Because the manifest *merges*, dropping this
        // run's entry would leave the month-old one vouching for 3000 items in a file
        // that now holds one — worse than having no entry at all.
        let small = ManifestEntry {
            path: "raw/github-main.json".into(),
            items: Some(1),
            pages: None,
            bytes: 7,
            generated_at: String::new(),
            unusable: None,
        };
        write_manifest(
            &dir,
            &[small],
            &[],
            &["github[main]".to_string()],
            "2026-08-29T00:00:00Z",
        )
        .unwrap();

        let m: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).unwrap())
                .unwrap();
        assert_eq!(m["files"][0]["items"], 1, "describes what is on disk now");
        assert_eq!(m["files"][0]["generated_at"], "2026-08-29T00:00:00Z");
        assert_eq!(m["complete"], false);
        assert_eq!(m["failed"][0], "github[main]");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_removed_file_loses_its_manifest_entry() {
        let dir = scratch_dir("backup-manifest-removed");
        touch(&dir, "raw/reddit-main.json");
        touch(&dir, "normalized/reddit-main.json");
        let entry = |path: &str| ManifestEntry {
            path: path.into(),
            items: Some(1),
            pages: None,
            bytes: 2,
            generated_at: String::new(),
            unusable: None,
        };
        write_manifest(
            &dir,
            &[
                entry("raw/reddit-main.json"),
                entry("normalized/reddit-main.json"),
            ],
            &[],
            &[],
            "2026-08-01T00:00:00Z",
        )
        .unwrap();

        // A shape break removes the normalized half; its entry must go too, or a consumer
        // walking the manifest hits ENOENT.
        std::fs::remove_file(dir.join("normalized/reddit-main.json")).unwrap();
        write_manifest(
            &dir,
            &[entry("raw/reddit-main.json")],
            &["normalized/reddit-main.json".to_string()],
            &["reddit[main]".to_string()],
            "2026-08-29T00:00:00Z",
        )
        .unwrap();

        let m: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).unwrap())
                .unwrap();
        let paths: Vec<&str> = m["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["path"].as_str().unwrap())
            .collect();
        assert_eq!(paths, vec!["raw/reddit-main.json"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_corrupt_previous_manifest_is_reported_not_silently_dropped() {
        let dir = scratch_dir("backup-manifest-corrupt");
        touch(&dir, "raw/pinboard.json");
        std::fs::write(dir.join("manifest.json"), "{ truncated").unwrap();

        // It still writes (the manifest is metadata, not the backup) but the entries it
        // couldn't read are gone, so this must not pass silently — see the `warn!`.
        let entry = ManifestEntry {
            path: "raw/pinboard.json".into(),
            items: Some(1),
            pages: None,
            bytes: 2,
            generated_at: String::new(),
            unusable: None,
        };
        write_manifest(&dir, &[entry], &[], &[], "2026-08-29T00:00:00Z").unwrap();
        let m: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).unwrap())
                .unwrap();
        assert_eq!(m["files"].as_array().unwrap().len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_narrowed_run_merges_into_the_manifest_instead_of_replacing_it() {
        let dir = scratch_dir("backup-manifest-merge");
        let entry = |path: &str| ManifestEntry {
            path: path.into(),
            items: Some(1),
            pages: None,
            bytes: 10,
            generated_at: String::new(),
            unusable: None,
        };

        // A full run describes two targets…
        touch(&dir, "raw/pinboard.json");
        touch(&dir, "raw/github-main.json");
        write_manifest(
            &dir,
            &[entry("raw/pinboard.json"), entry("raw/github-main.json")],
            &[],
            &[],
            "2026-08-01T00:00:00Z",
        )
        .unwrap();
        // …then `backup pinboard` refreshes only one. Replacing the manifest would leave
        // github's file present but undescribed — the stale-file-behind-a-fresh-timestamp
        // problem the manifest exists to prevent.
        write_manifest(
            &dir,
            &[entry("raw/pinboard.json")],
            &[],
            &[],
            "2026-08-29T00:00:00Z",
        )
        .unwrap();

        let m: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).unwrap())
                .unwrap();
        let files = m["files"].as_array().unwrap();
        assert_eq!(files.len(), 2, "the untouched target is still described");
        let at = |path: &str| {
            files.iter().find(|f| f["path"] == path).unwrap()["generated_at"]
                .as_str()
                .unwrap()
                .to_string()
        };
        assert_eq!(at("raw/pinboard.json"), "2026-08-29T00:00:00Z");
        assert_eq!(
            at("raw/github-main.json"),
            "2026-08-01T00:00:00Z",
            "an untouched file keeps its own, older timestamp"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn a_dry_run_reports_the_same_verdict_as_the_real_run() {
        let dir = scratch_dir("backup-dry-verdict");

        // The whole point of `--dry-run` is validating a config before trusting the
        // timer. If it reported a clean plan for a target the real run flags, an operator
        // would conclude the setup is good and only find out from the journal.
        let dry = run_job(&TruncatedSource, "github-main", &dir, true)
            .await
            .unwrap();
        assert!(
            dry.unusable.is_some(),
            "a dry run must surface the same verdict"
        );
        assert!(!dir.join("raw").exists(), "and still write nothing");

        let real = run_job(&TruncatedSource, "github-main", &dir, false)
            .await
            .unwrap();
        assert_eq!(dry.unusable, real.unusable);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A source whose normalization failed, so `run_job` goes on to remove the stale
    /// normalized half.
    struct ShapeBrokenSource;

    impl BackupSource for ShapeBrokenSource {
        async fn dump(&self) -> Result<BackupDump, SourceError> {
            Ok(BackupDump {
                raw: vec![page("", RawKind::Json, "[1]")],
                records: Err(anyhow::anyhow!("the response shape changed")),
                truncated: None,
            })
        }
    }

    #[tokio::test]
    async fn a_stale_removal_never_costs_the_report_of_what_was_written() {
        let dir = scratch_dir("backup-stale-removal");
        std::fs::create_dir_all(dir.join("normalized")).unwrap();
        std::fs::write(dir.join("normalized/reddit-main.json"), "[]").unwrap();

        let outcome = run_job(&ShapeBrokenSource, "reddit-main", &dir, false)
            .await
            .unwrap();

        // The raw half landed, so it must be reported whatever happened afterwards:
        // returning `Err` from this path would drop its entry and leave the merged
        // manifest describing the previous run's version of a file that just changed.
        assert!(
            outcome
                .written
                .iter()
                .any(|e| e.path == "raw/reddit-main.json"),
            "the written raw half is always reported"
        );
        assert!(outcome.unusable.is_some(), "the shape break is reported");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn a_stale_removal_that_fails_reports_it_without_losing_the_write() {
        use std::os::unix::fs::PermissionsExt;

        let dir = scratch_dir("backup-stale-removal-fails");
        let normalized = dir.join("normalized");
        std::fs::create_dir_all(&normalized).unwrap();
        std::fs::write(normalized.join("reddit-main.json"), "[]").unwrap();
        // A read-only parent makes the unlink fail while leaving the file readable.
        std::fs::set_permissions(&normalized, PermissionsExt::from_mode(0o500)).unwrap();

        let outcome = run_job(&ShapeBrokenSource, "reddit-main", &dir, false)
            .await
            .unwrap();

        assert!(
            outcome
                .written
                .iter()
                .any(|e| e.path == "raw/reddit-main.json"),
            "the raw half is still reported"
        );
        let reason = outcome.unusable.expect("the removal failure is reported");
        assert!(reason.contains("removing stale"), "{reason}");
        // The file is still there, so its entry must not be dropped — `removed` stays
        // empty and the manifest keeps describing what is actually on disk.
        assert!(outcome.removed.is_empty());

        std::fs::set_permissions(&normalized, PermissionsExt::from_mode(0o700)).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_write_that_fails_partway_still_reports_the_files_it_replaced() {
        let dir = scratch_dir("backup-partial-write");
        // Block the second file by putting a regular file where its directory must go.
        std::fs::create_dir_all(dir.join("raw")).unwrap();
        std::fs::write(dir.join("normalized"), "in the way").unwrap();

        let mut written = Vec::new();
        let err = write_files(
            &dir,
            &[
                json_file("raw/pinboard.json", "[1]"),
                json_file("normalized/pinboard.json", "[]"),
            ],
            &mut written,
        )
        .unwrap_err();

        assert!(format!("{err:#}").contains("normalized"), "{err:#}");
        // The first file was replaced before the failure. Reporting it is what stops the
        // merged manifest from keeping the previous run's entry for a file that changed.
        assert_eq!(written.len(), 1);
        assert_eq!(written[0].path, "raw/pinboard.json");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_partial_file_stays_marked_after_a_later_clean_run_of_another_target() {
        let dir = scratch_dir("backup-partial-flag");
        touch(&dir, "raw/github-main.json");
        touch(&dir, "raw/pinboard.json");
        let entry = |path: &str, unusable: Option<&str>| ManifestEntry {
            path: path.into(),
            items: Some(1),
            pages: None,
            bytes: 2,
            generated_at: String::new(),
            unusable: unusable.map(str::to_string),
        };

        // Run 1: github truncated.
        write_manifest(
            &dir,
            &[entry(
                "raw/github-main.json",
                Some("stopped at the page cap"),
            )],
            &[],
            &["github[main]".to_string()],
            "2026-08-01T00:00:00Z",
        )
        .unwrap();

        // Run 2: a clean `backup pinboard`. It rewrites `complete`/`failed`, so without a
        // per-entry flag github's partial file would become indistinguishable from a
        // healthy one — `generated_at` only ever answers "when", not "trustworthy".
        write_manifest(
            &dir,
            &[entry("raw/pinboard.json", None)],
            &[],
            &[],
            "2026-08-29T00:00:00Z",
        )
        .unwrap();

        let m: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).unwrap())
                .unwrap();
        assert_eq!(m["complete"], true, "this run had no failures");
        let github = m["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["path"] == "raw/github-main.json")
            .unwrap();
        assert_eq!(github["unusable"], "stopped at the page cap");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn probe_creates_nothing_and_reports_a_missing_directory() {
        let dir = scratch_dir("backup-probe");

        probe_writable(&dir).unwrap();
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            0,
            "the probe file is removed"
        );

        // A missing directory is reported, never created. Creating it would report ✓ for
        // a typo'd path, and — run as root against the NixOS default — would leave a
        // root-owned /var/lib/pinboard-sync where systemd expects to manage a symlink
        // into /var/lib/private, breaking what the check exists to validate.
        // A directory that doesn't exist yet is healthy — `backup` creates it on its
        // first run — so it must not be a `doctor` failure, and must not be created here.
        let missing = dir.join("nested");
        assert_eq!(probe_writable(&missing).unwrap(), DirProbe::WillBeCreated);
        assert!(!missing.exists(), "the probe must not create it");

        // A typo'd path, whose parent is missing too, is a real failure.
        assert!(probe_writable(&dir.join("no/such/parent")).is_err());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn backup_temp_refuses_to_reuse_a_preexisting_file() {
        use std::os::unix::fs::PermissionsExt;

        let dir = scratch_dir("backup-write-squat");
        let squatted = dir.join(".pinboard-backup.json.tmp.squatter");
        std::fs::write(&squatted, "SECRET-LEAK").unwrap();
        std::fs::set_permissions(&squatted, PermissionsExt::from_mode(0o644)).unwrap();

        // create_new refuses a path that already exists rather than truncating it and
        // inheriting its world-readable mode, so a squatted temp can't leak into a snapshot.
        let err = open_new_private(&squatted).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(&squatted).unwrap(), "SECRET-LEAK");
        let mode = std::fs::metadata(&squatted).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o644, "existing file left untouched");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn backup_temp_does_not_follow_a_symlink() {
        let dir = scratch_dir("backup-write-symlink");
        let victim = dir.join("victim.txt");
        std::fs::write(&victim, "PRECIOUS").unwrap();
        let link = dir.join(".pinboard-backup.json.tmp.link");
        std::os::unix::fs::symlink(&victim, &link).unwrap();

        // O_EXCL refuses the pre-existing symlink instead of following it and truncating
        // the target.
        let err = open_new_private(&link).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "PRECIOUS");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn create_new_temp_retries_past_a_colliding_name() {
        let dir = scratch_dir("backup-temp-retry");
        std::fs::write(dir.join("taken"), "SQUAT").unwrap();

        // First candidate collides with the pre-existing file; the loop must retry the
        // fresh one rather than reuse or truncate "taken".
        let mut names = ["taken", "fresh"].into_iter().map(str::to_string);
        let (tmp, _file) = create_new_temp(&dir, || names.next().unwrap()).unwrap();

        assert_eq!(tmp, dir.join("fresh"));
        assert_eq!(std::fs::read_to_string(dir.join("taken")).unwrap(), "SQUAT");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn create_new_temp_gives_up_after_exhausting_attempts() {
        let dir = scratch_dir("backup-temp-exhaust");
        std::fs::write(dir.join("always"), "SQUAT").unwrap();

        // A generator that never yields a free name exhausts the bounded loop and surfaces
        // the last AlreadyExists rather than spinning forever.
        let err = create_new_temp(&dir, || "always".to_string()).unwrap_err();
        assert_eq!(
            err.downcast_ref::<std::io::Error>()
                .map(std::io::Error::kind),
            Some(std::io::ErrorKind::AlreadyExists)
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_non_array_json_body_may_not_replace_a_snapshot() {
        let bad = BackupFile {
            path: "raw/pinboard.json".into(),
            kind: RawKind::Json,
            body: "<html>maintenance</html>".into(),
            count: Count::Items(0),
        };
        assert!(check_writable(&bad).is_err());

        let empty_html = BackupFile {
            path: "raw/hackernews-me-favorites.html".into(),
            kind: RawKind::Html,
            body: "   \n".into(),
            count: Count::Pages(1),
        };
        assert!(check_writable(&empty_html).is_err());

        let good = BackupFile {
            path: "raw/pinboard.json".into(),
            kind: RawKind::Json,
            body: "[]".into(),
            count: Count::Items(0),
        };
        assert!(check_writable(&good).is_ok());
    }
}
