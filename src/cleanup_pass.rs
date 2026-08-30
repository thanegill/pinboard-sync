//! The shared `cleanup` driver. Each source describes how to re-shape one bookmark
//! (a [`CleanupPass`]); this module owns the loop common to all of them: plan every
//! bookmark's end-state, then group the plans by their target URL and write each group.
//! A lone plan is diffed against its stored bookmark, skipped when nothing changed, and
//! written via [`BookmarkStore::apply_update`] (deleting the old URL on a rewrite). Two or
//! more plans landing on one URL are a collision: they are field-merged (see
//! [`merge_bookmarks`]) into a single record written via [`BookmarkStore::apply_merge`],
//! which deletes the absorbed URLs. A bookmark already stored at the target joins that
//! merge as well — Pinboard holds one record per URL, so the end state there has to be a
//! single bookmark, and merging keeps what it had instead of replacing it. A plan that
//! disagrees with it about visibility does not join: that one is refused and left where it
//! is, while the rest of the group merges. The exception is a bookmark that is itself
//! planned to move *away* from the target — it is not what stays there, so it neither leads
//! the merge nor blocks it; its own plan is what preserves it. A rewrite's old URL is never deleted when it is
//! itself the target of another planned write in the same pass, so colliding/chained
//! rewrites can't clobber each other's record and the pass is order-independent.
//! `run_pass` renders the dry-run lines and tallies into a [`PassOutcome`], which decides
//! on its own whether the run failed — see [`PassOutcome::into_result`].

use std::collections::HashMap;

use anyhow::{anyhow, Result};
use log::{debug, error, info, warn};
use time::OffsetDateTime;

use crate::bookmark::{AccountState, Bookmark, BookmarkStore};
use crate::source::SourceError;

/// The `use_post_date` policy applied uniformly across a pass: whether to re-date by
/// the source date, the backdate age cap, and whether to push stale (older-than-cap)
/// items to "now". Resolved once by the caller from its source's cleanup options.
#[derive(Clone, Copy)]
pub struct DateOpts {
    pub use_post_date: bool,
    pub max_age_days: u64,
    pub stale_to_now: bool,
}

/// What a pass decided about one bookmark. [`Plan::Unchanged`] and [`Plan::Skipped`]
/// both mean "write nothing", but they say opposite things about the *source*: the
/// first is an answer from it, the second is a bookmark the pass never asked about.
/// Only the first is evidence the source is reachable, which is what lets the driver
/// tell one dead link apart from an outage.
pub enum Plan {
    /// The bookmark's desired end-state; the driver diffs it and writes what changed.
    Bookmark(Bookmark),
    /// The source answered, and nothing about this bookmark needs to change.
    Unchanged,
    /// Not this pass's bookmark — a deep link, a foreign URL, a locally-filtered case.
    /// No lookup was made, so this says nothing about whether the source is up.
    Skipped,
}

#[cfg(test)]
impl Plan {
    /// The planned bookmark, or `None` for the two write-nothing variants.
    pub fn bookmark(self) -> Option<Bookmark> {
        match self {
            Plan::Bookmark(bookmark) => Some(bookmark),
            Plan::Unchanged | Plan::Skipped => None,
        }
    }
}

/// How a source re-shapes one bookmark during `cleanup`.
#[allow(async_fn_in_trait)]
pub trait CleanupPass {
    /// The end-state for `bookmark` as a [`Plan`] — distinguishing an answer from the
    /// source ([`Plan::Bookmark`], [`Plan::Unchanged`]) from a bookmark this pass does
    /// not handle ([`Plan::Skipped`]), which the driver needs to tell one dead link
    /// apart from an unreachable source.
    /// [`SourceError::Other`] marks a per-item failure (logged and counted; the
    /// pass continues with the next bookmark), while [`SourceError::ReauthRequired`] and
    /// [`SourceError::RateLimited`] stop the pass — a dead credential and an exhausted
    /// quota both fail every remaining lookup too. A plan's
    /// `timestamp` is the *candidate* source date; the driver resolves the final date
    /// from it via the pass's [`DateOpts`], and takes `public`/`read_later` from the
    /// stored bookmark — so those two fields on the returned `Bookmark` are ignored. (That
    /// holds of each plan; the record *written* for a group of colliding plans still
    /// follows [`merge_bookmarks`]'s rules across their stored flags.) The driver still
    /// skips an unchanged plan (one
    /// whose fields all match `bookmark`), so a pass can return the computed end-state
    /// without checking for changes itself.
    async fn plan(&self, bookmark: &Bookmark) -> Result<Plan, SourceError>;
}

/// What one pass did. The two failure counts are kept apart because they mean
/// different things: a `plan` failure is one link we could not read from the source,
/// while a write failure is the destination itself refusing us.
/// `must_use` because the whole design rests on [`PassOutcome::into_result`] being the
/// one place the exit code is decided — dropping an outcome silently discards every
/// failure it recorded.
#[must_use]
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PassOutcome {
    /// Bookmarks written, or under `dry_run` that would be.
    pub changed: usize,
    /// Rewrites abandoned because their target must not be written — see [`Refusal`] for
    /// the reasons. Not a failure — leaving it alone is the safe outcome — but it is work
    /// that did not happen, so it is reported rather than only logged per-item.
    pub refused: usize,
    /// Bookmarks the source gave an answer for — the evidence that it is reachable.
    /// Excludes [`Plan::Skipped`], which never asked it anything.
    pub reached: usize,
    /// Bookmarks whose `plan` failed to read the source.
    pub plan_failed: usize,
    /// Bookmarks whose write to the store failed.
    pub write_failed: usize,
    /// Set when the source told us to stop partway. See [`Halt`].
    pub halted: Option<Halt>,
}

/// Why a pass stopped planning early: the source said something that makes every
/// *remaining* lookup fail the same way, so working through them would only spend
/// requests to collect identical errors.
///
/// Two variants rather than one message because only one of them is an auth problem:
/// [`PassOutcome::into_result`] maps them to different [`SourceError`]s, which is what
/// lets `main` fire the `--on-auth-failure` hook for [`Halt::Reauth`] and withhold it for
/// [`Halt::RateLimited`] — no credential change clears a quota.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Halt {
    /// A credential needs refreshing.
    Reauth(String),
    /// The service is refusing requests until a reset; no credential change helps.
    RateLimited(String),
}

/// Why a rewrite was left in place rather than written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refusal {
    /// The target holds a record this pass could not read — a failed lookup, or one a halt
    /// stopped it short of — so what is stored there may be stale.
    Unreadable,
    /// The target holds a bookmark whose own rewrite was refused, so it never moved and its
    /// URL is still occupied. A visibility mismatch reaches the driver this way: the reason
    /// the mover stayed is logged where it is actionable, at the point of refusal, and what
    /// the *next* group needs to know is only that the URL is still taken.
    OccupiedByRefused,
}

impl Refusal {
    fn explain(self) -> &'static str {
        match self {
            Refusal::Unreadable => "occupied by a bookmark this pass could not read",
            Refusal::OccupiedByRefused => "occupied by a rewrite that was itself refused",
        }
    }
}

impl Halt {
    /// The operator-facing explanation, without a variant prefix.
    fn message(&self) -> &str {
        match self {
            Halt::Reauth(message) | Halt::RateLimited(message) => message,
        }
    }
}

impl PassOutcome {
    /// The run's verdict. A link we could not look up is *not* a failed run: it is
    /// logged, skipped, and every other bookmark is still cleaned up, so one
    /// permanently dead URL cannot wedge a scheduled service into a failed state
    /// forever. Three things do fail the run — a [`Halt`] (a dead credential or a rate
    /// limit), a write we could not make (the destination is what we sync *to*), and
    /// lookups failing in the *majority*, which is an outage wearing a per-item disguise.
    ///
    /// The majority test rather than "every lookup failed" because a `plan` failure can
    /// no longer be one dead link: every permanent per-item condition reaches the driver
    /// as a [`Plan`] (a deleted or blocked repo is [`Plan::Bookmark`], a URL the pass
    /// does not handle is [`Plan::Skipped`]), and a recognised rate limit is a [`Halt`].
    /// What is left in `plan_failed` is post-retry 5xx, network trouble, and throttling a
    /// source did not label — a handful is a blip worth riding out, but most of them
    /// failing means most of the work silently did not happen.
    ///
    /// The `max(1)` floor keeps that promise for a pass with a tiny population: the
    /// discussion-link pass strips its own marker tag, so it steadily looks up one or two
    /// bookmarks, and without the floor a single hiccup there would be a "majority" and
    /// would wedge the service all over again. One failed lookup is always survivable.
    /// Returns [`SourceError`] rather than a plain `anyhow::Error` so the *reason* still
    /// reaches `main`, which is what decides whether to fire the auth-failure hook — a
    /// halted credential must, a rate limit must not.
    pub fn into_result(self) -> Result<(), SourceError> {
        if let Some(halt) = &self.halted {
            // Why we stopped is the headline, but it must not bury what else went wrong
            // before it: those counts are the only record once this returns.
            let mut also = Vec::new();
            if self.plan_failed > 0 {
                also.push(format!("{} lookup(s)", self.plan_failed));
            }
            if self.write_failed > 0 {
                also.push(format!("{} write(s)", self.write_failed));
            }
            let message = match also.is_empty() {
                true => halt.message().to_string(),
                false => format!("{} (also failed: {})", halt.message(), also.join(", ")),
            };
            return Err(match halt {
                Halt::Reauth(_) => SourceError::ReauthRequired(message),
                Halt::RateLimited(_) => SourceError::RateLimited(message),
            });
        }
        if self.write_failed > 0 {
            return Err(anyhow!("{} bookmark(s) failed to write", self.write_failed).into());
        }
        if self.plan_failed > self.reached.max(1) {
            return Err(anyhow!(
                "{} of {} lookup(s) failed — the source looks unreachable",
                self.plan_failed,
                self.plan_failed + self.reached
            )
            .into());
        }
        if self.plan_failed > 0 {
            warn!(
                "{} bookmark(s) skipped after a failed lookup; the rest were cleaned up",
                self.plan_failed
            );
        }
        Ok(())
    }
}

