use super::*;
use crate::test_support::{scratch_dir, stored_at, strings, tag_list};
use time::OffsetDateTime;

fn page(stream: &'static str, kind: RawKind, body: &str) -> RawPage {
    RawPage {
        stream,
        kind,
        body: body.to_string(),
    }
}

fn bookmark() -> Bookmark {
    Bookmark {
        title: "Title".into(),
        note: "Note".into(),
        tags: tag_list(&["reddit"]),
        timestamp: OffsetDateTime::from_unix_timestamp(1_600_000_000).ok(),
        read_later: true,
        ..stored_at("https://example.com/a")
    }
}

#[test]
fn disabled_sink_retains_nothing() {
    let mut sink = RawSink::disabled();
    sink.push("saved", RawKind::Json, "[]");
    let (pages, _) = sink.into_parts();
    assert!(pages.is_empty(), "{pages:?}");
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
    // docs promise *fields*, not bytes -- see `merge_json_pages`.
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
    assert!(
        merged.contains("<!-- pinboard-sync page 1 -->"),
        "{merged:?}"
    );
    assert!(
        merged.contains("<!-- pinboard-sync page 2 -->"),
        "{merged:?}"
    );
    assert!(merged.contains("<a>one</a>"), "{merged:?}");
    assert!(merged.contains("<a>two</a>"), "{merged:?}");
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
    assert!(check_stem_collisions(["reddit-a", "reddit-b"]).is_ok());

    let err = check_stem_collisions(["reddit-a-b", "reddit-a-b"]).unwrap_err();
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
    assert_eq!(json.get("dedup_key"), None);

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
    // freshly emptied `normalized/*.json` -- the two halves silently disagreeing.
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
    let scratch = scratch_dir("backup-manifest");
    let dir = scratch.path();
    touch(dir, "raw/pinboard.json");
    let entries = vec![ManifestEntry {
        path: "raw/pinboard.json".into(),
        items: Some(3),
        pages: None,
        bytes: 10,
        generated_at: String::new(),
        unusable: None,
    }];

    write_manifest(dir, &entries, &[], &[], "2026-08-29T00:00:00Z").unwrap();
    let ok: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(ok["complete"], true);
    assert_eq!(ok.get("failed"), None);
    assert_eq!(ok["files"][0]["items"], 3);
    assert!(ok["files"][0].get("pages").is_none(), "never both counts");

    // A failed target left its previous files in place; without this the fresh
    // `generated_at` would vouch for them.
    write_manifest(
        dir,
        &entries,
        &[],
        &strings(&["github[work]"]),
        "2026-08-29T00:00:00Z",
    )
    .unwrap();
    let partial: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(partial["complete"], false);
    assert_eq!(partial["failed"][0], "github[work]");
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
    let scratch = scratch_dir("backup-write-ok");
    let dir = scratch.path();
    let target = dir.join("raw/pinboard.json");
    write_files(
        dir,
        &[json_file("raw/pinboard.json", "[1]")],
        &mut Vec::new(),
    )
    .unwrap();
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "[1]");

    // A second run overwrites in place and leaves no temp behind.
    write_files(
        dir,
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
}

#[test]
fn written_snapshots_are_not_world_readable() {
    use std::os::unix::fs::PermissionsExt;

    let scratch = scratch_dir("backup-write-perms");
    let dir = scratch.path();
    write_files(
        dir,
        &[json_file("raw/pinboard.json", "[]")],
        &mut Vec::new(),
    )
    .unwrap();

    // A snapshot holds every private bookmark: file and directory alike stay private.
    let mode = |p: PathBuf| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(dir.join("raw/pinboard.json")), 0o600);
    assert_eq!(mode(dir.join("raw")), 0o700);
}

