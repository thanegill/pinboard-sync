//! The `backup` driver: one snapshot directory holding every service's API responses with
//! every field (`raw/`) and the same items as bookmarks (`normalized/`). Every field, not
//! every byte: merging a service's pages re-serializes them, and with no `preserve_order`
//! their object keys come out sorted.
//!
//! Three stages: a client [`dump`](BackupSource::dump)s, [`layout`] names the files (pure),
//! and [`write_files`] alone touches the disk, so clients never learn where a file goes.
//! The driver also owns the manifest and the dry run.
//!
//! The wire structs keep only the fields they need, so `raw/` is the response text, taken
//! by a [`RawSink`] in the clients' own pagination. Never re-walk pagination for it: one
//! traversal feeds both halves, so the two describe one instant.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use log::warn;
use serde::Serialize;
use serde_json::Value;

use crate::bookmark::{Bookmark, WriteMode};
use crate::preview;
use crate::source::{BookmarkDraft, SourceError};
use crate::stdout::outln;

mod fs;

use fs::{create_backup_tmp, create_private_dir, write_atomically};

/// A captured body's media kind: its file's extension, and the check it must pass before
/// replacing a good snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawKind {
    Json,
    Html,
}

impl RawKind {
    const fn extension(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Html => "html",
        }
    }
}

/// The stream of an account's primary JSON payload, whose file takes the bare stem.
pub const PRIMARY_STREAM: &str = "";

/// One verbatim response body, with its stream, which names its file, and its kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawPage {
    pub stream: &'static str,
    pub kind: RawKind,
    pub body: String,
}

/// Collects response bodies as a client paginates, so `backup` gets both halves from one
/// traversal. `sync` and `cleanup` pass a disabled sink, which retains nothing.
#[derive(Debug, Default)]
pub struct RawSink {
    enabled: bool,
    pages: Vec<RawPage>,
    truncated: Option<String>,
}

impl RawSink {
    pub fn disabled() -> Self {
        Self::default()
    }

    /// A sink that retains every body, in push order.
    pub const fn collecting() -> Self {
        Self {
            enabled: true,
            pages: Vec::new(),
            truncated: None,
        }
    }

    pub fn push(&mut self, stream: &'static str, kind: RawKind, body: &str) {
        if self.enabled {
            self.pages.push(RawPage {
                stream,
                kind,
                body: body.to_owned(),
            });
        }
    }

    /// Record that the walk stopped early: a warning for `sync`, but here a partial
    /// snapshot would replace a complete one, so it can't quietly pass.
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

/// One bookmark in `normalized/*.json`. Separate from [`Bookmark`] on purpose: it is an
/// output contract, which a domain rename must not change. Plain `String`s need no serde
/// features for `url` and the time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExportBookmark {
    pub url: String,
    pub title: String,
    pub note: String,
    pub tags: Vec<String>,
    /// RFC 3339, omitted with no creation time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    pub public: bool,
    pub read_later: bool,
    /// The key sync dedups on; absent for Pinboard's dump.
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
            dedup_key: Some(d.dedup_key.as_str().to_owned()),
            ..Self::from(&d.bookmark)
        }
    }
}

/// One account's payload: the bodies in fetch order, and the records from that traversal.
pub struct BackupDump {
    pub raw: Vec<RawPage>,
    /// `Err` when the source's shape broke. The raw half is still written, since a schema
    /// break is when it is worth having; the normalized half is skipped, not emptied, and
    /// the job fails.
    pub records: Result<Vec<ExportBookmark>>,
    /// Set when pagination stopped at a page cap: not a complete snapshot.
    pub truncated: Option<String>,
}

impl BackupDump {
    /// What `sink` captured, with `drafts` as the records.
    pub fn from_drafts(sink: RawSink, drafts: &[BookmarkDraft]) -> Self {
        sink.into_dump(Ok(drafts.iter().map(ExportBookmark::from).collect()))
    }
}