/// Run `pass` over `bookmarks` (already filtered to the source). Re-writes each changed
/// bookmark (deleting the old URL when it changed), or prints the diff under the store's
/// dry-run. `noun` names the source in the scan log. Never fails outright: every failure,
/// including a dead credential, is tallied into the returned [`PassOutcome`], which the
/// caller turns into a verdict.
///
/// Residency is asked of `store`, whose view covers the *whole* account and reflects what
/// earlier passes in this run already wrote. Both halves matter: a plan can target a URL
/// outside its own filter — HackerNews rewrites a story bookmark to its article, which is
/// by definition not an HN item URL — and writing there is `replace=yes` over a record
/// this pass never planned; and `cleanup --all` runs three sources in turn over one
/// account, so a snapshot taken before the first would be stale for the others.
///
/// Two phases so that several bookmarks whose plans normalize to the *same* URL don't
/// clobber each other: phase 1 plans every bookmark (resolving date + privacy), phase 2
/// groups the plans by target URL and writes each group. A lone group is a normal
/// rewrite; a group of more than one is field-merged (see [`merge_bookmarks`]) into a
/// single bookmark that absorbs the others.
///
/// A rewrite's old URL is only deleted when it is not itself the target of another
/// planned write in the pass: if bookmark X normalizes onto a URL that is bookmark Y's
/// stored URL while Y moves elsewhere, Y's delete would otherwise remove the record X
/// just wrote there. Precomputing the set of all target URLs and refusing to delete any
/// of them makes the pass order-independent.
pub async fn run_pass<S: BookmarkStore + AccountState, C: CleanupPass>(
    store: &S,
    bookmarks: &[Bookmark],
    noun: &str,
    dates: DateOpts,
    pass: &C,
) -> PassOutcome {
    let dry_run = store.dry_run();
    // Dry-run output is consistently prefixed with `[dry-run]`.
    let dry_prefix = if dry_run { "[dry-run] " } else { "" };
    info!(
        "{dry_prefix}scanning {} {noun} bookmark(s)",
        bookmarks.len()
    );

    let now = OffsetDateTime::now_utc();
    let mut outcome = PassOutcome::default();

    let mut planned_pairs: Vec<(&Bookmark, Bookmark)> = Vec::new();
    // URLs whose record this pass could not establish — a lookup failed, or a halt stopped
    // it short. Nothing may be written there at all. Owned rather than borrowed because
    // the fixpoint below grows it from the group keys.
    let mut untouchable: HashMap<url::Url, Refusal> = HashMap::new();
    for (index, bookmark) in bookmarks.iter().enumerate() {
        let mut planned = match pass.plan(bookmark).await {
            Ok(Plan::Bookmark(p)) => {
                outcome.reached += 1;
                p
            }
            Ok(Plan::Unchanged) => {
                outcome.reached += 1;
                continue;
            }
            Ok(Plan::Skipped) => continue,
            // Every remaining lookup would fail the same way, so stop planning — but
            // fall through to write the plans already made rather than discard them.
            // This one and everything after it goes unplanned, so protect all of them:
            // a record we never looked at must not be written over.
            Err(SourceError::ReauthRequired(message)) => {
                outcome.halted = Some(Halt::Reauth(message));
                untouchable.extend(
                    bookmarks[index..]
                        .iter()
                        .map(|b| (b.url.clone(), Refusal::Unreadable)),
                );
                break;
            }
            Err(SourceError::RateLimited(message)) => {
                outcome.halted = Some(Halt::RateLimited(message));
                untouchable.extend(
                    bookmarks[index..]
                        .iter()
                        .map(|b| (b.url.clone(), Refusal::Unreadable)),
                );
                break;
            }
            // Log and skip a single failed plan so the rest of the pass still runs. Its
            // end-state is unknown, which makes it as untouchable as a skipped one —
            // another bookmark rewriting onto its URL would clobber the record.
            Err(e) => {
                outcome.plan_failed += 1;
                untouchable.insert(bookmark.url.clone(), Refusal::Unreadable);
                error!("looking up bookmark {}: {e:#}", bookmark.url);
                continue;
            }
        };

        // Resolve the final creation time here, at the write boundary: the candidate
        // source time when dating is on and within the cap, else "now"/preserve per the
        // policy. Comparable by instant to the stored `timestamp`.
        planned.timestamp = crate::timefmt::cleanup_date(
            dates.use_post_date,
            dates.max_age_days,
            dates.stale_to_now,
            planned.timestamp,
            now,
            bookmark.timestamp,
        );

        // Privacy flags are never re-shaped by cleanup: take them from the stored
        // bookmark so a plan can't silently flip them.
        planned.public = bookmark.public;
        planned.read_later = bookmark.read_later;

        planned_pairs.push((bookmark, planned));
    }

    // First-appearance order of the groups (and snapshot order within each group).
    let mut group_order: Vec<&url::Url> = Vec::new();
    let mut groups: HashMap<&url::Url, Vec<(&Bookmark, &Bookmark)>> = HashMap::new();
    for pair in &planned_pairs {
        let (original, planned) = (pair.0, &pair.1);
        let key = &planned.url;
        if !groups.contains_key(key) {
            group_order.push(key);
        }
        groups.entry(key).or_default().push((original, planned));
    }

    // The stored URLs of the bookmarks this pass actually planned. They are excluded from
    // residency: a bookmark that plans to stay at (or move within) its own URL would
    // otherwise find itself sitting there and merge its stored record back into its own
    // plan, undoing the very changes the plan computed.
    let planned_originals: std::collections::HashSet<&url::Url> = planned_pairs
        .iter()
        .map(|(original, _)| &original.url)
        .collect();

    // Every group's target URL. A rewrite's old URL is never deleted when it is one of
    // these, because some other planned write owns that URL; deleting it would clobber
    // that write's record. This keeps the pass order-independent.
    let targets: std::collections::HashSet<&url::Url> = group_order.iter().copied().collect();

    // Resolve every group's resident once, before any write. A resident is a record the
    // live view holds at a group's target that this pass neither planned (a bookmark is
    // never a resident of its own plan — merging with itself would resurrect what the plan
    // removed) nor failed to read. Precomputing is equivalent to looking each up lazily:
    // group keys are distinct, and every URL a write deletes was planned, so no write in
    // the loop below can change a record read here.
    let residents: HashMap<&url::Url, Bookmark> = group_order
        .iter()
        .filter(|key| !untouchable.contains_key(**key) && !planned_originals.contains(*key))
        .filter_map(|key| store.resident(key).map(|resident| (*key, resident)))
        .collect();

    // A merge fuses the members' notes and tags into one record, so a bookmark may only
    // join when it agrees with the record that stays there about who may read it. Widening
    // would publish a private member's annotation; narrowing would unshare a bookmark the
    // user chose to share. Neither is ours to decide, so the disagreeing bookmark drops out
    // of the group and stays where it is.
    //
    // Only that bookmark, not the whole group: the record that stays usually has a plan of
    // its own, which has nothing to do with the mover's visibility, and blocking that too
    // would let one mismatched duplicate freeze an unrelated bookmark's cleanup for good.
    // Settled here, ahead of the fixpoint, because a refused mover keeps its own URL
    // occupied exactly like a group refused for an unreadable target.
    let mut refused_movers: Vec<(url::Url, &url::Url, bool, bool)> = Vec::new();
    for key in &group_order {
        if untouchable.contains_key(*key) {
            continue;
        }
        let members = &groups[key];
        let staying = match residents.get(key) {
            Some(resident) => resident.public,
            None => match incumbent_of(members, key) {
                Some(at) => members[at].1.public,
                // Nothing is stored at this target, so there is no visibility to match and
                // the plans merge as peers.
                None => continue,
            },
        };
        let members = groups.get_mut(key).expect("key came from group_order");
        let (kept, refused): (Vec<_>, Vec<_>) = members
            .iter()
            .copied()
            .partition(|(_, planned)| planned.public == staying);
        if refused.is_empty() {
            continue;
        }
        refused_movers.extend(
            refused
                .iter()
                .map(|(original, planned)| (original.url.clone(), *key, planned.public, staying)),
        );
        *members = kept;
    }
    for (mover, target, mover_public, staying) in refused_movers {
        outcome.refused += 1;
        let describe = |public| if public { "public" } else { "private" };
        warn!(
            "not merging {mover} into {target}: it is {}, but {target} is {}",
            describe(mover_public),
            describe(staying)
        );
        if dry_run {
            println!("[dry-run] {mover}");
            println!(
                "          (refused: would merge into {target}, which is {})",
                describe(staying)
            );
        }
        // The mover didn't move, so its own URL is still occupied and no other rewrite may
        // land there.
        untouchable
            .entry(mover)
            .or_insert(Refusal::OccupiedByRefused);
    }

    // A group whose target is occupied won't move, so its *own* URL stays occupied too —
    // and a second group heading there would replace the record the refusal just saved.
    // Settled before any write so the outcome doesn't depend on group order, and iterated
    // to a fixpoint because each refusal can occupy the URL that refuses the next.
    loop {
        let newly_occupied: Vec<url::Url> = group_order
            .iter()
            .filter(|key| untouchable.contains_key(**key))
            .filter_map(|key| groups.get(*key))
            .flatten()
            .map(|(original, _)| original.url.clone())
            .filter(|url| !untouchable.contains_key(url))
            .collect();
        if newly_occupied.is_empty() {
            break;
        }
        untouchable.extend(
            newly_occupied
                .into_iter()
                .map(|url| (url, Refusal::OccupiedByRefused)),
        );
    }

    for key in group_order {
        let group = groups.remove(key).expect("key came from group_order");
        // The target must not be written, so leave the rewriting bookmark(s) at their old
        // URLs. Separate from the `targets` delete-guard below — that protects a URL some
        // *plan* writes to, this one a URL no plan may land on.
        if let Some(reason) = untouchable.get(key) {
            outcome.refused += 1;
            warn!("skipping cleanup write to {key}: {}", reason.explain());
            // Also on stdout: a dry run is a preview, and work the real run would refuse
            // is part of what the operator is previewing.
            if dry_run {
                println!("[dry-run] {key}");
                println!("          (refused: {})", reason.explain());
            }
            continue;
        }
        // Pinboard keys on URL, so the end state at this target has to be one record.
        // Anything already stored there joins the group rather than being replaced by it;
        // which member leads that merge is settled below.
        let resident = residents.get(key);
        if resident.is_none() && group.len() == 1 {
            let (original, planned) = group[0];
            // The written fields that differ; empty means nothing a write would change.
            let changes = original.diff(planned);
            if changes.is_empty() {
                continue;
            }
            // Delete the old URL only when it changed and no other planned write targets
            // it; otherwise that write's record lives there and must not be removed.
            let url_changed = planned.url != original.url;
            let delete_old = url_changed && !targets.contains(&original.url);

            if dry_run {
                println!("[dry-run] {}", original.url);
                for (label, value) in &changes {
                    println!("          {label:<6}-> {value}");
                }
            }

            // Always through the store, dry run included: it is what withholds the
            // network, and it is also what advances the view a later pass previews from.
            // `planned` carries the stored `public`/`read_later` and the driver-resolved
            // `timestamp`, so it's the complete write model.
            // Log and skip a single failed update so the rest of the pass still runs.
            let write = store
                .apply_update(planned, delete_old.then_some(&original.url))
                .await;
            match write.error {
                None => {
                    outcome.changed += 1;
                    debug!(
                        "updated {} -> {} [{}]",
                        original.url,
                        planned.url,
                        planned.tags.join(" ")
                    );
                }
                Some(e) => {
                    outcome.write_failed += 1;
                    error!("updating bookmark {}: {e:#}", original.url);
                    // The move didn't happen, so the record is still at its old URL and a
                    // later group heading there would overwrite it. The pre-write fixpoint
                    // can't know this, so protect it now — later groups only, which is why
                    // a failed write is still reported as a failure rather than a refusal.
                    if url_changed {
                        untouchable
                            .entry(original.url.clone())
                            .or_insert(Refusal::OccupiedByRefused);
                    }
                }
            }
            continue;
        }

        // Field-merge and delete the absorbed URLs so a later run sees a single bookmark at
        // the target and converges.
        // The record that stays at this URL: an unplanned resident, else the member already
        // stored here. Whichever it is leads the merge, so its title and note are the base
        // the others extend. For an incumbent that is its *plan*, not its stored record —
        // the plan is this pass's intended end-state for it, and merging its stored record
        // back in would resurrect exactly what the plan removed.
        let incumbent = incumbent_of(&group, key);
        // The tail filter below drops index `incumbent` unconditionally, which would lose a
        // member outright if both could be `Some`. They can't — an incumbent's stored URL is
        // this key, so the key is in `planned_originals`, which `residents` excludes — but
        // that proof spans a hundred lines, so assert it where it is relied on.
        debug_assert!(resident.is_none() || incumbent.is_none());
        let survivor = resident.or_else(|| incumbent.map(|at| group[at].1));
        let plans: Vec<&Bookmark> = survivor
            .into_iter()
            .chain(
                group
                    .iter()
                    .enumerate()
                    .filter(|(at, _)| Some(*at) != incumbent)
                    .map(|(_, (_, planned))| *planned),
            )
            .collect();
        let mut merged = merge_bookmarks(&plans);
        if let Some(survivor) = survivor {
            // The survivor's own record-level state survives the merge's field rules: it is
            // the bookmark that stays here, and cleanup never re-shapes those (see the
            // single-plan path above). Its date in particular must not shift to a mover's
            // under `merge_bookmarks`'s earliest-wins rule, which is meant for two plans —
            // with dating off, nothing may re-date a bookmark at all. `public` needs no
            // such restore: a group only reaches this point when every member already
            // matches it, so the all()-rule cannot change it.
            merged.read_later = survivor.read_later;
            // `None` means the stored date was absent or wouldn't parse, and writing it
            // back omits `dt` entirely — under `replace=yes` Pinboard then re-dates the
            // record to now. Keep the earliest date the merge found instead of losing the
            // record's age; before this path such a bookmark was refused, never written.
            merged.timestamp = survivor.timestamp.or(merged.timestamp);
        }
        let target = &merged.url;

        // Absorbed URLs to delete: an original that is neither the target nor another
        // planned write's target (deleting the latter would clobber that write's record).
        let mut old_urls: Vec<&url::Url> = Vec::new();
        for (original, _) in &group {
            if original.url != *target
                && !targets.contains(&original.url)
                && !old_urls.contains(&&original.url)
            {
                old_urls.push(&original.url);
            }
        }

        // Nothing to do when the merge reproduces what is already stored at the target and
        // there is nothing to absorb — otherwise a converged state would rewrite itself
        // every run. Compared against the *stored* record, which for an incumbent is not
        // the plan that led the merge.
        let stored_at_target = resident.or_else(|| incumbent.map(|at| group[at].0));
        if stored_at_target.is_some_and(|stored| stored.diff(&merged).is_empty())
            && old_urls.is_empty()
        {
            continue;
        }

        if dry_run {
            println!("[dry-run] {target}");
            println!("          {:<6}-> {}", "title", merged.title);
            let note_value = if merged.note.is_empty() {
                "(removed)"
            } else {
                &merged.note
            };
            println!("          {:<6}-> {note_value}", "notes");
            println!("          {:<6}-> [{}]", "tags", merged.tags.join(" "));
            if let Some(date) = merged.timestamp.and_then(crate::timefmt::to_rfc3339) {
                println!("          {:<6}-> {date}", "date");
            }
            println!("          {:<6}-> {}", "public", merged.public);
            println!("          {:<6}-> {}", "toread", merged.read_later);
            for old in &old_urls {
                println!("          absorb {old}");
            }
        }

        // Always through the store — see the single-plan path above.
        let write = store.apply_merge(&merged, &old_urls).await;
        match write.error {
            None => {
                outcome.changed += 1;
                let absorbed: Vec<String> = write.deleted.iter().map(|u| u.to_string()).collect();
                debug!("merged {target} <- [{}]", absorbed.join(" "));
            }
            // A merge can half-apply: the record is written and an absorbed URL survives.
            // Say which, because "updating {target} failed" would send the operator after
            // a record that is actually fine.
            Some(e) if write.wrote => {
                outcome.write_failed += 1;
                let stranded = old_urls.len() - write.deleted.len();
                error!("merged {target} but {stranded} absorbed URL(s) remain: {e:#}");
            }
            Some(e) => {
                outcome.write_failed += 1;
                error!("merging into {target}: {e:#}");
                // Nothing was written and nothing absorbed, so every member is still at its
                // own URL — see the single-plan path above.
                for old in &old_urls {
                    untouchable
                        .entry((*old).clone())
                        .or_insert(Refusal::OccupiedByRefused);
                }
            }
        }
    }

    if dry_run {
        println!("{dry_prefix}{} bookmark(s) would change.", outcome.changed);
    } else {
        info!("done: updated {} bookmark(s)", outcome.changed);
    }
    if outcome.refused > 0 {
        warn!(
            "{} rewrite(s) left in place: their target holds a bookmark this pass must not overwrite",
            outcome.refused
        );
    }
    outcome
}