#[test]
fn a_bad_body_anywhere_leaves_every_previous_file_intact() {
    let scratch = scratch_dir("backup-write-bad");
    let dir = scratch.path();
    write_files(
        dir,
        &[json_file("raw/pinboard.json", "[1]")],
        &mut Vec::new(),
    )
    .unwrap();

    // A 200 that isn't a JSON array (proxy page, empty body) or a connection dropped
    // mid-array must not clobber a good snapshot -- and because every file is checked
    // before any is written, a bad *second* file can't leave the first replaced.
    for bad in [
        "",
        "  ",
        "<html>Back off</html>",
        r#"[{"href":"https://x/"}"#,
    ] {
        let err = write_files(
            dir,
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
    let scratch = scratch_dir("backup-dry-run");
    let dir = scratch.path();

    run_job(&FakeSource("[1]"), "pinboard", dir, WriteMode::DryRun)
        .await
        .unwrap();
    assert!(!dir.join("raw").exists(), "a dry run touches no files");

    // A dry run must not bless a plan the real run would abort on -- an operator
    // validating a config behind a captive portal would otherwise see "would be
    // written" and conclude the setup is good. (A non-JSON body is rejected by the
    // merge before `check_writable` sees it; either way the run fails.)
    let err = run_job(
        &FakeSource("<html>portal</html>"),
        "pinboard",
        dir,
        WriteMode::DryRun,
    )
    .await
    .unwrap_err();
    assert!(
        format!("{err:#}").contains("parsing captured response page 1"),
        "{err:#}"
    );
    assert!(!dir.join("raw").exists());
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
    let scratch = scratch_dir("backup-truncated");
    let dir = scratch.path();

    // The files are still written -- a partial snapshot beats none -- but the target
    // is reported failed so it lands in the manifest's `failed` list. Silently
    // replacing a complete snapshot with a truncated one is the failure mode here.
    let outcome = run_job(&TruncatedSource, "github-main", dir, WriteMode::Live)
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
}

#[test]
fn a_failed_target_updates_its_manifest_entry_rather_than_keeping_the_old_one() {
    let scratch = scratch_dir("backup-manifest-failed");
    let dir = scratch.path();
    touch(dir, "raw/github-main.json");
    let big = ManifestEntry {
        path: "raw/github-main.json".into(),
        items: Some(3000),
        pages: None,
        bytes: 12_000_000,
        generated_at: String::new(),
        unusable: None,
    };
    write_manifest(dir, &[big], &[], &[], "2026-08-01T00:00:00Z").unwrap();

    // The next run hits a page cap: the file is replaced by a much smaller one and
    // the target is reported failed. Because the manifest *merges*, dropping this
    // run's entry would leave the month-old one vouching for 3000 items in a file
    // that now holds one -- worse than having no entry at all.
    let small = ManifestEntry {
        path: "raw/github-main.json".into(),
        items: Some(1),
        pages: None,
        bytes: 7,
        generated_at: String::new(),
        unusable: None,
    };
    write_manifest(
        dir,
        &[small],
        &[],
        &strings(&["github[main]"]),
        "2026-08-29T00:00:00Z",
    )
    .unwrap();

    let m: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(m["files"][0]["items"], 1, "describes what is on disk now");
    assert_eq!(m["files"][0]["generated_at"], "2026-08-29T00:00:00Z");
    assert_eq!(m["complete"], false);
    assert_eq!(m["failed"][0], "github[main]");
}

#[test]
fn a_removed_file_loses_its_manifest_entry() {
    let scratch = scratch_dir("backup-manifest-removed");
    let dir = scratch.path();
    touch(dir, "raw/reddit-main.json");
    touch(dir, "normalized/reddit-main.json");
    let entry = |path: &str| ManifestEntry {
        path: path.into(),
        items: Some(1),
        pages: None,
        bytes: 2,
        generated_at: String::new(),
        unusable: None,
    };
    write_manifest(
        dir,
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
        dir,
        &[entry("raw/reddit-main.json")],
        &strings(&["normalized/reddit-main.json"]),
        &strings(&["reddit[main]"]),
        "2026-08-29T00:00:00Z",
    )
    .unwrap();

    let m: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).unwrap()).unwrap();
    let paths: Vec<&str> = m["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, vec!["raw/reddit-main.json"]);
}

#[test]
fn a_corrupt_previous_manifest_is_reported_not_silently_dropped() {
    let scratch = scratch_dir("backup-manifest-corrupt");
    let dir = scratch.path();
    touch(dir, "raw/pinboard.json");
    std::fs::write(dir.join("manifest.json"), "{ truncated").unwrap();

    // It still writes (the manifest is metadata, not the backup) but the entries it
    // couldn't read are gone, so this must not pass silently -- see the `warn!`.
    let entry = ManifestEntry {
        path: "raw/pinboard.json".into(),
        items: Some(1),
        pages: None,
        bytes: 2,
        generated_at: String::new(),
        unusable: None,
    };
    write_manifest(dir, &[entry], &[], &[], "2026-08-29T00:00:00Z").unwrap();
    let m: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(m["files"].as_array().unwrap().len(), 1);
}

#[test]
fn a_narrowed_run_merges_into_the_manifest_instead_of_replacing_it() {
    let scratch = scratch_dir("backup-manifest-merge");
    let dir = scratch.path();
    let entry = |path: &str| ManifestEntry {
        path: path.into(),
        items: Some(1),
        pages: None,
        bytes: 10,
        generated_at: String::new(),
        unusable: None,
    };

    // A full run describes two targets...
    touch(dir, "raw/pinboard.json");
    touch(dir, "raw/github-main.json");
    write_manifest(
        dir,
        &[entry("raw/pinboard.json"), entry("raw/github-main.json")],
        &[],
        &[],
        "2026-08-01T00:00:00Z",
    )
    .unwrap();
    // ...then `backup pinboard` refreshes only one. Replacing the manifest would leave
    // github's file present but undescribed -- the stale-file-behind-a-fresh-timestamp
    // problem the manifest exists to prevent.
    write_manifest(
        dir,
        &[entry("raw/pinboard.json")],
        &[],
        &[],
        "2026-08-29T00:00:00Z",
    )
    .unwrap();

    let m: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).unwrap()).unwrap();
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
}

