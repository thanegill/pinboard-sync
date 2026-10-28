//! The `backup` driver: one snapshot directory holding, for every service, the API
//! responses with **every field** intact (`raw/`) and the same items as domain bookmarks
//! (`normalized/`).
//!
//! "Every field", not "every byte": merging the pages of one service into a single file
//! means re-serializing, and `serde_json` here has no `preserve_order`, so object keys
//! come out sorted. Nothing is dropped, but the file is not the server's bytes.
//!
//! Backing up runs in three stages: a client [`dump`](BackupSource::dump)s (async),
//! [`layout`] turns that payload into named files (pure), and [`write_files`] puts them on
//! disk, the only stage that touches the filesystem. The clients know how to produce their
//! data and nothing about where it goes, the same split `cleanup_pass::run_pass` makes.
//!
//! Raw fidelity matters because every wire struct in this crate deserializes only the
//! fields it needs, with no catch-all: `GitHubStarredRepo` keeps a handful of GitHub's
//! ~80 repo fields, and `HackerNewsItem` is already a normalized view of an Algolia hit.
//! So `raw/` is captured as response *text*, before any parsing, by a [`RawSink`] the
//! clients push to as they walk their existing pagination -- one traversal feeds both
//! halves, and the two can never disagree about a point in time.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use log::warn;
use serde::Serialize;
use serde_json::Value;

use crate::bookmark::{Bookmark, WriteMode};
use crate::source::{BookmarkDraft, SourceError};
use crate::stdout::outln;

mod fs;

use fs::{create_backup_tmp, create_private_dir, write_atomically};

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
    /// A sink that discards everything -- for the `sync`/`cleanup` read paths.
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

    /// The dump of what this sink captured, with `records` as its normalized half.
    pub fn into_dump(self, records: Result<Vec<ExportBookmark>>) -> BackupDump {
        BackupDump {
            raw: self.pages,
            records,
            truncated: self.truncated,
        }
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
            tags: b.tags.to_vec(),
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
            dedup_key: Some(d.dedup_key.as_str().to_string()),
            ..Self::from(&d.bookmark)
        }
    }
}

/// One account's payload before it is laid out on disk: the verbatim bodies in fetch
/// order, and the normalized records built from that same traversal.
pub struct BackupDump {
    pub raw: Vec<RawPage>,
    /// `Err` when the source's shape broke so no bookmark could be built. The raw half is
    /// still written (a schema break is exactly when the verbatim body is worth having),
    /// the normalized half is skipped rather than emptied, and the job is reported failed.
    pub records: Result<Vec<ExportBookmark>>,
    /// Set when pagination stopped at a page cap, so this dump is only part of the
    /// account and must not pass as a complete snapshot.
    pub truncated: Option<String>,
}

impl BackupDump {
    /// The dump of a source that shapes its drafts as it paginates (GitHub, HackerNews):
    /// everything `sink` captured, with `drafts` as the records.
    pub fn from_drafts(sink: RawSink, drafts: &[BookmarkDraft]) -> Self {
        sink.into_dump(Ok(drafts.iter().map(ExportBookmark::from).collect()))
    }
}