/// The index within `members` of the group's *incumbent*: the member whose stored URL is
/// already the group's target. It is the record that stays at that URL, so it plays the
/// same role a resident does — its plan leads the merge, and its own reading state and date
/// survive the merge's field rules. A resident is by definition unplanned, so a group has
/// at most one of the two, and being inside the pass's slice is an implementation detail
/// that must not decide whether a bookmark is protected.
fn incumbent_of(members: &[(&Bookmark, &Bookmark)], target: &url::Url) -> Option<usize> {
    members
        .iter()
        .position(|(original, _)| original.url == *target)
}

/// Field-merge the PLANNED bookmarks of a collision group (all sharing `url`, given in
/// stable order) into one bookmark. Tags are an order-preserving union (first occurrence
/// wins, case-sensitive); the note is the first member's verbatim, extended with whichever
/// blank-line-separated blocks of the later members it doesn't already hold; title is the
/// first non-empty; timestamp the earliest present. Privacy never widens:
/// the merge is `public` only if every merged member is public (a single private member
/// keeps it private), and `read_later` if any member is, so a merge can never republish a
/// private bookmark as public.
fn merge_bookmarks(group: &[&Bookmark]) -> Bookmark {
    let url = group[0].url.clone();

    let mut tags: Vec<String> = Vec::new();
    for b in group {
        for tag in &b.tags {
            if !tags.contains(tag) {
                tags.push(tag.clone());
            }
        }
    }

    // The first member's note is the base, carried byte for byte: it is the record that
    // stays, and its spacing is the user's. Later members contribute only the
    // blank-line-separated blocks the base doesn't already have. Blocks are compared
    // trimmed so re-merging an already-merged note is a no-op even when the base ends in a
    // newline, which otherwise re-splits as a block the plain comparison can't recognise —
    // and any group whose old URL is never absorbed re-merges every run. One case still
    // escapes it: a note long enough that `PinboardClient::fit_extended` trims it comes
    // back as a fragment no block matches, and re-merges forever. That is why the trim is
    // warned about rather than silent.
    let mut note = group[0].note.clone();
    for b in &group[1..] {
        for block in b.note.split("\n\n") {
            let candidate = block.trim();
            if candidate.is_empty() || note.split("\n\n").any(|have| have.trim() == candidate) {
                continue;
            }
            if !note.is_empty() {
                note.push_str("\n\n");
            }
            // The raw block, not the trimmed candidate: its indentation is the user's
            // writing (a 4-space Markdown code block, say), and Pinboard renders the note
            // as markup. Only the *comparison* trims.
            note.push_str(block);
        }
    }

    let title = group
        .iter()
        .map(|b| b.title.as_str())
        .find(|t| !t.is_empty())
        .unwrap_or_default()
        .to_string();

    let timestamp = group.iter().filter_map(|b| b.timestamp).min();

    Bookmark {
        url,
        title,
        note,
        tags,
        timestamp,
        public: group.iter().all(|b| b.public),
        read_later: group.iter().any(|b| b.read_later),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bookmark::{AccountState, AccountView, CleanupStore};
    use crate::test_support::FakePinboard;
    use anyhow::anyhow;
    use url::Url;

    fn u(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    /// Wrap the fake in the production store so every test exercises the same view
    /// bookkeeping the real run does, rather than a stand-in that can drift from it.
    fn store<'a>(
        pinboard: &'a FakePinboard,
        all: &[Bookmark],
        dry_run: bool,
    ) -> CleanupStore<'a, FakePinboard> {
        CleanupStore::new(pinboard, AccountView::new(all.to_vec()), dry_run)
    }

    /// A `CleanupPass` whose `plan` is a closure, so each test scripts its own outcome.
    struct FakePass<F>(F);
    impl<F: Fn(&Bookmark) -> Result<Plan, SourceError>> CleanupPass for FakePass<F> {
        async fn plan(&self, bookmark: &Bookmark) -> Result<Plan, SourceError> {
            (self.0)(bookmark)
        }
    }

    /// A per-item read failure: the kind that must not fail the whole run.
    fn item_error() -> SourceError {
        SourceError::Other(anyhow!("boom"))
    }

    fn bookmark(url: &str) -> Bookmark {
        Bookmark {
            url: u(url),
            title: "Title".into(),
            note: "notes".into(),
            tags: vec!["a".into(), "b".into()],
            timestamp: crate::timefmt::from_unix(1_577_836_800), // 2020-01-01T00:00:00Z
            public: false,
            read_later: false,
        }
    }

    /// A plan identical to `bookmark` (the driver should treat it as unchanged under
    /// [`NO_DATING`], which leaves the stored date intact).
    fn unchanged_plan(bookmark: &Bookmark) -> Bookmark {
        bookmark.clone()
    }

    /// Dating off: the driver preserves each bookmark's existing date.
    const NO_DATING: DateOpts = DateOpts {
        use_post_date: false,
        max_age_days: 0,
        stale_to_now: false,
    };

    #[tokio::test]
    async fn err_plan_counts_failed_and_continues() {
        // The first bookmark's plan fails; the second still gets written.
        let books = vec![bookmark("https://x/bad"), bookmark("https://x/good")];
        let pass = FakePass(|bookmark: &Bookmark| {
            if bookmark.url.as_str().contains("bad") {
                Err(item_error())
            } else {
                Ok(Plan::Bookmark(Bookmark {
                    title: "New".into(),
                    ..unchanged_plan(bookmark)
                }))
            }
        });
        let pinboard = FakePinboard::default();

        let outcome = run_pass(
            &store(&pinboard, &books, false),
            &books,
            "test",
            NO_DATING,
            &pass,
        )
        .await;
        assert_eq!(outcome.plan_failed, 1);
        assert_eq!(outcome.write_failed, 0);
        let updated = pinboard.updated.borrow();
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].url, "https://x/good");
        assert_eq!(updated[0].description, "New");
    }

    #[tokio::test]
    async fn one_unreachable_link_does_not_fail_the_run() {
        // The heart of the policy: a single link we couldn't look up is logged, but the
        // run still succeeds, because everything we *could* do, we did. The good ones
        // outnumber it, so this stays clear of the majority-failure guard.
        let books = vec![
            bookmark("https://x/bad"),
            bookmark("https://x/good-1"),
            bookmark("https://x/good-2"),
        ];
        let pass = FakePass(|bookmark: &Bookmark| {
            if bookmark.url.as_str().contains("bad") {
                Err(item_error())
            } else {
                Ok(Plan::Bookmark(unchanged_plan(bookmark)))
            }
        });
        let pinboard = FakePinboard::default();

        let outcome = run_pass(
            &store(&pinboard, &books, false),
            &books,
            "test",
            NO_DATING,
            &pass,
        )
        .await;
        assert!(outcome.into_result().is_ok());
    }

    #[tokio::test]
    async fn a_failed_write_fails_the_run() {
        // The destination is what we cannot sync to, so this one is fatal.
        let books = vec![bookmark("https://x/")];
        let pass = FakePass(|bookmark: &Bookmark| {
            Ok(Plan::Bookmark(Bookmark {
                title: "New".into(),
                ..unchanged_plan(bookmark)
            }))
        });
        let mut pinboard = FakePinboard::default();
        pinboard.fail_update_urls.insert("https://x/".into());

        let outcome = run_pass(
            &store(&pinboard, &books, false),
            &books,
            "test",
            NO_DATING,
            &pass,
        )
        .await;
        assert_eq!(outcome.write_failed, 1);
        assert!(pinboard.updated.borrow().is_empty());
        let err = outcome.into_result().unwrap_err();
        assert!(err.to_string().contains("failed to write"), "{err}");
    }

    #[tokio::test]
    async fn every_plan_failing_fails_the_run() {
        // Not one bad link but an unreachable source: nothing got through, so the run
        // must not report success.
        let books = vec![bookmark("https://x/one"), bookmark("https://x/two")];
        let pass = FakePass(|_: &Bookmark| Err(item_error()));
        let pinboard = FakePinboard::default();

        let outcome = run_pass(
            &store(&pinboard, &books, false),
            &books,
            "test",
            NO_DATING,
            &pass,
        )
        .await;
        assert_eq!(outcome.plan_failed, 2);
        let err = outcome.into_result().unwrap_err();
        assert!(err.to_string().contains("2 of 2"), "{err}");
    }

    #[tokio::test]
    async fn a_lone_failed_lookup_never_fails_the_run() {
        // A pass can have a tiny population — `cleanup hackernews --link-discussions`
        // strips its own marker tag, so in the steady state it looks up one or two
        // bookmarks. One transient hiccup there must not fail the run, or a single bad
        // lookup wedges the scheduled service exactly as before.
        let books = vec![bookmark("https://x/only")];
        let pass = FakePass(|_: &Bookmark| Err(item_error()));
        let pinboard = FakePinboard::default();

        let outcome = run_pass(
            &store(&pinboard, &books, false),
            &books,
            "test",
            NO_DATING,
            &pass,
        )
        .await;
        assert_eq!((outcome.reached, outcome.plan_failed), (0, 1));
        assert!(outcome.into_result().is_ok());
    }

    #[tokio::test]
    async fn a_majority_of_lookups_failing_fails_the_run() {
        // Not one bad link but widespread trouble — a rate limit or an outage part-way
        // through the run. Some lookups got through, so the source isn't flat down, but
        // reporting success would hide most of the work never happening.
        let books = vec![
            bookmark("https://x/ok"),
            bookmark("https://x/bad-1"),
            bookmark("https://x/bad-2"),
            bookmark("https://x/bad-3"),
        ];
        let pass = FakePass(|bookmark: &Bookmark| {
            if bookmark.url.as_str().contains("bad") {
                Err(item_error())
            } else {
                Ok(Plan::Unchanged)
            }
        });
        let pinboard = FakePinboard::default();

        let outcome = run_pass(
            &store(&pinboard, &books, false),
            &books,
            "test",
            NO_DATING,
            &pass,
        )
        .await;
        assert_eq!((outcome.reached, outcome.plan_failed), (1, 3));
        let err = outcome.into_result().unwrap_err();
        assert!(err.to_string().contains("3 of 4"), "{err}");
    }

    #[tokio::test]
    async fn a_rate_limit_stops_the_pass_like_a_dead_credential() {
        // Being rate limited fails every remaining lookup just as a dead credential does,
        // so stop rather than spend one doomed request per bookmark — while still writing
        // the plans already made.
        let books = vec![
            bookmark("https://x/one"),
            bookmark("https://x/limited"),
            bookmark("https://x/never"),
        ];
        let planned = std::cell::Cell::new(0usize);
        let pass = FakePass(|bookmark: &Bookmark| {
            planned.set(planned.get() + 1);
            if bookmark.url.as_str().ends_with("one") {
                return Ok(Plan::Bookmark(Bookmark {
                    title: "New".into(),
                    ..unchanged_plan(bookmark)
                }));
            }
            Err(SourceError::RateLimited("resets at 14:23".into()))
        });
        let pinboard = FakePinboard::default();

        let outcome = run_pass(
            &store(&pinboard, &books, false),
            &books,
            "test",
            NO_DATING,
            &pass,
        )
        .await;
        assert_eq!(planned.get(), 2, "should stop at the rate limit");
        assert_eq!(
            outcome.halted,
            Some(Halt::RateLimited("resets at 14:23".into()))
        );
        assert_eq!(pinboard.updated.borrow().len(), 1);
        let err = outcome.into_result().unwrap_err().to_string();
        assert!(err.contains("resets at 14:23"), "{err}");
    }

    #[tokio::test]
    async fn reauth_stops_planning_but_still_writes_what_was_planned() {
        // A dead credential fails every remaining lookup, so stop rather than burn a
        // request per bookmark — but the plans already made are good, so write them
        // rather than throwing the work away.
        let books = vec![
            bookmark("https://x/one"),
            bookmark("https://x/two"),
            bookmark("https://x/three"),
        ];
        let planned = std::cell::Cell::new(0usize);
        let pass = FakePass(|bookmark: &Bookmark| {
            planned.set(planned.get() + 1);
            if bookmark.url.as_str().ends_with("one") {
                return Ok(Plan::Bookmark(Bookmark {
                    title: "New".into(),
                    ..unchanged_plan(bookmark)
                }));
            }
            Err(SourceError::ReauthRequired("token expired".into()))
        });
        let pinboard = FakePinboard::default();

        let outcome = run_pass(
            &store(&pinboard, &books, false),
            &books,
            "test",
            NO_DATING,
            &pass,
        )
        .await;
        assert_eq!(planned.get(), 2, "should stop at the reauth failure");
        let updated = pinboard.updated.borrow();
        assert_eq!(
            updated.len(),
            1,
            "the plan made before it died is still written"
        );
        assert_eq!(updated[0].url, "https://x/one");
        let err = outcome.into_result().unwrap_err();
        assert!(err.to_string().contains("token expired"), "{err}");
    }

    #[tokio::test]
    async fn a_reauth_alongside_failed_writes_reports_both() {
        // Planning stops at the dead credential but the earlier plans are still written,
        // so both failures can be real. Reporting only the credential would hide that
        // writes were also being rejected.
        let books = vec![bookmark("https://x/one"), bookmark("https://x/dead")];
        let pass = FakePass(|bookmark: &Bookmark| {
            if bookmark.url.as_str().ends_with("dead") {
                return Err(SourceError::ReauthRequired("token expired".into()));
            }
            Ok(Plan::Bookmark(Bookmark {
                title: "New".into(),
                ..unchanged_plan(bookmark)
            }))
        });
        let mut pinboard = FakePinboard::default();
        pinboard.fail_update_urls.insert("https://x/one".into());

        let outcome = run_pass(
            &store(&pinboard, &books, false),
            &books,
            "test",
            NO_DATING,
            &pass,
        )
        .await;
        assert_eq!(outcome.write_failed, 1);
        let err = outcome.into_result().unwrap_err().to_string();
        assert!(err.contains("token expired"), "{err}");
        assert!(err.contains("1 write(s)"), "{err}");
    }

    #[tokio::test]
    async fn a_reauth_does_not_hide_lookups_that_already_failed() {
        // Ten lookups 5xx'd before the credential died; reporting only the credential
        // would lose the fact that the source was already misbehaving.
        let books = vec![
            bookmark("https://x/bad"),
            bookmark("https://x/dead"),
            bookmark("https://x/never"),
        ];
        let pass = FakePass(|bookmark: &Bookmark| {
            if bookmark.url.as_str().ends_with("dead") {
                return Err(SourceError::ReauthRequired("token expired".into()));
            }
            Err(item_error())
        });
        let pinboard = FakePinboard::default();

        let outcome = run_pass(
            &store(&pinboard, &books, false),
            &books,
            "test",
            NO_DATING,
            &pass,
        )
        .await;
        assert_eq!(outcome.plan_failed, 1);
        let err = outcome.into_result().unwrap_err().to_string();
        assert!(err.contains("token expired"), "{err}");
        assert!(err.contains("1 lookup(s)"), "{err}");
    }

    #[tokio::test]
    async fn bookmarks_a_pass_skipped_do_not_count_as_reaching_the_source() {
        // A skipped bookmark never touched the source, so it must not dilute the ratio:
        // `cleanup github` skips deep links and gists without an API call, and those must
        // not vouch for a source that is actually down. Two failures, to clear the
        // one-failure floor and isolate what's being tested here.
        let books = vec![
            bookmark("https://x/not-mine-1"),
            bookmark("https://x/not-mine-2"),
            bookmark("https://x/not-mine-3"),
            bookmark("https://x/mine-1"),
            bookmark("https://x/mine-2"),
        ];
        let pass = FakePass(|bookmark: &Bookmark| {
            if bookmark.url.as_str().contains("not-mine") {
                Ok(Plan::Skipped)
            } else {
                Err(item_error())
            }
        });
        let pinboard = FakePinboard::default();

        let outcome = run_pass(
            &store(&pinboard, &books, false),
            &books,
            "test",
            NO_DATING,
            &pass,
        )
        .await;
        assert_eq!(outcome.reached, 0);
        // "2 of 2", not "2 of 5": the three skipped bookmarks are not in the denominator,
        // which is what stops them padding it into a passing ratio.
        let err = outcome.into_result().unwrap_err();
        assert!(err.to_string().contains("2 of 2"), "{err}");
    }

    #[tokio::test]
    async fn an_unchanged_plan_counts_as_reaching_the_source() {
        // The pass looked this one up and found nothing to do — that answer proves the
        // source is up, so a separate failure is just one bad link.
        let books = vec![bookmark("https://x/looked-up"), bookmark("https://x/bad")];
        let pass = FakePass(|bookmark: &Bookmark| {
            if bookmark.url.as_str().contains("bad") {
                Err(item_error())
            } else {
                Ok(Plan::Unchanged)
            }
        });
        let pinboard = FakePinboard::default();

        let outcome = run_pass(
            &store(&pinboard, &books, false),
            &books,
            "test",
            NO_DATING,
            &pass,
        )
        .await;
        assert_eq!(outcome.reached, 1);
        assert!(outcome.into_result().is_ok());
    }

    #[tokio::test]
    async fn none_plan_is_skipped() {
        let books = vec![bookmark("https://x/")];
        let pass = FakePass(|_: &Bookmark| Ok(Plan::Skipped));
        let pinboard = FakePinboard::default();

        let outcome = run_pass(
            &store(&pinboard, &books, false),
            &books,
            "test",
            NO_DATING,
            &pass,
        )
        .await;
        assert_eq!(outcome.plan_failed + outcome.write_failed, 0);
        assert!(pinboard.updated.borrow().is_empty());
        assert!(pinboard.deleted.borrow().is_empty());
    }

    #[tokio::test]
    async fn unchanged_plan_is_skipped() {
        let books = vec![bookmark("https://x/")];
        let pass = FakePass(|bookmark: &Bookmark| Ok(Plan::Bookmark(unchanged_plan(bookmark))));
        let pinboard = FakePinboard::default();

        let outcome = run_pass(
            &store(&pinboard, &books, false),
            &books,
            "test",
            NO_DATING,
            &pass,
        )
        .await;
        assert_eq!(outcome.plan_failed + outcome.write_failed, 0);
        assert!(pinboard.updated.borrow().is_empty());
    }

    #[tokio::test]
    async fn url_change_updates_and_deletes_old_preserving_privacy() {
        let books = vec![bookmark("https://old/")];
        let pass = FakePass(|bookmark: &Bookmark| {
            Ok(Plan::Bookmark(Bookmark {
                url: u("https://new/"),
                title: "New".into(),
                tags: vec!["x".into()],
                ..unchanged_plan(bookmark)
            }))
        });
        let pinboard = FakePinboard::default();

        let outcome = run_pass(
            &store(&pinboard, &books, false),
            &books,
            "test",
            NO_DATING,
            &pass,
        )
        .await;
        assert_eq!(outcome.plan_failed + outcome.write_failed, 0);
        let updated = pinboard.updated.borrow();
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].url, "https://new/");
        // shared/toread are carried over from the stored bookmark (both off).
        assert!(!updated[0].shared && !updated[0].toread);
        // The old URL is deleted after the rewrite.
        assert_eq!(*pinboard.deleted.borrow(), vec!["https://old/".to_string()]);
    }

    #[test]
    fn merge_bookmarks_is_idempotent_for_notes() {
        // Merging is not a one-shot: a group whose old URL can't be absorbed re-merges
        // every run, so feeding a merged note back in must not append the same block
        // again. Whole-string dedup missed this and grew the note without bound.
        let mut resident = bookmark("https://t/");
        resident.note = "R".into();
        let mut mover = bookmark("https://t/");
        mover.note = "M".into();

        let once = merge_bookmarks(&[&resident, &mover]);
        assert_eq!(once.note, "R\n\nM");

        // Second run: the resident now holds the merged note, the mover still holds "M".
        let twice = merge_bookmarks(&[&once, &mover]);
        assert_eq!(twice.note, "R\n\nM", "the note must not grow on re-merge");
    }

    #[test]
    fn merge_bookmarks_keeps_the_first_note_byte_for_byte() {
        // The first member is the record that stays, so its note is carried verbatim: a
        // hand-written note's own spacing is the user's, not ours to normalize. Splitting
        // it into blocks and rejoining them collapsed a deliberate run of blank lines and
        // dropped its trailing whitespace.
        let mut resident = bookmark("https://t/");
        resident.note = "one\n\n\n\ntwo  ".into();
        let mut mover = bookmark("https://t/");
        mover.note = "three".into();

        let once = merge_bookmarks(&[&resident, &mover]);
        assert_eq!(once.note, "one\n\n\n\ntwo  \n\nthree");

        // And re-merging that unusual shape still has to settle, or the note grows every
        // run for any group whose old URL is never absorbed.
        let twice = merge_bookmarks(&[&once, &mover]);
        assert_eq!(twice.note, once.note, "the note must not grow on re-merge");
    }

    #[test]
    fn merge_bookmarks_keeps_a_later_members_block_verbatim() {
        // Carrying the first member's note byte for byte is only half of it: an absorbed
        // member's blocks are the user's writing too. Comparing them trimmed is what makes
        // re-merging settle, but appending them trimmed dedents a Markdown code block —
        // Pinboard renders notes as markup, so that changes what the note says.
        let mut resident = bookmark("https://t/");
        resident.note = "A".into();
        let mut mover = bookmark("https://t/");
        mover.note = "    let x = 1;\n    let y = 2;".into();

        let once = merge_bookmarks(&[&resident, &mover]);
        assert_eq!(once.note, "A\n\n    let x = 1;\n    let y = 2;");
        let twice = merge_bookmarks(&[&once, &mover]);
        assert_eq!(twice.note, once.note, "the note must not grow on re-merge");
    }

    #[test]
    fn merge_bookmarks_settles_when_a_note_ends_in_a_newline() {
        // A base ending in a newline puts three newlines before the appended block, which
        // splits as a block with a leading newline rather than the block itself. Comparing
        // blocks trimmed is what keeps that from re-appending forever.
        let mut resident = bookmark("https://t/");
        resident.note = "base\n".into();
        let mut mover = bookmark("https://t/");
        mover.note = "extra".into();

        let once = merge_bookmarks(&[&resident, &mover]);
        assert_eq!(once.note, "base\n\n\nextra");
        let twice = merge_bookmarks(&[&once, &mover]);
        assert_eq!(twice.note, once.note, "the note must not grow on re-merge");
    }

    #[test]
    fn merge_bookmarks_applies_field_rules() {
        let ts_early = crate::timefmt::from_unix(1_000);
        let ts_late = crate::timefmt::from_unix(2_000);
        let a = Bookmark {
            url: u("https://collide/"),
            title: String::new(),
            note: "from A".into(),
            tags: vec!["x".into(), "shared".into()],
            timestamp: ts_late,
            public: false,
            read_later: true,
        };
        let b = Bookmark {
            url: u("https://collide/"),
            title: "Title B".into(),
            note: "from B".into(),
            tags: vec!["shared".into(), "y".into()],
            timestamp: ts_early,
            public: true,
            read_later: false,
        };
        let c = Bookmark {
            url: u("https://collide/"),
            title: "Title C".into(),
            // A duplicate of B's note must not be repeated in the join.
            note: "from B".into(),
            tags: vec!["z".into()],
            timestamp: None,
            public: false,
            read_later: false,
        };

        let merged = merge_bookmarks(&[&a, &b, &c]);
        assert_eq!(merged.url.as_str(), "https://collide/");
        // Order-preserving, case-sensitive union across the group.
        assert_eq!(merged.tags, vec!["x", "shared", "y", "z"]);
        // Distinct non-empty notes in order, joined by a blank line.
        assert_eq!(merged.note, "from A\n\nfrom B");
        // First non-empty title.
        assert_eq!(merged.title, "Title B");
        // Earliest present timestamp.
        assert_eq!(merged.timestamp, ts_early);
        // Privacy never widens: A is private, so the merge is private even though B is public.
        assert!(!merged.public);
        // read_later is the OR across the group: A wants it, so the merge does too.
        assert!(merged.read_later);
    }

    #[test]
    fn merge_bookmarks_passes_a_single_note_through() {
        let a = Bookmark {
            note: "only note".into(),
            ..bookmark("https://collide/")
        };
        let b = Bookmark {
            note: String::new(),
            ..bookmark("https://collide/")
        };
        let merged = merge_bookmarks(&[&a, &b]);
        assert_eq!(merged.note, "only note");
    }

    #[test]
    fn merge_bookmarks_public_absorbing_private_stays_private() {
        // A public member absorbing a private one must not leak the private content as
        // public: privacy is the AND across the group, so one private member wins.
        let public = Bookmark {
            public: true,
            ..bookmark("https://collide/")
        };
        let private = Bookmark {
            public: false,
            ..bookmark("https://collide/")
        };
        assert!(!merge_bookmarks(&[&public, &private]).public);
        // Order-independent: private first is equally private.
        assert!(!merge_bookmarks(&[&private, &public]).public);
    }

    #[tokio::test]
    async fn colliding_rewrites_are_field_merged() {
        // Two stored bookmarks whose plans land on the same URL: A normalizes onto B's URL
        // and B stays put. The driver must field-merge them into one record at that URL and
        // delete A's old URL, rather than clobbering either.
        let mut stored_a = bookmark("https://old-a/");
        stored_a.tags = vec!["x".into()];
        stored_a.note = "from A".into();
        let mut stored_b = bookmark("https://collide/");
        stored_b.tags = vec!["y".into()];
        stored_b.note = "from B".into();
        let books = vec![stored_a, stored_b];

        let pass = FakePass(|bookmark: &Bookmark| {
            if bookmark.url.as_str().contains("old-a") {
                Ok(Plan::Bookmark(Bookmark {
                    url: u("https://collide/"),
                    ..unchanged_plan(bookmark)
                }))
            } else {
                Ok(Plan::Bookmark(unchanged_plan(bookmark)))
            }
        });
        let pinboard = FakePinboard::default();

        let outcome = run_pass(
            &store(&pinboard, &books, false),
            &books,
            "test",
            NO_DATING,
            &pass,
        )
        .await;
        assert_eq!(outcome.plan_failed + outcome.write_failed, 0);

        let updated = pinboard.updated.borrow();
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].url, "https://collide/");
        // B leads the union, not A: B is the record that stays at this URL, so it is the
        // base the others extend. A led only because it happened to come first in the
        // slice, which is an ordering the user has no way to see or control.
        assert_eq!(updated[0].tags, vec!["y".to_string(), "x".to_string()]);
        assert_eq!(updated[0].extended, "from B\n\nfrom A");
        // Only A's old URL is absorbed; the shared target is not deleted.
        assert_eq!(
            *pinboard.deleted.borrow(),
            vec!["https://old-a/".to_string()]
        );
    }

    #[tokio::test]
    async fn chained_rewrite_does_not_delete_another_writes_target() {
        // X at old-a rewrites to collide-b; Y at collide-b rewrites to new-z. X's target is
        // Y's stored URL, so Y's post-rewrite delete of collide-b would clobber X's record.
        // The pass must leave collide-b (a planned target) undeleted, in either order.
        async fn run_in_order(books: Vec<Bookmark>) {
            let pass = FakePass(|bookmark: &Bookmark| {
                let next = if bookmark.url.as_str().contains("old-a") {
                    "https://collide-b/"
                } else {
                    "https://new-z/"
                };
                Ok(Plan::Bookmark(Bookmark {
                    url: u(next),
                    ..unchanged_plan(bookmark)
                }))
            });
            let pinboard = FakePinboard::default();

            let outcome = run_pass(
                &store(&pinboard, &books, false),
                &books,
                "test",
                NO_DATING,
                &pass,
            )
            .await;
            assert_eq!(outcome.plan_failed + outcome.write_failed, 0);

            let updated = pinboard.updated.borrow();
            let mut written: Vec<&str> = updated.iter().map(|c| c.url.as_str()).collect();
            written.sort_unstable();
            assert_eq!(written, vec!["https://collide-b/", "https://new-z/"]);

            let deleted = pinboard.deleted.borrow();
            // old-a is deleted (no other write targets it); collide-b is X's target and
            // must survive Y's rewrite.
            assert!(deleted.contains(&"https://old-a/".to_string()));
            assert!(!deleted.contains(&"https://collide-b/".to_string()));
        }

        let x = bookmark("https://old-a/");
        let y = bookmark("https://collide-b/");
        run_in_order(vec![x.clone(), y.clone()]).await;
        run_in_order(vec![y, x]).await;
    }

    #[tokio::test]
    async fn a_refused_rewrite_leaves_its_own_record_protected() {
        // C's lookup fails, so C is untouchable. A wants C's URL and is refused — meaning
        // A stays put, so A's own URL is occupied after all, and B's rewrite onto it would
        // replace the very record A's refusal just saved. The protection has to propagate.
        let mut a = bookmark("https://x/a");
        a.note = "A's notes".into();
        let b = bookmark("https://x/b");
        let c = bookmark("https://x/c");
        let slice = vec![a.clone(), b.clone(), c.clone()];
        let all = vec![a, b, c];

        let pass = FakePass(|bookmark: &Bookmark| {
            let target = match bookmark.url.as_str() {
                url if url.ends_with("/a") => "https://x/c",
                url if url.ends_with("/b") => "https://x/a",
                _ => return Err(item_error()),
            };
            Ok(Plan::Bookmark(Bookmark {
                url: u(target),
                ..unchanged_plan(bookmark)
            }))
        });
        let pinboard = FakePinboard::default();

        let outcome = run_pass(
            &store(&pinboard, &all, false),
            &slice,
            "test",
            NO_DATING,
            &pass,
        )
        .await;
        assert_eq!(outcome.refused, 2, "both rewrites must be refused");
        assert!(
            pinboard.updated.borrow().is_empty(),
            "A's record must survive its own refusal: {:?}",
            pinboard.updated.borrow()
        );
    }

    #[tokio::test]
    async fn a_matching_public_group_merges_and_stays_public() {
        // The mirror of the refusal: when every member agrees with the resident, the merge
        // runs and neither flag moves. This is what lets the converged-state guard use
        // `diff`, which ignores `public`/`read_later` — those two cannot differ here, so
        // there is nothing for it to be blind to.
        let mut mover = bookmark("https://x/mine");
        mover.public = true;
        mover.read_later = true;
        mover.note = "shared annotation".into();
        let mut resident = bookmark("https://other/resident");
        resident.public = true;
        resident.read_later = false;
        resident.note = "resident note".into();
        let slice = vec![mover.clone()];
        let all = vec![mover, resident];

        let pass = FakePass(|bookmark: &Bookmark| {
            Ok(Plan::Bookmark(Bookmark {
                url: u("https://other/resident"),
                ..unchanged_plan(bookmark)
            }))
        });
        let pinboard = FakePinboard::default();

        let outcome = run_pass(
            &store(&pinboard, &all, false),
            &slice,
            "test",
            NO_DATING,
            &pass,
        )
        .await;
        assert_eq!(outcome.refused, 0);
        let updated = pinboard.updated.borrow();
        assert_eq!(updated.len(), 1);
        assert!(updated[0].extended.contains("shared annotation"));
        assert!(updated[0].shared, "a matching group must stay public");
        assert!(
            !updated[0].toread,
            "the resident's reading state survives the merge"
        );
    }

    #[tokio::test]
    async fn a_converged_incumbent_merge_is_not_rewritten_every_run() {
        // The incumbent half of the converged-state guard. The bookmark at the target
        // already holds everything the merge produces, and the mover's old URL is another
        // plan's target so it can't be absorbed — so the group re-merges every run and must
        // recognise that there is nothing left to write.
        let mut sitting = bookmark("https://collide/");
        sitting.note = "settled".into();
        let mover = bookmark("https://x/mover");
        let other = bookmark("https://x/other");
        let books = vec![sitting, mover, other];

        let pass = FakePass(|bookmark: &Bookmark| {
            let target = match bookmark.url.path() {
                // `other` heads for the mover's URL, which keeps the mover's URL a target
                // and so protects it from being deleted as absorbed.
                "/other" => "https://x/mover",
                _ => "https://collide/",
            };
            Ok(Plan::Bookmark(Bookmark {
                url: u(target),
                note: "settled".into(),
                ..unchanged_plan(bookmark)
            }))
        });
        let pinboard = FakePinboard::default();

        let outcome = run_pass(
            &store(&pinboard, &books, false),
            &books,
            "test",
            NO_DATING,
            &pass,
        )
        .await;
        assert_eq!(outcome.write_failed, 0);
        assert!(
            !pinboard
                .updated
                .borrow()
                .iter()
                .any(|call| call.url == "https://collide/"),
            "a converged incumbent merge must not rewrite itself every run"
        );
    }

    #[tokio::test]
    async fn a_failed_move_protects_the_record_it_left_behind() {
        // Y fails to move to its new URL, so its record is still sitting at the old one.
        // A is heading there. Refusals are settled before any write, so nothing had marked
        // that URL — and `replace=yes` would overwrite Y with A, losing Y from the account
        // entirely: never written to its new URL, and clobbered at its old one.
        let mut stranded = bookmark("https://collide/");
        stranded.title = "Y original".into();
        let mover = bookmark("https://a/");
        let books = vec![stranded, mover];

        let pass = FakePass(|bookmark: &Bookmark| {
            let target = match bookmark.url.as_str() {
                "https://collide/" => "https://elsewhere/",
                _ => "https://collide/",
            };
            Ok(Plan::Bookmark(Bookmark {
                url: u(target),
                ..unchanged_plan(bookmark)
            }))
        });
        let pinboard = FakePinboard {
            fail_update_urls: ["https://elsewhere/".to_string()].into_iter().collect(),
            ..FakePinboard::default()
        };

        let outcome = run_pass(
            &store(&pinboard, &books, false),
            &books,
            "test",
            NO_DATING,
            &pass,
        )
        .await;
        assert_eq!(outcome.write_failed, 1);
        assert_eq!(
            outcome.refused, 1,
            "the URL the failed move left occupied must refuse the next rewrite"
        );
        assert!(
            !pinboard
                .updated
                .borrow()
                .iter()
                .any(|call| call.url == "https://collide/"),
            "a record stranded by a failed write must not be overwritten"
        );
    }

    #[tokio::test]
    async fn a_visibility_mismatch_refuses_only_the_mover() {
        // The record that stays usually has a plan of its own, and that plan has nothing to
        // do with the mover's visibility. Refusing the whole group would let one mismatched
        // duplicate freeze an unrelated bookmark's cleanup for good — every run, with no
        // way out short of the user editing a visibility by hand.
        let mut sitting = bookmark("https://github.com/new/name");
        sitting.public = false;
        sitting.title = "stale".into();
        let mut mover = bookmark("https://github.com/old/name");
        mover.public = true;
        let target = sitting.url.clone();
        let books = vec![mover, sitting];
        let pass = FakePass(move |bookmark: &Bookmark| {
            Ok(Plan::Bookmark(Bookmark {
                url: target.clone(),
                title: "fresh".into(),
                ..unchanged_plan(bookmark)
            }))
        });
        let pinboard = FakePinboard::default();

        let outcome = run_pass(
            &store(&pinboard, &books, false),
            &books,
            "test",
            NO_DATING,
            &pass,
        )
        .await;
        assert_eq!(outcome.refused, 1, "only the mover is refused");
        let updated = pinboard.updated.borrow();
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].url, "https://github.com/new/name");
        assert_eq!(
            updated[0].description, "fresh",
            "the bookmark that stays still gets its own cleanup"
        );
        assert!(
            !updated[0].shared,
            "and it is not merged with, so its visibility is untouched"
        );
        assert!(
            pinboard.deleted.borrow().is_empty(),
            "the refused mover must not be absorbed"
        );
    }

    #[tokio::test]
    async fn a_visibility_mismatch_refuses_instead_of_merging() {
        // Merging a private mover into a public resident has no safe answer: keeping the
        // resident public republishes the mover's private annotation, and narrowing it
        // silently unpublishes a bookmark the user chose to share. Refuse instead, and
        // leave both records exactly as they are.
        let mut mover = bookmark("https://x/mine");
        mover.public = false;
        mover.note = "private annotation".into();
        let mut resident = bookmark("https://other/resident");
        resident.public = true;
        let slice = vec![mover.clone()];
        let all = vec![mover, resident];

        let pass = FakePass(|bookmark: &Bookmark| {
            Ok(Plan::Bookmark(Bookmark {
                url: u("https://other/resident"),
                ..unchanged_plan(bookmark)
            }))
        });
        let pinboard = FakePinboard::default();

        let outcome = run_pass(
            &store(&pinboard, &all, false),
            &slice,
            "test",
            NO_DATING,
            &pass,
        )
        .await;
        assert_eq!(outcome.refused, 1);
        assert_eq!(outcome.changed, 0);
        assert!(
            pinboard.updated.borrow().is_empty(),
            "a mismatched merge must write nothing at all"
        );
        assert!(pinboard.deleted.borrow().is_empty());
    }

    #[tokio::test]
    async fn a_visibility_mismatch_protects_the_movers_own_url() {
        // The refused mover stays at its own URL, so a second rewrite heading there would
        // overwrite the record the refusal just preserved. The mismatch has to be settled
        // before the occupied-by-refused fixpoint, not after it.
        let mut mover = bookmark("https://x/mine");
        mover.public = false;
        let mut resident = bookmark("https://other/resident");
        resident.public = true;
        let second = bookmark("https://x/second");
        let slice = vec![mover.clone(), second.clone()];
        let all = vec![mover, second, resident];

        let pass = FakePass(|bookmark: &Bookmark| {
            let target = match bookmark.url.path() {
                "/mine" => "https://other/resident",
                _ => "https://x/mine",
            };
            Ok(Plan::Bookmark(Bookmark {
                url: u(target),
                ..unchanged_plan(bookmark)
            }))
        });
        let pinboard = FakePinboard::default();

        let outcome = run_pass(
            &store(&pinboard, &all, false),
            &slice,
            "test",
            NO_DATING,
            &pass,
        )
        .await;
        assert_eq!(outcome.refused, 2);
        assert!(
            pinboard.updated.borrow().is_empty(),
            "the mover's own URL is still occupied, so nothing may be written there"
        );
    }

    #[tokio::test]
    async fn merging_does_not_reset_a_residents_unparseable_date() {
        // A stored date that wouldn't parse reads back as `None`, and writing `None` omits
        // `dt` — under `replace=yes` Pinboard then dates the record to today. Keep the
        // date the merge found rather than losing the record's age.
        let mut mover = bookmark("https://x/mine");
        mover.timestamp = crate::timefmt::from_unix(1_000_000);
        let mut outsider = bookmark("https://other/resident");
        outsider.timestamp = None;
        let slice = vec![mover.clone()];
        let all = vec![mover, outsider];

        let pass = FakePass(|bookmark: &Bookmark| {
            Ok(Plan::Bookmark(Bookmark {
                url: u("https://other/resident"),
                ..unchanged_plan(bookmark)
            }))
        });
        let pinboard = FakePinboard::default();

        let outcome = run_pass(
            &store(&pinboard, &all, false),
            &slice,
            "test",
            NO_DATING,
            &pass,
        )
        .await;
        assert_eq!(outcome.refused, 0);
        let updated = pinboard.updated.borrow();
        assert_eq!(updated.len(), 1);
        assert!(
            !updated[0].dt.is_empty(),
            "an empty dt lets Pinboard re-date the record to now"
        );
    }

    #[tokio::test]
    async fn merging_a_resident_keeps_its_read_later_and_date() {
        // The resident is the record that survives at this URL, so its own reading state
        // and stored date stay put: `merge_bookmarks`'s any()/earliest rules are for two
        // plans, and with dating off nothing may re-date a bookmark at all.
        let mut mover = bookmark("https://x/mine");
        mover.read_later = true;
        mover.timestamp = crate::timefmt::from_unix(1_000_000);
        let mut outsider = bookmark("https://other/resident");
        outsider.read_later = false;
        outsider.timestamp = crate::timefmt::from_unix(1_700_000_000);
        let slice = vec![mover.clone()];
        let all = vec![mover, outsider];

        let pass = FakePass(|bookmark: &Bookmark| {
            Ok(Plan::Bookmark(Bookmark {
                url: u("https://other/resident"),
                title: "New".into(),
                ..unchanged_plan(bookmark)
            }))
        });
        let pinboard = FakePinboard::default();

        let outcome = run_pass(
            &store(&pinboard, &all, false),
            &slice,
            "test",
            NO_DATING,
            &pass,
        )
        .await;
        assert_eq!(outcome.refused, 0);
        let updated = pinboard.updated.borrow();
        assert_eq!(updated.len(), 1);
        assert!(!updated[0].toread, "must not pick up the mover's to-read");
        assert_eq!(
            updated[0].dt,
            crate::timefmt::to_rfc3339(crate::timefmt::from_unix(1_700_000_000).unwrap()).unwrap(),
            "must not be backdated to the mover's date"
        );
    }

    #[tokio::test]
    async fn a_converged_merge_is_not_rewritten_every_run() {
        // The resident already holds exactly what the merge produces and there is nothing
        // left to absorb (the mover's old URL is another plan's target, so it is not
        // deleted), so the pass must leave it alone rather than rewriting it forever.
        let mut resident = bookmark("https://collide/");
        resident.note = "settled".into();
        let mover = bookmark("https://x/mover");
        let other = bookmark("https://x/other");
        let slice = vec![mover.clone(), other.clone()];
        let all = vec![resident.clone(), mover, other];

        let pass = FakePass(move |bookmark: &Bookmark| {
            if bookmark.url.as_str().ends_with("/mover") {
                // Plans exactly the resident's content onto the resident's URL.
                return Ok(Plan::Bookmark(Bookmark {
                    url: u("https://collide/"),
                    note: "settled".into(),
                    ..unchanged_plan(bookmark)
                }));
            }
            // Targets the mover's URL, so the mover's URL is never absorbed.
            Ok(Plan::Bookmark(Bookmark {
                url: u("https://x/mover"),
                ..unchanged_plan(bookmark)
            }))
        });
        let pinboard = FakePinboard::default();

        let outcome = run_pass(
            &store(&pinboard, &all, false),
            &slice,
            "test",
            NO_DATING,
            &pass,
        )
        .await;
        let updated = pinboard.updated.borrow();
        let written: Vec<&str> = updated.iter().map(|c| c.url.as_str()).collect();
        assert!(
            !written.contains(&"https://collide/"),
            "the settled record must not be rewritten: {written:?}"
        );
        assert_eq!(outcome.refused, 0);
    }

    #[tokio::test]
    async fn a_bookmark_is_not_a_resident_of_its_own_plan() {
        // Reading residency from a live view makes a bookmark that stays at its own URL
        // find *itself* sitting there. Merging with itself would union its stored record
        // back into the plan and resurrect exactly what the plan was computed to remove.
        let mut stored = bookmark("https://x/mine");
        stored.tags = vec!["keep".into(), "drop-me".into()];
        stored.note = "stale note".into();
        let books = vec![stored];

        let pass = FakePass(|bookmark: &Bookmark| {
            Ok(Plan::Bookmark(Bookmark {
                tags: vec!["keep".into()],
                note: "fresh note".into(),
                ..unchanged_plan(bookmark)
            }))
        });
        let pinboard = FakePinboard::default();

        let outcome = run_pass(
            &store(&pinboard, &books, false),
            &books,
            "test",
            NO_DATING,
            &pass,
        )
        .await;
        assert_eq!(outcome.refused, 0);
        let updated = pinboard.updated.borrow();
        assert_eq!(updated.len(), 1);
        assert!(
            !updated[0].tags.contains(&"drop-me".to_string()),
            "the plan dropped this tag; merging with itself would bring it back: {updated:?}"
        );
        assert_eq!(
            updated[0].extended, "fresh note",
            "the stale note must not be merged back in"
        );
    }

    #[tokio::test]
    async fn rewrite_onto_a_resident_outside_the_slice_merges_with_it() {
        // A plan can target a URL its own filter would have excluded — HackerNews
        // rewrites a story bookmark to its article — so the resident sitting there is
        // invisible to a residency check that only looks at the slice. Pinboard keys on
        // URL, so the end state is one record: merge, keeping what the resident had.
        let mut mover = bookmark("https://x/mine");
        mover.note = "from the mover".into();
        mover.tags = vec!["moved".into()];
        let mut outsider = bookmark("https://other/resident");
        outsider.note = "hand written".into();
        outsider.tags = vec!["keepme".into()];
        outsider.title = "Resident title".into();
        let slice = vec![mover.clone()];
        let all = vec![mover, outsider];

        let pass = FakePass(|bookmark: &Bookmark| {
            Ok(Plan::Bookmark(Bookmark {
                url: u("https://other/resident"),
                ..unchanged_plan(bookmark)
            }))
        });
        let pinboard = FakePinboard::default();

        let outcome = run_pass(
            &store(&pinboard, &all, false),
            &slice,
            "test",
            NO_DATING,
            &pass,
        )
        .await;
        assert_eq!(outcome.refused, 0);
        let updated = pinboard.updated.borrow();
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].url, "https://other/resident");
        // The resident goes into the merge first, so its title survives and its notes and
        // tags are kept alongside the mover's rather than replaced by them.
        assert_eq!(updated[0].description, "Resident title");
        assert!(updated[0].extended.contains("hand written"));
        assert!(updated[0].extended.contains("from the mover"));
        assert!(updated[0].tags.contains(&"keepme".to_string()));
        assert!(updated[0].tags.contains(&"moved".to_string()));
        // The mover's old URL is absorbed, so a later run sees one bookmark.
        assert_eq!(
            *pinboard.deleted.borrow(),
            vec!["https://x/mine".to_string()]
        );
    }

    #[tokio::test]
    async fn a_rewrite_onto_an_unoccupied_url_still_goes_through() {
        // The other half of the guard: protecting residents must not block the ordinary
        // rewrite onto a URL nobody has bookmarked.
        let mover = bookmark("https://x/mine");
        let slice = vec![mover.clone()];
        let all = vec![mover];

        let pass = FakePass(|bookmark: &Bookmark| {
            Ok(Plan::Bookmark(Bookmark {
                url: u("https://other/vacant"),
                ..unchanged_plan(bookmark)
            }))
        });
        let pinboard = FakePinboard::default();

        let outcome = run_pass(
            &store(&pinboard, &all, false),
            &slice,
            "test",
            NO_DATING,
            &pass,
        )
        .await;
        assert_eq!(outcome.refused, 0);
        let updated = pinboard.updated.borrow();
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].url, "https://other/vacant");
        assert_eq!(
            *pinboard.deleted.borrow(),
            vec!["https://x/mine".to_string()]
        );
    }

    #[tokio::test]
    async fn rewrite_onto_skipped_resident_merges_with_it() {
        // A is skipped (it stays put at T with its own record); B rewrites onto T. A being
        // skipped means its stored record is current, so B's plan merges into it rather
        // than replacing A's title/note/tags — and B's old URL is absorbed.
        let mut stored_a = bookmark("https://collide/");
        stored_a.tags = vec!["x".into()];
        stored_a.note = "from A".into();
        let mut stored_b = bookmark("https://old-b/");
        stored_b.tags = vec!["y".into()];
        stored_b.note = "from B".into();
        let books = vec![stored_a, stored_b];

        let pass = FakePass(|bookmark: &Bookmark| {
            if bookmark.url.as_str().contains("collide") {
                Ok(Plan::Skipped)
            } else {
                Ok(Plan::Bookmark(Bookmark {
                    url: u("https://collide/"),
                    ..unchanged_plan(bookmark)
                }))
            }
        });
        let pinboard = FakePinboard::default();

        let outcome = run_pass(
            &store(&pinboard, &books, false),
            &books,
            "test",
            NO_DATING,
            &pass,
        )
        .await;
        assert_eq!(outcome.plan_failed + outcome.write_failed, 0);
        assert_eq!(outcome.refused, 0);
        let updated = pinboard.updated.borrow();
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].url, "https://collide/");
        assert!(updated[0].extended.contains("from A"), "{updated:?}");
        assert!(updated[0].extended.contains("from B"), "{updated:?}");
        assert!(updated[0].tags.contains(&"x".to_string()));
        assert!(updated[0].tags.contains(&"y".to_string()));
        assert_eq!(
            *pinboard.deleted.borrow(),
            vec!["https://old-b/".to_string()]
        );
    }

    #[tokio::test]
    async fn rewrite_onto_a_bookmark_whose_lookup_failed_is_refused() {
        // A failed lookup leaves us not knowing A's end-state, so it is just as much an
        // untouchable resident as a skipped one. Without that, B's rewrite lands on A's
        // URL with replace=yes and destroys A's record — and now that a failed lookup no
        // longer fails the run, it would do so while reporting success.
        let mut stored_a = bookmark("https://collide/");
        stored_a.note = "hand written".into();
        let stored_b = bookmark("https://old-b/");
        let books = vec![stored_a, stored_b];

        let pass = FakePass(|bookmark: &Bookmark| {
            if bookmark.url.as_str().contains("collide") {
                Err(item_error())
            } else {
                Ok(Plan::Bookmark(Bookmark {
                    url: u("https://collide/"),
                    ..unchanged_plan(bookmark)
                }))
            }
        });
        let pinboard = FakePinboard::default();

        let outcome = run_pass(
            &store(&pinboard, &books, false),
            &books,
            "test",
            NO_DATING,
            &pass,
        )
        .await;
        assert_eq!(outcome.plan_failed, 1);
        assert!(
            pinboard.updated.borrow().is_empty(),
            "must not write over a bookmark whose own lookup failed"
        );
        assert!(pinboard.deleted.borrow().is_empty());
    }

    #[tokio::test]
    async fn rewrite_onto_a_bookmark_left_unplanned_by_reauth_is_refused() {
        // Planning stops at the dead credential, so the third bookmark is never planned.
        // It must still be protected: the first bookmark's rewrite targets its URL, and
        // writing there would clobber a record we never even looked at.
        let mover = bookmark("https://old-b/");
        let dead = bookmark("https://dead/");
        let mut resident = bookmark("https://collide/");
        resident.note = "hand written".into();
        let books = vec![mover, dead, resident];

        let pass = FakePass(|bookmark: &Bookmark| {
            if bookmark.url.as_str().contains("dead") {
                return Err(SourceError::ReauthRequired("token expired".into()));
            }
            Ok(Plan::Bookmark(Bookmark {
                url: u("https://collide/"),
                ..unchanged_plan(bookmark)
            }))
        });
        let pinboard = FakePinboard::default();

        let outcome = run_pass(
            &store(&pinboard, &books, false),
            &books,
            "test",
            NO_DATING,
            &pass,
        )
        .await;
        assert!(matches!(outcome.halted, Some(Halt::Reauth(_))));
        assert!(
            pinboard.updated.borrow().is_empty(),
            "must not write over a bookmark reauth left unplanned"
        );
        assert!(pinboard.deleted.borrow().is_empty());
    }

    /// Two bookmarks in the slice, both planned onto the second one's own URL — a GitHub
    /// repo rename, where the old and new names are separately starred. The bookmark
    /// already stored at the target is the record that stays there, exactly like an
    /// unplanned resident; being inside the slice is an implementation detail the user
    /// can't see, so it must not decide whether their bookmark is protected.
    async fn run_rename_collision(
        pinboard: &FakePinboard,
        sitting: Bookmark,
        mover: Bookmark,
    ) -> PassOutcome {
        let target = sitting.url.clone();
        let books = vec![mover, sitting];
        let pass = FakePass(move |bookmark: &Bookmark| {
            Ok(Plan::Bookmark(Bookmark {
                url: target.clone(),
                ..unchanged_plan(bookmark)
            }))
        });
        run_pass(
            &store(pinboard, &books, false),
            &books,
            "test",
            NO_DATING,
            &pass,
        )
        .await
    }

    #[tokio::test]
    async fn a_collision_onto_a_stored_bookmark_refuses_on_a_visibility_mismatch() {
        // The public bookmark is the one that stays; merging a private one onto it would
        // unshare it. Same hazard as the resident case, so the same refusal.
        let mut sitting = bookmark("https://github.com/new/name");
        sitting.public = true;
        let mut mover = bookmark("https://github.com/old/name");
        mover.public = false;
        let pinboard = FakePinboard::default();

        let outcome = run_rename_collision(&pinboard, sitting, mover).await;
        assert_eq!(outcome.refused, 1);
        assert!(
            pinboard.updated.borrow().is_empty(),
            "a public bookmark must not be silently unshared by a collision"
        );
        assert!(pinboard.deleted.borrow().is_empty());
    }

    #[tokio::test]
    async fn a_collision_onto_a_stored_bookmark_keeps_its_date_and_reading_state() {
        // Visibility matches, so the merge runs — and the bookmark already at the target
        // keeps its own date and to-read state rather than picking up the absorbed one's
        // under `merge_bookmarks`'s earliest/any rules. Its note leads, too.
        let mut sitting = bookmark("https://github.com/new/name");
        sitting.read_later = false;
        sitting.timestamp = crate::timefmt::from_unix(1_700_000_000);
        sitting.note = "sitting".into();
        let mut mover = bookmark("https://github.com/old/name");
        mover.read_later = true;
        mover.timestamp = crate::timefmt::from_unix(1_000_000);
        mover.note = "mover".into();
        let pinboard = FakePinboard::default();

        let outcome = run_rename_collision(&pinboard, sitting, mover).await;
        assert_eq!(outcome.refused, 0);
        let updated = pinboard.updated.borrow();
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].url, "https://github.com/new/name");
        assert!(!updated[0].toread, "must not pick up the mover's to-read");
        assert_eq!(
            updated[0].dt,
            crate::timefmt::to_rfc3339(crate::timefmt::from_unix(1_700_000_000).unwrap()).unwrap(),
            "must not be backdated to the mover's date"
        );
        assert_eq!(
            updated[0].extended, "sitting\n\nmover",
            "the record that stays leads the merge, so its note is the base"
        );
        assert_eq!(
            *pinboard.deleted.borrow(),
            vec!["https://github.com/old/name".to_string()]
        );
    }

    #[tokio::test]
    async fn a_collision_refuses_when_a_public_mover_lands_on_a_private_bookmark() {
        // The mirror direction of the mismatch: A (public) normalizes onto B's URL, and B
        // (private) is the record that stays. Refused rather than merged — narrowing A into
        // a private record is the safe direction for disclosure, but it still makes a
        // bookmark the user chose to share unreachable, and then deletes A.
        let mut stored_a = bookmark("https://old-a/");
        stored_a.public = true;
        let mut stored_b = bookmark("https://collide/");
        stored_b.public = false;
        let books = vec![stored_a, stored_b];

        let pass = FakePass(|bookmark: &Bookmark| {
            if bookmark.url.as_str().contains("old-a") {
                Ok(Plan::Bookmark(Bookmark {
                    url: u("https://collide/"),
                    ..unchanged_plan(bookmark)
                }))
            } else {
                Ok(Plan::Bookmark(unchanged_plan(bookmark)))
            }
        });
        let pinboard = FakePinboard::default();

        let outcome = run_pass(
            &store(&pinboard, &books, false),
            &books,
            "test",
            NO_DATING,
            &pass,
        )
        .await;
        assert_eq!(outcome.plan_failed + outcome.write_failed, 0);
        assert_eq!(outcome.refused, 1);
        assert!(pinboard.updated.borrow().is_empty());
        assert!(
            pinboard.deleted.borrow().is_empty(),
            "the refused mover must not be deleted"
        );
    }

    #[tokio::test]
    async fn colliding_new_url_privacy_never_widened() {
        // Neither member is resident at the target: both normalize to a brand-new URL, so
        // the pass has no resident to inherit privacy from. The first in order is public and
        // the second private; privacy must be the AND across the group (private wins), not
        // the first member's, so the private member's content is never republished public.
        let mut stored_a = bookmark("https://old-a/");
        stored_a.public = true;
        let mut stored_b = bookmark("https://old-b/");
        stored_b.public = false;
        let books = vec![stored_a, stored_b];

        let pass = FakePass(|bookmark: &Bookmark| {
            Ok(Plan::Bookmark(Bookmark {
                url: u("https://new/"),
                ..unchanged_plan(bookmark)
            }))
        });
        let pinboard = FakePinboard::default();

        let outcome = run_pass(
            &store(&pinboard, &books, false),
            &books,
            "test",
            NO_DATING,
            &pass,
        )
        .await;
        assert_eq!(outcome.plan_failed + outcome.write_failed, 0);

        let updated = pinboard.updated.borrow();
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].url, "https://new/");
        // The public member sorts first, but the private member keeps the merge private.
        assert!(!updated[0].shared);
    }

    #[tokio::test]
    async fn a_dry_run_pass_still_advances_the_view() {
        // A preview has to show the changes a real run would make, including the ones a
        // later pass only makes because an earlier one wrote. So a dry run routes through
        // the store as usual — the store is what withholds the network, not this loop.
        let books = vec![bookmark("https://x/mine")];
        let pass = FakePass(|bookmark: &Bookmark| {
            Ok(Plan::Bookmark(Bookmark {
                url: u("https://x/moved"),
                ..unchanged_plan(bookmark)
            }))
        });
        let pinboard = FakePinboard::default();
        let store = store(&pinboard, &books, true);

        let outcome = run_pass(&store, &books, "test", NO_DATING, &pass).await;
        assert_eq!(outcome.changed, 1);

        assert!(
            store.resident(&u("https://x/moved")).is_some(),
            "the next pass must preview from the state this one would leave"
        );
        assert!(store.resident(&u("https://x/mine")).is_none());
        assert!(pinboard.updated.borrow().is_empty(), "and write nothing");
        assert!(pinboard.deleted.borrow().is_empty());
    }

    #[tokio::test]
    async fn dry_run_renders_every_field_and_writes_nothing() {
        // Two bookmarks so the dry-run renderer hits both notes branches: the first
        // empties the notes ("(removed)"), the second sets new non-empty notes.
        let books = vec![bookmark("https://empty/"), bookmark("https://full/")];
        let pass = FakePass(|bookmark: &Bookmark| {
            let note = if bookmark.url.as_str().contains("empty") {
                String::new()
            } else {
                "new notes".into()
            };
            Ok(Plan::Bookmark(Bookmark {
                url: u(&format!("{}new", bookmark.url)),
                title: "New".into(),
                note,
                tags: vec!["x".into()],
                // A datable candidate source time, so the driver re-dates and the
                // `date ->` line renders.
                timestamp: crate::timefmt::from_unix(1_700_000_000),
                ..unchanged_plan(bookmark)
            }))
        });
        let pinboard = FakePinboard::default();

        // Dating on with a huge cap, so the source date is always applied.
        let dates = DateOpts {
            use_post_date: true,
            max_age_days: 1_000_000,
            stale_to_now: false,
        };
        let outcome = run_pass(
            &store(&pinboard, &books, true),
            &books,
            "test",
            dates,
            &pass,
        )
        .await;
        assert_eq!(outcome.plan_failed + outcome.write_failed, 0);
        assert!(pinboard.updated.borrow().is_empty());
        assert!(pinboard.deleted.borrow().is_empty());
    }
}