#[tokio::test]
async fn a_dry_run_reports_the_same_verdict_as_the_real_run() {
    let scratch = scratch_dir("backup-dry-verdict");
    let dir = scratch.path();

    // The whole point of `--dry-run` is validating a config before trusting the
    // timer. If it reported a clean plan for a target the real run flags, an operator
    // would conclude the setup is good and only find out from the journal.
    let dry = run_job(&TruncatedSource, "github-main", dir, WriteMode::DryRun)
        .await
        .unwrap();
    assert!(
        dry.unusable.is_some(),
        "a dry run must surface the same verdict"
    );
    assert!(!dir.join("raw").exists(), "and still write nothing");

    let real = run_job(&TruncatedSource, "github-main", dir, WriteMode::Live)
        .await
        .unwrap();
    assert_eq!(dry.unusable, real.unusable);
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
    let scratch = scratch_dir("backup-stale-removal");
    let dir = scratch.path();
    std::fs::create_dir_all(dir.join("normalized")).unwrap();
    std::fs::write(dir.join("normalized/reddit-main.json"), "[]").unwrap();

    let outcome = run_job(&ShapeBrokenSource, "reddit-main", dir, WriteMode::Live)
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
}

#[tokio::test]
async fn a_stale_removal_that_fails_reports_it_without_losing_the_write() {
    use std::os::unix::fs::PermissionsExt;

    let scratch = scratch_dir("backup-stale-removal-fails");
    let dir = scratch.path();
    let normalized = dir.join("normalized");
    std::fs::create_dir_all(&normalized).unwrap();
    std::fs::write(normalized.join("reddit-main.json"), "[]").unwrap();
    // A read-only parent makes the unlink fail while leaving the file readable.
    std::fs::set_permissions(&normalized, PermissionsExt::from_mode(0o500)).unwrap();

    let outcome = run_job(&ShapeBrokenSource, "reddit-main", dir, WriteMode::Live)
        .await
        .unwrap();
    std::fs::set_permissions(&normalized, PermissionsExt::from_mode(0o700)).unwrap();

    assert!(
        outcome
            .written
            .iter()
            .any(|e| e.path == "raw/reddit-main.json"),
        "the raw half is still reported"
    );
    let reason = outcome.unusable.expect("the removal failure is reported");
    assert!(reason.contains("removing stale"), "{reason}");
    // The file is still there, so its entry must not be dropped -- `removed` stays
    // empty and the manifest keeps describing what is actually on disk.
    assert!(outcome.removed.is_empty(), "{:?}", outcome.removed);
    scratch.close().unwrap();
}