/// A service that can dump what it holds. Implemented by the three `Source` clients and
/// by the Pinboard client, so `backup` covers the destination as well as the sources.
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
/// outside `[a-z0-9_-]` -- `.` included, so `a.b` and `a/b` slug alike and
/// [`check_stem_collisions`] has to catch the clash -- collapsed to a single `-` and the
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
/// envelope, Algolia's `{"hits":[...]}`).
///
/// So GitHub and Pinboard produce a flat array of items while Reddit and HackerNews
/// produce an array of page envelopes. Always pushing envelopes would make GitHub an
/// array-of-arrays, which is worse to consume.
///
/// Every field survives, but object keys come back out sorted (`serde_json` is built
/// without `preserve_order`), so the result holds the server's fields, not its bytes.
///
/// Returns the [`Count`] alongside, since the classification (were all the bodies arrays?)
/// falls out of the same walk -- recomputing it would mean parsing every body twice, and
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
    use std::fmt::Write;

    let mut out = String::new();
    for (i, body) in bodies.iter().enumerate() {
        let _ = writeln!(out, "<!-- pinboard-sync page {} -->", i + 1);
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
    // freshly emptied normalized half -- the exact disagreement the pairing rules out.
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

    // Skipped, not emptied, when normalization failed -- see `BackupDump::records`.
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

/// Bail if two jobs would write the same files -- `a.b` and `a/b` both slug to `a-b`, and
/// the second job would silently overwrite the first. Config validation
/// (`config::check_distinct_slugs`) already rejects such accounts; this stays as the
/// guard on the stems actually built. Checked before any write, not during.
pub fn check_stem_collisions<'a>(stems: impl IntoIterator<Item = &'a str>) -> Result<()> {
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
    /// Either `items` or `pages` -- see [`Count`]; never both, so a manifest never implies
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
/// path -- it would simply create it -- and, run as root against the NixOS default, would
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

/// What one target actually did: the files it wrote, the files it deleted, and -- when the
/// target wrote something it can't vouch for -- why.
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
    /// Add a reason the snapshot can't be trusted, keeping any already recorded -- two
    /// problems describe different damage, and dropping one costs the diagnosis.
    fn note_unusable(&mut self, reason: String) {
        self.unusable = Some(match self.unusable.take() {
            Some(prior) => format!("{prior}; {reason}"),
            None => reason,
        });
    }
}

/// Back up one target: dump it, lay the payload out, and write it under `dir` -- or, in
/// [`WriteMode::DryRun`], render what would be written without touching the filesystem.
///
/// `Err` means nothing was written (the fetch, layout, or a pre-write check failed).
/// A target that wrote files it can't vouch for returns `Ok` with `unusable` set, so the
/// caller can record both what landed and that it isn't good.
pub async fn run_job<S: BackupSource>(
    source: &S,
    stem: &str,
    dir: &Path,
    mode: WriteMode,
) -> Result<JobOutcome, SourceError> {
    let dump = source.dump().await?;
    let files = layout(stem, &dump)?;
    // Checked in both modes, so a dry run can't report a plan the real run would refuse.
    for file in &files {
        check_writable(file)?;
    }

    // Decided before the dry-run branch, for the same reason: a dry run that reported a
    // clean plan for a target the real run flags would be worse than useless -- it is what
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

    if mode.is_dry_run() {
        for file in &files {
            outln!("[dry-run] {}", file_path(dir, file).display());
            let (label, value) = file.count.parts();
            outln!("          {label:<6}-> {value}");
            outln!("          {:<6}-> {}", "bytes", file.body.len());
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
    // beside a freshly written raw half -- the two would silently disagree. Absent says
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
/// `complete` is false, so a stale file can't hide behind a fresh `generated_at` --
/// without them a reader would see today's timestamp over week-old data and believe it.
///
/// This is a read-modify-write with no locking, so two runs writing the *same* directory
/// concurrently lose each other's entries. That is not new -- they would already be racing
/// on the data files themselves, and a snapshot directory is single-writer by design --
/// but the merge is what makes `manifest.json` shared state, so it is worth stating.
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
        /// It says nothing about targets the run didn't attempt -- for that, compare each
        /// entry's own `generated_at`.
        complete: bool,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        failed: Vec<String>,
        files: Vec<ManifestEntry>,
    }

    // Merge over the previous manifest rather than replacing it. A narrowed run
    // (`backup pinboard`) refreshes part of the directory; replacing the manifest would
    // leave the other targets' files present but undescribed -- the stale-file-behind-a-
    // fresh-timestamp problem the manifest exists to prevent. Each entry carries its own
    // `generated_at`, so a file older than the run is visible as such.
    let path = dir.join("manifest.json");
    let mut files: Vec<ManifestEntry> = match std::fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str::<Manifest>(&text) {
            Ok(m) => m.files,
            // Never silent: discarding the previous entries here would quietly undo the
            // merge and let a narrowed run write a manifest describing only its own
            // target -- the very failure merging exists to prevent.
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

#[cfg(test)]
mod tests;