/// A service that can dump what it holds: every source, and Pinboard.
pub trait BackupSource {
    async fn dump(&self) -> Result<BackupDump, SourceError>;
}

/// How many things a file holds. An envelope stream's count is of pages, not items, kept
/// apart so a manifest diff still shows data loss.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Count {
    Items(usize),
    Pages(usize),
}

impl Count {
    const fn parts(self) -> (&'static str, usize) {
        match self {
            Self::Items(n) => ("items", n),
            Self::Pages(n) => ("pages", n),
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

/// A filename-safe slug of an account name: lowercased, each run outside `[a-z0-9_-]` one
/// `-` (so `a.b` and `a/b` clash, for [`check_stem_collisions`]), ends trimmed, and
/// `default` when none or empty, as `job_label` labels it. With no `/` or `..`, a name
/// can't escape the snapshot directory.
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
        "default".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// One stream's JSON bodies as one array: an array body is flattened in (GitHub, Pinboard),
/// any other is one element (Reddit's and Algolia's envelopes). Object keys come out
/// sorted. Pushing every body whole would nest GitHub's arrays. The [`Count`] comes from
/// the same walk: no second parse of Pinboard's export.
pub fn merge_json_pages(bodies: &[&str]) -> Result<(Value, Count)> {
    let mut merged = Vec::new();
    let mut all_arrays = true;
    for (i, body) in bodies.iter().enumerate() {
        let value: Value = serde_json::from_str(body)
            .with_context(|| format!("parsing captured response page {}", i + 1))?;
        if let Value::Array(items) = value {
            merged.extend(items);
        } else {
            all_arrays = false;
            merged.push(value);
        }
    }
    let count = if all_arrays {
        Count::Items(merged.len())
    } else {
        Count::Pages(merged.len())
    };
    Ok((Value::Array(merged), count))
}

/// One stream's HTML pages in fetch order, each behind a marker comment. One file, not one
/// per page: the snapshot is overwritten in place and never pruned, so a page file from a
/// longer earlier run would survive. The file set stays a function of the config, and the
/// markers are inert comments, so each page stays verbatim.
pub fn merge_html_pages(bodies: &[&str]) -> String {
    use std::fmt::Write;

    let mut out = String::new();
    for (i, body) in bodies.iter().enumerate() {
        #[expect(
            clippy::let_underscore_must_use,
            reason = "writing to a `String` cannot fail"
        )]
        let _ = writeln!(out, "<!-- pinboard-sync page {} -->", i + 1);
        out.push_str(body);
        if !body.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// One account's payload as files relative to the snapshot directory, without touching the
/// disk. The primary stream lands at `stem`, any other at `stem-<stream>`.
pub fn layout(stem: &str, dump: &BackupDump) -> Result<Vec<BackupFile>> {
    let mut files = Vec::new();

    // The primary stream is written even with no pages (an emptied favorites list makes no
    // Algolia request), or last run's file would survive beside an emptied normalized half.
    let mut streams: Vec<&'static str> = vec![PRIMARY_STREAM];
    for page in &dump.raw {
        if !streams.contains(&page.stream) {
            streams.push(page.stream);
        }
    }

    for stream in streams {
        let pages: Vec<&RawPage> = dump.raw.iter().filter(|p| p.stream == stream).collect();
        let kind = pages.first().map_or(RawKind::Json, |p| p.kind);
        let bodies: Vec<&str> = pages.iter().map(|p| p.body.as_str()).collect();
        let name = if stream == PRIMARY_STREAM {
            stem.to_owned()
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

/// Reject a body that must not replace a good snapshot: every JSON file `layout` builds is
/// an array, and a 2xx proxy page or a body cut off mid-array passes a status check.
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

/// Fail, before any write, if two jobs would write the same files. Config validation
/// (`config::check_distinct_slugs`) refuses such accounts too; this guards the built stems.
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
    /// `items` or `pages` ([`Count`]), never both.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pages: Option<usize>,
    pub bytes: usize,
    /// When this file was written, not the run: a narrowed run (`backup pinboard`)
    /// refreshes only part of the directory.
    #[serde(default)]
    pub generated_at: String,
    /// Why this file isn't a trustworthy snapshot. On the entry, not only in `failed`,
    /// since a later clean run of another target rewrites `failed`.
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

/// Fail before the fetch when the snapshot directory's parent is missing, so a bad path
/// doesn't spend a rate-limited run.
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

/// Whether a snapshot could be written to `dir`, for `doctor`: a file is created and
/// removed, since permission bits get `DynamicUser`, ACLs and read-only mounts wrong.
///
/// It creates no directory: `mkdir -p` would pass a typo'd path, and as root would leave a
/// real `/var/lib/pinboard-sync` where systemd manages a symlink into `/var/lib/private`.
pub fn probe_writable(dir: &Path) -> Result<DirProbe> {
    if !dir.is_dir() {
        // `backup` creates it; only the parent must exist, which catches a typo'd path.
        check_backup_dir(dir)?;
        return Ok(DirProbe::WillBeCreated);
    }
    let (tmp, _file) = create_backup_tmp(dir, "doctor")?;
    std::fs::remove_file(&tmp)
        .with_context(|| format!("removing the probe file {}", tmp.display()))?;
    Ok(DirProbe::Writable)
}

/// The outcome of [`probe_writable`]. A directory not yet created is no error: `backup` may
/// not have run yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirProbe {
    Writable,
    WillBeCreated,
}

/// What one target did: the files it wrote and deleted, and why what it wrote can't be
/// vouched for. `written`/`removed` are reported even then: the manifest merges over the
/// last run, whose entries would otherwise describe the replaced files.
#[derive(Debug, Default)]
pub struct JobOutcome {
    pub written: Vec<ManifestEntry>,
    /// Relative paths deleted, whose manifest entries go too.
    pub removed: Vec<String>,
    /// Set when the files landed but are untrustworthy: a truncated fetch, a shape break or
    /// a write failed partway. Recorded on each entry too.
    pub unusable: Option<String>,
}

impl JobOutcome {
    /// Add a reason, keeping any already recorded: each describes different damage.
    fn note_unusable(&mut self, reason: String) {
        self.unusable = Some(match self.unusable.take() {
            Some(prior) => format!("{prior}; {reason}"),
            None => reason,
        });
    }
}

/// Back up one target under `dir`, or in [`WriteMode::DryRun`] print what would be written.
/// `Err` means nothing was written; a target that wrote files it can't vouch for returns
/// `Ok` with `unusable` set, so the caller records both.
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

    // Before the dry-run branch too. Neither is an `Err`: a schema break is when the
    // verbatim body is worth keeping, and a truncated snapshot beats none.
    let mut unusable: Vec<String> = Vec::new();
    if let Err(e) = &dump.records {
        unusable.push(format!("{e:#}"));
    }
    if let Some(reason) = &dump.truncated {
        unusable.push(format!(
            "{reason} — the snapshot for this target is incomplete"
        ));
    }
    let mut outcome = JobOutcome {
        written: files.iter().map(ManifestEntry::from).collect(),
        unusable: (!unusable.is_empty()).then(|| unusable.join("; ")),
        ..JobOutcome::default()
    };

    if mode.is_dry_run() {
        for file in &files {
            outln!("{}", preview::header(file_path(dir, file).display()));
            let (label, value) = file.count.parts();
            outln!("{}", preview::field(label, value));
            outln!("{}", preview::field("bytes", file.body.len()));
        }
        return Ok(outcome);
    }

    // What actually reached the disk: a failure partway still replaced the earlier files.
    let mut written = Vec::new();
    let write_result = write_files(dir, &files, &mut written);
    outcome.written = written;
    if let Err(e) = write_result {
        outcome.note_unusable(format!("{e:#}"));
        return Ok(outcome);
    }

    // Last run's normalized half would disagree with this raw half; absent says "unknown".
    if dump.records.is_err() {
        let relative = normalized_path(stem);
        let stale = dir.join(&relative);
        if stale.exists() {
            // Not `?`: past the write, `Err` would drop the entries of files already on
            // disk. Problems past it ride on the outcome: `Err` means nothing was written.
            if let Err(e) = std::fs::remove_file(&stale) {
                outcome.note_unusable(format!("removing stale {}: {e}", stale.display()));
                return Ok(outcome);
            }
        }
        // Even when already absent: the manifest may still carry an entry for it.
        outcome.removed.push(relative);
    }

    Ok(outcome)
}

/// The normalized half's relative path, shared by [`layout`] and [`run_job`]'s removal.
fn normalized_path(stem: &str) -> String {
    format!("normalized/{stem}.json")
}

/// Write the run's manifest, last, so a reader can tell a complete snapshot from a failed
/// run's. Each file is atomic but the run is not: one whose third target fails mixes its
/// files with the last run's. `failed` names the failed targets and `complete` is false, so
/// a stale file can't hide behind a fresh `generated_at`.
///
/// An unlocked read-modify-write: two runs on one directory lose each other's entries, as
/// they would race on the data files anyway. A snapshot directory has one writer.
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
        /// Whether this run finished every target it attempted, and nothing about the rest:
        /// each entry's `generated_at` tells that.
        complete: bool,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        failed: Vec<String>,
        files: Vec<ManifestEntry>,
    }

    // Merged, not replaced: a narrowed run (`backup pinboard`) would otherwise leave the
    // other targets' files undescribed.
    let path = dir.join("manifest.json");
    let mut files: Vec<ManifestEntry> = std::fs::read_to_string(&path).map_or_else(
        |_| Vec::new(),
        |text| match serde_json::from_str::<Manifest>(&text) {
            Ok(m) => m.files,
            // Never silent: this undoes the merge.
            Err(e) => {
                warn!(
                    "{} exists but could not be parsed ({e}); its entries are being \
                     dropped and the manifest rebuilt from this run only",
                    path.display()
                );
                Vec::new()
            }
        },
    );
    files.retain(|f| !removed.contains(&f.path));
    for entry in entries {
        let mut entry = entry.clone();
        generated_at.clone_into(&mut entry.generated_at);
        match files.iter_mut().find(|f| f.path == entry.path) {
            Some(existing) => *existing = entry,
            None => files.push(entry),
        }
    }
    // A renamed or dropped account's file goes. `try_exists`, not `exists`, which says
    // `false` on an I/O error and would prune entries of files still there.
    files.retain(|f| dir.join(&f.path).try_exists().unwrap_or(true));
    files.sort_by(|a, b| a.path.cmp(&b.path));

    let body = serde_json::to_string_pretty(&Manifest {
        generated_at: generated_at.to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        complete: failed.is_empty(),
        failed: failed.to_vec(),
        files,
    })
    .context("serializing backup manifest")?;
    // A first run whose every target failed created no `dir`, and ENOENT would hide why.
    create_private_dir(dir)?;
    write_atomically(&path, &body)
}

/// The absolute path each file is written to.
pub fn file_path(dir: &Path, file: &BackupFile) -> PathBuf {
    dir.join(&file.path)
}

/// Write every file under `dir`, each atomically, checking them all first so a bad last
/// body can't leave the snapshot half-replaced.
pub fn write_files(
    dir: &Path,
    files: &[BackupFile],
    written: &mut Vec<ManifestEntry>,
) -> Result<()> {
    for file in files {
        check_writable(file)?;
    }
    // Pushed as each lands, so a failure partway still reports the files already replaced.
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