#[test]
fn a_write_that_fails_partway_still_reports_the_files_it_replaced() {
    let scratch = scratch_dir("backup-partial-write");
    let dir = scratch.path();
    // Block the second file by putting a regular file where its directory must go.
    std::fs::create_dir_all(dir.join("raw")).unwrap();
    std::fs::write(dir.join("normalized"), "in the way").unwrap();

    let mut written = Vec::new();
    let err = write_files(
        dir,
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
}

#[test]
fn a_partial_file_stays_marked_after_a_later_clean_run_of_another_target() {
    let scratch = scratch_dir("backup-partial-flag");
    let dir = scratch.path();
    touch(dir, "raw/github-main.json");
    touch(dir, "raw/pinboard.json");
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
        dir,
        &[entry(
            "raw/github-main.json",
            Some("stopped at the page cap"),
        )],
        &[],
        &strings(&["github[main]"]),
        "2026-08-01T00:00:00Z",
    )
    .unwrap();

    // Run 2: a clean `backup pinboard`. It rewrites `complete`/`failed`, so without a
    // per-entry flag github's partial file would become indistinguishable from a
    // healthy one -- `generated_at` only ever answers "when", not "trustworthy".
    write_manifest(
        dir,
        &[entry("raw/pinboard.json", None)],
        &[],
        &[],
        "2026-08-29T00:00:00Z",
    )
    .unwrap();

    let m: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(m["complete"], true, "this run had no failures");
    let github = m["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["path"] == "raw/github-main.json")
        .unwrap();
    assert_eq!(github["unusable"], "stopped at the page cap");
}

#[test]
fn probe_creates_nothing_and_reports_a_missing_directory() {
    let scratch = scratch_dir("backup-probe");
    let dir = scratch.path();

    probe_writable(dir).unwrap();
    assert_eq!(
        std::fs::read_dir(dir).unwrap().count(),
        0,
        "the probe file is removed"
    );

    // A missing directory is reported, never created. Creating it would report ✓ for
    // a typo'd path, and -- run as root against the NixOS default -- would leave a
    // root-owned /var/lib/pinboard-sync where systemd expects to manage a symlink
    // into /var/lib/private, breaking what the check exists to validate.
    // A directory that doesn't exist yet is healthy -- `backup` creates it on its
    // first run -- so it must not be a `doctor` failure, and must not be created here.
    let missing = dir.join("nested");
    assert_eq!(probe_writable(&missing).unwrap(), DirProbe::WillBeCreated);
    assert!(!missing.exists(), "the probe must not create it");

    // A typo'd path, whose parent is missing too, is a real failure.
    probe_writable(&dir.join("no/such/parent")).unwrap_err();
}

#[test]
fn a_non_array_json_body_may_not_replace_a_snapshot() {
    let bad = BackupFile {
        path: "raw/pinboard.json".into(),
        kind: RawKind::Json,
        body: "<html>maintenance</html>".into(),
        count: Count::Items(0),
    };
    check_writable(&bad).unwrap_err();

    let empty_html = BackupFile {
        path: "raw/hackernews-me-favorites.html".into(),
        kind: RawKind::Html,
        body: "   \n".into(),
        count: Count::Pages(1),
    };
    check_writable(&empty_html).unwrap_err();

    let good = BackupFile {
        path: "raw/pinboard.json".into(),
        kind: RawKind::Json,
        body: "[]".into(),
        count: Count::Items(0),
    };
    check_writable(&good).unwrap();
}
