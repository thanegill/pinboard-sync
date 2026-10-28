//! The shared `cleanup` driver. Each source describes how to re-shape one bookmark (a
//! [`CleanupPass`]); [`run_pass`] owns the loop common to all of them: plan every
//! bookmark's end-state, group the plans by target URL, and write each group.
//!
//! A lone plan is fitted to what the store would keep ([`BookmarkStore::fit`]), skipped
//! when it would store nothing that writing its stored bookmark back wouldn't, and
//! otherwise written via [`BookmarkStore::apply_update`], deleting the old URL on a
//! rewrite. Plans landing on one URL, with any record already stored there, are
//! field-merged ([`merge_bookmarks`]) into one record, checked and written the same way via
//! [`BookmarkStore::apply_merge`], which deletes the absorbed URLs: Pinboard holds one
//! record per URL. A plan that disagrees about visibility with the record staying at its
//! target is refused and left where it is; with no record staying, plans that disagree with
//! each other about `public` are all refused, since a merge never picks a visibility. A
//! bookmark planned to move *away* from a target neither leads that merge nor blocks it.
//!
//! A rewrite never deletes an old URL that another plan targets, so chained rewrites can't
//! clobber each other. Every refusal decided before the first write is closed under
//! [`propagate_refusals`], which makes it order-independent; a failed write re-closes it
//! over the groups not yet written, so a failed run is the one case where order shows.
//! Groups are written in [`write_order`], each after the group moving its target's occupant
//! away, so a failed move is protected before anything lands on the record it left behind.
//! A cycle of moves, such as a swap, has no such order. The dry-run lines and tallies go
//! into a [`PassOutcome`], and [`PassOutcome::into_result`] decides whether the run failed.

use std::collections::{HashMap, HashSet};

use anyhow::{anyhow, Result};
use log::{debug, error, info, warn};
use time::OffsetDateTime;
use url::Url;

use crate::bookmark::{AccountState, Bookmark, BookmarkStore};
use crate::preview;
use crate::source::{Halt, SourceError};
use crate::stdout::outln;
use crate::timefmt::DateOpts;

mod merge;

use merge::{incumbent_of, merge_bookmarks};

/// What a pass decided about one bookmark. [`Plan::Unchanged`] and [`Plan::Skipped`]
/// both mean "write nothing", but they say opposite things about the *source*: the
/// first is an answer from it, the second is a bookmark the pass never asked about.
/// Only the first is evidence the source is reachable, which is what lets the driver
/// tell one dead link apart from an outage.
#[derive(Debug, PartialEq, Eq)]
#[expect(
    clippy::large_enum_variant,
    reason = "a plan is returned and unpacked at once, never stored as a Plan"
)]
pub enum Plan {
    /// The bookmark's desired end-state; the driver diffs it and writes what changed.
    Bookmark(Bookmark),
    /// The source answered, and nothing about this bookmark needs to change.
    Unchanged,
    /// Not this pass's bookmark -- a deep link, a foreign URL, a locally-filtered case.
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
pub trait CleanupPass {
    /// The end-state for `bookmark` as a [`Plan`]: an answer from the source
    /// ([`Plan::Bookmark`], [`Plan::Unchanged`]) or a bookmark this pass does not handle
    /// ([`Plan::Skipped`]): how the driver tells one dead link from an unreachable source.
    ///
    /// [`SourceError::Other`] is a per-item failure: logged and counted, and the pass goes
    /// on. [`SourceError::Halted`] stops the pass, since every remaining lookup fails too.
    ///
    /// A plan's `timestamp` is only the candidate source date: the driver resolves the
    /// final date with the pass's [`DateOpts`] and takes `public`/`read_later` from the
    /// stored bookmark, ignoring the plan's (a merged group's record follows
    /// [`merge_bookmarks`]'s rules over its members' stored flags). The driver skips a plan
    /// that matches `bookmark`, so a pass need not check for changes itself.
    async fn plan(&self, bookmark: &Bookmark) -> Result<Plan, SourceError>;
}

/// What one pass did. `plan_failed` (a link the source would not answer for) and
/// `write_failed` (the destination refusing a write) are kept apart because they mean
/// different things. `must_use`: [`PassOutcome::into_result`] is the one place the exit
/// code is decided, so dropping an outcome discards every failure it recorded.
#[must_use]
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PassOutcome {
    /// Bookmarks written, or under `dry_run` that would be.
    pub changed: usize,
    /// Under `dry_run`, the per-bookmark preview lines as printed (not the closing tally),
    /// kept so tests can assert on what an operator reads before a real run.
    pub preview: Vec<String>,
    /// Rewrites this pass declined to make: a target that must not be written at all (see
    /// [`Refusal`]), or a plan held back over visibility. Not a failure, but work that did
    /// not happen, so it is reported. Counted per *group* for an untouchable target and per
    /// *plan* for one held back, so it counts abandoned writes, not bookmarks.
    pub refused: usize,
    /// Bookmarks the source gave an answer for -- the evidence that it is reachable.
    /// Excludes [`Plan::Skipped`], which never asked it anything.
    pub reached: usize,
    /// Bookmarks whose `plan` failed to read the source.
    pub plan_failed: usize,
    /// Bookmarks whose write to the store failed.
    pub write_failed: usize,
    /// Set when the source told us to stop partway. [`PassOutcome::into_result`] hands it
    /// back as [`SourceError::Halted`], so the `cleanup` command can tell a dead credential
    /// from a quota or an edge block.
    pub halted: Option<Halt>,
}

/// Why a rewrite was left in place rather than written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refusal {
    /// The target holds a record this pass could not read -- a failed lookup, or one a halt
    /// stopped it short of -- so what is stored there may be stale.
    Unreadable,
    /// The target holds a bookmark whose own rewrite was refused, so it never moved and its
    /// URL is still occupied. A visibility mismatch reaches the driver this way: the reason
    /// the mover stayed is logged where it is actionable, at the point of refusal, and what
    /// the *next* group needs to know is only that the URL is still taken.
    OccupiedByRefused,
    /// The target holds a bookmark whose own rewrite was attempted and *failed*, so it is
    /// still there. Distinct from [`Refusal::OccupiedByRefused`] because the operator's
    /// problem is different: something is wrong with the writes, not with the plan.
    WriteFailed,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Refusal::Unreadable => "occupied by a bookmark this pass could not read",
            Refusal::OccupiedByRefused => "occupied by a rewrite that was itself refused",
            Refusal::WriteFailed => "occupied by a bookmark whose own rewrite failed",
        })
    }
}

impl PassOutcome {
    /// The run's verdict. A link that could not be looked up does not fail the run: it is
    /// logged and skipped, so one dead URL can't wedge a scheduled service. A [`Halt`], a
    /// failed write, or a majority of failed lookups (`plan_failed > reached.max(1)`) do.
    ///
    /// The majority, not "every lookup", because permanent per-item conditions arrive as
    /// [`Plan`]s, leaving `plan_failed` to post-retry 5xx, network trouble and unlabelled
    /// throttling. The `max(1)` floor keeps one failed lookup survivable for a pass that
    /// looks up only one or two bookmarks, like the discussion-link pass. Returns
    /// [`SourceError`] so the `cleanup` command fires the auth-failure hook for a halted
    /// credential, and only for that.
    pub fn into_result(self) -> Result<(), SourceError> {
        if let Some(halt) = self.halted {
            // Why we stopped is the headline, but it must not bury what else went wrong
            // before it: those counts are the only record once this returns.
            let mut also = Vec::new();
            if self.plan_failed > 0 {
                also.push(format!("{} lookup(s)", self.plan_failed));
            }
            if self.write_failed > 0 {
                also.push(format!("{} write(s)", self.write_failed));
            }
            if also.is_empty() {
                return Err(halt.into());
            }
            let also = also.join(", ");
            return Err(halt
                .map_message(|message| format!("{message} (also failed: {also})"))
                .into());
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

/// Combine the verdicts of a cleanup's two passes, each of which reached its own so that
/// neither can vouch for the other. Each is given as `(name, verdict)`. When both failed,
/// both are reported: one leads and the other follows after `; and for <name>: `. The
/// first pass leads unless the second outranks it. A dead credential outranks any other
/// halt, because the outer error is the one whose variant survives, and the variant is
/// what decides the auth-failure hook. Any halt outranks an ordinary failure, since why a
/// pass stopped is the headline.
pub fn two_pass_verdict(
    (first_name, first): (&str, Result<(), SourceError>),
    (second_name, second): (&str, Result<(), SourceError>),
) -> Result<(), SourceError> {
    let (first, second) = match (first, second) {
        (Err(first), Err(second)) => (first, second),
        (Err(e), Ok(())) | (Ok(()), Err(e)) => return Err(e),
        (Ok(()), Ok(())) => return Ok(()),
    };
    let rank = |error: &SourceError| match error {
        SourceError::Other(_) => 0,
        SourceError::Halted(Halt::RateLimited(_) | Halt::Blocked(_)) => 1,
        SourceError::Halted(Halt::Reauth(_)) => 2,
    };
    let (lead, other, other_name) = if rank(&second) > rank(&first) {
        (second, first, first_name)
    } else {
        (first, second, second_name)
    };
    let note = |message: String| format!("{message}; and for {other_name}: {other:#}");
    Err(match lead {
        SourceError::Halted(halt) => halt.map_message(note).into(),
        SourceError::Other(error) => SourceError::Other(anyhow!(note(format!("{error:#}")))),
    })
}

/// Run `pass` over `bookmarks` (already filtered to the source), as the module doc
/// describes; under the store's dry-run, print what would change instead. `noun` names the
/// source in the scan log. Never fails outright: every failure, a dead credential included,
/// is tallied into the returned [`PassOutcome`].
///
/// Residency is asked of `store`, whose view covers the *whole* account and reflects what
/// earlier passes of this run wrote: a plan can target a URL outside its own filter (an HN
/// story rewritten to its article), and `cleanup --all` runs three sources on one account.
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

    let mut outcome = PassOutcome::default();
    let Planned {
        pairs,
        mut untouchable,
    } = plan_all(bookmarks, dates, pass, &mut outcome).await;
    let (first_appearance, mut groups) = group_by_target(&pairs);
    let group_order = write_order(&first_appearance, &groups);
    let residents = residents_of(store, &pairs, &group_order, &untouchable);
    hold_back_visibility_mismatches(
        &group_order,
        &mut groups,
        &residents,
        &mut untouchable,
        dry_run,
        &mut outcome,
    );

    // The URLs a planned write still owns. A rewrite's old URL is never deleted when it is
    // one of these, because that write's record will live there and deleting it would
    // clobber it. Settled *after* the visibility partition: a group every member was held
    // back from owns nothing, and leaving its key here would strand a bookmark moving away
    // from that URL at both its old and new URL, silently and for good.
    let targets: HashSet<&Url> = group_order
        .iter()
        .filter(|key| !groups[*key].is_empty())
        .copied()
        .collect();

    // A group whose target is occupied won't move, so its *own* URL stays occupied too --
    // and a second group heading there would replace the record the refusal just saved.
    // Settled before any write so the outcome doesn't depend on group order.
    propagate_refusals(&group_order, &groups, &mut untouchable);

    for key in &group_order {
        let group = groups.remove(*key).expect("key came from group_order");
        // The visibility partition can empty a group outright, when every plan heading here
        // was held back. There is nothing left to refuse *or* write, so this comes before
        // the untouchable check: those plans were already counted as they were held back,
        // and reporting the group again would inflate `refused` past the number of writes
        // actually abandoned. It is also what guarantees `write_merge` a member to lead.
        if group.is_empty() {
            continue;
        }
        // The target must not be written, so leave the rewriting bookmark(s) at their old
        // URLs. Separate from the `targets` delete-guard -- that protects a URL some *plan*
        // writes to, this one a URL no plan may land on.
        if let Some(reason) = untouchable.get(key) {
            refuse_group(key, *reason, dry_run, &mut outcome);
            continue;
        }
        // Pinboard keys on URL, so the end state at this target has to be one record.
        // Anything already stored there joins the group rather than being replaced by it;
        // which member leads that merge is settled in `write_merge`.
        let resident = residents.get(key);
        let stranded = match (resident, &group[..]) {
            (None, [Member { stored, planned }]) => {
                write_single(store, stored, planned, &targets, dry_run, &mut outcome).await
            }
            _ => {
                write_merge(
                    store,
                    key,
                    &group,
                    resident,
                    &targets,
                    dry_run,
                    &mut outcome,
                )
                .await
            }
        };
        // A failed write left its bookmarks where they were, so a later group heading for
        // one of their URLs would overwrite it. Protect them and re-close the refusal set,
        // over the groups after this one only, which is why a failed write stays a failure
        // rather than becoming a refusal.
        if !stranded.is_empty() {
            for url in stranded {
                untouchable.entry(url).or_insert(Refusal::WriteFailed);
            }
            propagate_refusals(&group_order, &groups, &mut untouchable);
        }
    }

    if dry_run {
        outln!("{dry_prefix}{} bookmark(s) would change.", outcome.changed);
    } else {
        info!("done: updated {} bookmark(s)", outcome.changed);
    }
    if outcome.refused > 0 {
        warn!(
            "{} rewrite(s) not made: their target holds a bookmark this pass must not overwrite",
            outcome.refused
        );
    }
    outcome
}

/// One member of a collision group, the planned bookmarks heading for one target URL: its
/// stored record and its plan. A group holds its members in snapshot order.
#[derive(Clone, Copy)]
struct Member<'p> {
    stored: &'p Bookmark,
    planned: &'p Bookmark,
}

/// Phase 1's result: each planned bookmark beside its stored record, and the URLs whose
/// record this pass could not establish.
struct Planned<'b> {
    pairs: Vec<(&'b Bookmark, Bookmark)>,
    /// A lookup failed there, or a halt stopped the pass short of it. Nothing may be
    /// written there at all. Owned rather than borrowed because the fixpoint grows it from
    /// the group keys.
    untouchable: HashMap<Url, Refusal>,
}

/// Plan every bookmark, tallying `reached`, `plan_failed` and `halted` into `outcome`, and
/// resolve each plan's date and privacy flags against its stored record.
async fn plan_all<'b, C: CleanupPass>(
    bookmarks: &'b [Bookmark],
    dates: DateOpts,
    pass: &C,
    outcome: &mut PassOutcome,
) -> Planned<'b> {
    let now = OffsetDateTime::now_utc().truncate_to_second();
    let mut pairs: Vec<(&Bookmark, Bookmark)> = Vec::new();
    let mut untouchable: HashMap<Url, Refusal> = HashMap::new();
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
            // Every remaining lookup would fail the same way, so stop planning, but fall
            // through to write the plans already made rather than discard them. This one
            // and everything after it goes unplanned, so protect all of them: a record we
            // never looked at must not be written over.
            Err(SourceError::Halted(halt)) => {
                outcome.halted = Some(halt);
                untouchable.extend(
                    bookmarks[index..]
                        .iter()
                        .map(|b| (b.url.clone(), Refusal::Unreadable)),
                );
                break;
            }
            // Log and skip a single failed plan so the rest of the pass still runs. Its
            // end-state is unknown, which makes it as untouchable as a skipped one: another
            // bookmark rewriting onto its URL would clobber the record.
            Err(e @ SourceError::Other(_)) => {
                outcome.plan_failed += 1;
                untouchable.insert(bookmark.url.clone(), Refusal::Unreadable);
                error!("looking up bookmark {}: {e:#}", bookmark.url);
                continue;
            }
        };

        // Resolve the final creation time here, at the write boundary: the candidate
        // source time when dating is on and within the cap, else "now"/preserve per the
        // policy. Comparable by instant to the stored `timestamp`.
        planned.timestamp = dates.cleanup_date(planned.timestamp, now, bookmark.timestamp);

        // Privacy flags are never re-shaped by cleanup: take them from the stored
        // bookmark so a plan can't silently flip them.
        planned.public = bookmark.public;
        planned.read_later = bookmark.read_later;

        pairs.push((bookmark, planned));
    }
    Planned { pairs, untouchable }
}

/// The plans grouped by target URL, with the groups' first-appearance order (and snapshot
/// order within each group).
fn group_by_target<'p>(
    pairs: &'p [(&'p Bookmark, Bookmark)],
) -> (Vec<&'p Url>, HashMap<&'p Url, Vec<Member<'p>>>) {
    let mut group_order: Vec<&Url> = Vec::new();
    let mut groups: HashMap<&Url, Vec<Member<'_>>> = HashMap::new();
    for (stored, planned) in pairs {
        let key = &planned.url;
        groups
            .entry(key)
            .or_insert_with(|| {
                group_order.push(key);
                Vec::new()
            })
            .push(Member { stored, planned });
    }
    (group_order, groups)
}

/// The groups in the order they are written: each after the group that moves its target's
/// occupant away, so a URL is vacated before a record lands on it, and a move that fails
/// leaves its record where [`propagate_refusals`] can still protect it. Otherwise
/// first-appearance order, which keeps a dry run's preview deterministic.
///
/// A cycle, such as two bookmarks swapping URLs, has no safe order: one of its writes lands
/// on a record that has not left yet and is lost if a later write in the cycle fails.
fn write_order<'p>(
    first_appearance: &[&'p Url],
    groups: &HashMap<&'p Url, Vec<Member<'p>>>,
) -> Vec<&'p Url> {
    let leaving_to: HashMap<&Url, &Url> = first_appearance
        .iter()
        .flat_map(|key| groups[*key].iter().map(|member| (&member.stored.url, *key)))
        .filter(|(from, to)| from != to)
        .collect();
    let mut placed: HashSet<&Url> = HashSet::with_capacity(first_appearance.len());
    let mut order = Vec::with_capacity(first_appearance.len());
    for key in first_appearance {
        let mut chain = Vec::new();
        let mut next = Some(*key);
        while let Some(url) = next {
            if !placed.insert(url) {
                break;
            }
            chain.push(url);
            next = leaving_to.get(url).copied();
        }
        order.extend(chain.into_iter().rev());
    }
    order
}

/// Every group's resident, resolved once, before any write. A resident is a record the live
/// view holds at a group's target that this pass neither planned (a bookmark is never a
/// resident of its own plan -- merging with itself would resurrect what the plan removed)
/// nor failed to read. Precomputing is equivalent to looking each up lazily: group keys are
/// distinct, and every URL a write deletes was planned, so no write can change a record
/// read here.
fn residents_of<'p, S: AccountState>(
    store: &S,
    pairs: &[(&Bookmark, Bookmark)],
    group_order: &[&'p Url],
    untouchable: &HashMap<Url, Refusal>,
) -> HashMap<&'p Url, Bookmark> {
    // The stored URLs of the bookmarks this pass actually planned. They are excluded from
    // residency: a bookmark that plans to stay at (or move within) its own URL would
    // otherwise find itself sitting there and merge its stored record back into its own
    // plan, undoing the very changes the plan computed.
    let planned_originals: HashSet<&Url> =
        pairs.iter().map(|(original, _)| &original.url).collect();
    group_order
        .iter()
        .filter(|key| !untouchable.contains_key(**key) && !planned_originals.contains(*key))
        .filter_map(|key| store.resident(key).map(|resident| (*key, resident)))
        .collect()
}

/// A plan [`hold_back_visibility_mismatches`] drops from its group.
struct HeldBack<'p> {
    /// The held-back bookmark's stored URL, which it keeps.
    mover: Url,
    target: &'p Url,
    mover_public: bool,
    /// Whether the record staying at `target` is public, or `None` when nothing stays
    /// there and the plans heading for it disagreed with each other.
    staying: Option<bool>,
}

/// Drop from each group the plans that disagree about visibility with what stays at its
/// target, counting each as refused and marking its own URL occupied.
///
/// A merge fuses the members' notes and tags into one record, so a bookmark may only join
/// when it agrees with the record that stays there about who may read it. Widening would
/// publish a private member's annotation; narrowing would unshare a bookmark the user chose
/// to share. Neither is ours to decide, so the disagreeing bookmark drops out of the group
/// and stays where it is.
///
/// Only that bookmark, not the whole group: the record that stays usually has a plan of
/// its own, which has nothing to do with the mover's visibility, and blocking that too
/// would let one mismatched duplicate freeze an unrelated bookmark's cleanup for good.
/// Settled ahead of the fixpoint, because a refused mover keeps its own URL occupied
/// exactly like a group refused for an unreadable target.
fn hold_back_visibility_mismatches<'p>(
    group_order: &[&'p Url],
    groups: &mut HashMap<&'p Url, Vec<Member<'p>>>,
    residents: &HashMap<&'p Url, Bookmark>,
    untouchable: &mut HashMap<Url, Refusal>,
    dry_run: bool,
    outcome: &mut PassOutcome,
) {
    let mut held_back: Vec<HeldBack> = Vec::new();
    for key in group_order {
        if untouchable.contains_key(*key) {
            continue;
        }
        let members = &groups[key];
        // What the joining plans have to agree with. A record that stays at the target sets
        // it. With none -- a target nothing occupies, or one whose occupant is itself
        // moving away -- there is nothing to defer to, so the plans need only agree with
        // *each other*; when they don't, no member may write, because merging would then
        // pick a visibility on the user's behalf with nothing to justify the choice.
        let staying = residents
            .get(key)
            .map(|resident| resident.public)
            .or_else(|| incumbent_of(members, key).map(|at| members[at].planned.public));
        let agreed = match staying {
            Some(public) => Some(public),
            None => members
                .first()
                .map(|member| member.planned.public)
                .filter(|first| members.iter().all(|m| m.planned.public == *first)),
        };
        let members = groups.get_mut(key).expect("key came from group_order");
        let (kept, refused): (Vec<_>, Vec<_>) = match agreed {
            Some(public) => members
                .iter()
                .copied()
                .partition(|member| member.planned.public == public),
            None => (Vec::new(), members.clone()),
        };
        if refused.is_empty() {
            continue;
        }
        held_back.extend(refused.iter().map(|member| HeldBack {
            mover: member.stored.url.clone(),
            target: key,
            mover_public: member.planned.public,
            staying,
        }));
        *members = kept;
    }
    for HeldBack {
        mover,
        target,
        mover_public,
        staying,
    } in held_back
    {
        outcome.refused += 1;
        let describe = |public| if public { "public" } else { "private" };
        match staying {
            Some(staying) => warn!(
                "not merging {mover} into {target}: it is {}, but {target} is {}",
                describe(mover_public),
                describe(staying)
            ),
            None => warn!(
                "not merging {mover} into {target}: the bookmarks moving there disagree \
                 about whether it should be public, and nothing is stored at {target} to \
                 settle it"
            ),
        }
        if dry_run {
            let reason = match staying {
                Some(staying) => {
                    format!("would merge into {target}, which is {}", describe(staying))
                }
                None => format!(
                    "would merge into {target} with bookmarks that disagree about visibility"
                ),
            };
            emit_preview(
                &mut outcome.preview,
                &mover,
                [preview::detail(format!("(refused: {reason})"))],
            );
        }
        // The mover didn't move, so its own URL is still occupied and no other rewrite may
        // land there.
        untouchable
            .entry(mover)
            .or_insert(Refusal::OccupiedByRefused);
    }
}

/// Count, log and preview a group whose target must not be written at all.
fn refuse_group(key: &Url, reason: Refusal, dry_run: bool, outcome: &mut PassOutcome) {
    outcome.refused += 1;
    warn!("skipping cleanup write to {key}: {reason}");
    // Also on stdout: a dry run is a preview, and work the real run would refuse is part
    // of what the operator is previewing.
    if dry_run {
        emit_preview(
            &mut outcome.preview,
            key,
            [preview::detail(format!("(refused: {reason})"))],
        );
    }
}

/// Write a lone plan with nothing stored at its target: skip it when it would store nothing
/// new ([`stores_nothing_new`]), else rewrite it (deleting the old URL when it moved and no
/// other plan targets it) and tally the result. Returns the stored URLs a failed write left
/// occupied, for the caller to protect: empty unless the write failed.
async fn write_single<S: BookmarkStore>(
    store: &S,
    original: &Bookmark,
    planned: &Bookmark,
    targets: &HashSet<&Url>,
    dry_run: bool,
    outcome: &mut PassOutcome,
) -> Vec<Url> {
    let fitted = fit_plan(store, Some(original), planned);
    if stores_nothing_new(store, original, &fitted) {
        return Vec::new();
    }
    let changes = original.diff(&fitted);
    warn_if_cut(planned, &fitted);
    let planned = &fitted;
    // Delete the old URL only when it changed and no other planned write targets it;
    // otherwise that write's record lives there and must not be removed.
    let delete_old = planned.url != original.url && !targets.contains(&original.url);

    if dry_run {
        let lines = changes
            .iter()
            .map(|(label, value)| preview::field(label, value));
        emit_preview(&mut outcome.preview, &original.url, lines);
    }

    // Always through the store, dry run included: it is what withholds the network, and it
    // is also what advances the view a later pass previews from. `planned` carries the
    // stored `public`/`read_later` and the driver-resolved `timestamp`, so it's the
    // complete write model.
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
                planned.tags.joined()
            );
            Vec::new()
        }
        // The rewrite can half-apply just as a merge can: the record landed at the new URL
        // and only the old-URL delete failed. Count the change that really happened, and
        // don't name the old URL as the thing that went wrong.
        Some(e) if write.wrote => {
            outcome.changed += 1;
            outcome.write_failed += 1;
            error!("rewrote {} to {} but: {e:#}", original.url, planned.url);
            Vec::new()
        }
        // The move didn't happen, so the record is still at its old URL.
        Some(e) => {
            outcome.write_failed += 1;
            error!("rewriting bookmark {}: {e:#}", original.url);
            vec![original.url.clone()]
        }
    }
}

/// Field-merge a group -- with the record staying at its target, if any -- into one record,
/// write it, and delete the absorbed URLs, so a later run sees a single bookmark at the
/// target and converges. When the merge would store nothing new over the record stored
/// there ([`stores_nothing_new`]), only the absorbed URLs are deleted, and with none the
/// group is skipped. Returns the stored URLs a failed write left occupied, for the caller
/// to protect: empty unless the write failed.
async fn write_merge<S: BookmarkStore>(
    store: &S,
    key: &Url,
    group: &[Member<'_>],
    resident: Option<&Bookmark>,
    targets: &HashSet<&Url>,
    dry_run: bool,
    outcome: &mut PassOutcome,
) -> Vec<Url> {
    // The record that stays at this URL: an unplanned resident, else the member already
    // stored here. Whichever it is leads the merge, so its title and note are the base the
    // others extend. For an incumbent that is its *plan*, not its stored record -- the plan
    // is this pass's intended end-state for it, and merging its stored record back in would
    // resurrect exactly what the plan removed.
    // A member is only lifted out of the tail when it is itself the survivor, so the two
    // can't disagree about which index to drop -- the alternative, filtering by `incumbent`
    // independently of which branch `survivor` took, would silently lose a member if both
    // were ever `Some`.
    let (survivor, lifted) = match resident {
        Some(resident) => (Some(resident), None),
        None => match incumbent_of(group, key) {
            Some(at) => (Some(group[at].planned), Some(at)),
            None => (None, None),
        },
    };
    let mut movers = group
        .iter()
        .enumerate()
        .filter(|(at, _)| Some(*at) != lifted)
        .map(|(_, member)| member.planned);
    let lead = survivor
        .or_else(|| movers.next())
        .expect("run_pass skips an empty group");
    let rest: Vec<&Bookmark> = movers.collect();
    let mut merged = merge_bookmarks(lead, &rest);
    if let Some(survivor) = survivor {
        // The survivor's own record-level state survives the merge's field rules: it is the
        // bookmark that stays here, and cleanup never re-shapes those (see `write_single`).
        // Its date in particular must not shift to a mover's under `merge_bookmarks`'s
        // earliest-wins rule, which is meant for two plans -- with dating off, nothing may
        // re-date a bookmark at all. `public` needs no such restore: a group only reaches
        // this point when every member already matches it, so the all()-rule cannot change
        // it.
        merged.read_later = survivor.read_later;
        // `None` means the stored date was absent or wouldn't parse, and writing it back
        // omits `dt` entirely -- under `replace=yes` Pinboard then re-dates the record to
        // now. Keep the earliest date the merge found instead of losing the record's age.
        merged.timestamp = survivor.timestamp.or(merged.timestamp);
    }
    // The *stored* record at the target, which for an incumbent is not the plan that led
    // the merge: the merge is fitted against it, and compared with it.
    let stored_at_target = resident.or_else(|| lifted.map(|at| group[at].stored));
    let fitted = fit_plan(store, stored_at_target, &merged);
    let target = &fitted.url;

    // Absorbed URLs to delete: an original that is neither the target nor another planned
    // write's target (deleting the latter would clobber that write's record).
    let mut old_urls: Vec<&Url> = Vec::new();
    for Member { stored, .. } in group {
        if stored.url != *target
            && !targets.contains(&stored.url)
            && !old_urls.contains(&&stored.url)
        {
            old_urls.push(&stored.url);
        }
    }

    // Nothing to do when the merge reproduces what is already stored at the target and
    // there is nothing to absorb, or a converged state would rewrite itself every run.
    let converged =
        stored_at_target.is_some_and(|stored| stores_nothing_new(store, stored, &fitted));
    if converged && old_urls.is_empty() {
        return Vec::new();
    }
    // A converged merge with URLs to absorb only deletes them: rewriting the target would
    // store nothing new, and would cut a whole note there longer than a write allows.
    let rewrite = !converged;
    warn_if_cut(&merged, &fitted);
    let merged = &fitted;

    if dry_run {
        let mut lines = Vec::new();
        if rewrite {
            lines.extend([
                preview::field("title", &merged.title),
                preview::field("notes", preview::note(&merged.note)),
                preview::field("tags", preview::tags(&merged.tags)),
            ]);
            if let Some(date) = merged.timestamp.and_then(crate::timefmt::to_pinboard_dt) {
                lines.push(preview::field("date", date));
            }
            lines.push(preview::field("public", merged.public));
            lines.push(preview::field("toread", merged.read_later));
        }
        lines.extend(
            old_urls
                .iter()
                .map(|old| preview::detail(format!("absorb {old}"))),
        );
        emit_preview(&mut outcome.preview, target, lines);
    }

    // Always through the store -- see `write_single`.
    let write = if rewrite {
        store.apply_merge(merged, &old_urls).await
    } else {
        store.apply_absorb(target, &old_urls).await
    };
    match write.error {
        None => {
            outcome.changed += 1;
            let absorbed: Vec<String> = write.deleted.iter().map(|u| u.to_string()).collect();
            let done = if rewrite { "merged" } else { "absorbed into" };
            debug!("{done} {target} <- [{}]", absorbed.join(" "));
            Vec::new()
        }
        // Half-applied: the merged record is at the target, written or already there, and
        // an absorbed URL survived. Count the change when the account did change, as the
        // rewrite path does, and name what is actually wrong, since "merging into
        // {target} failed" would send the operator after a record that is fine.
        Some(e) if write.wrote => {
            if rewrite || !write.deleted.is_empty() {
                outcome.changed += 1;
            }
            outcome.write_failed += 1;
            let stranded = old_urls.len() - write.deleted.len();
            let done = if rewrite { "merged" } else { "absorbed into" };
            error!("{done} {target} but {stranded} absorbed URL(s) remain: {e:#}");
            Vec::new()
        }
        // Nothing was written and nothing absorbed, so every member is still at its own
        // URL. Every member, not `old_urls`: that list deliberately excludes URLs another
        // group targets, which are precisely the ones in danger here.
        Some(e) => {
            outcome.write_failed += 1;
            error!("merging into {target}: {e:#}");
            group
                .iter()
                .map(|member| member.stored.url.clone())
                .collect()
        }
    }
}

/// `planned` as `store` would keep it, with `stored`'s note in place of its own when the
/// store would cut the plan's note down to part of the stored one. Writing that cut would
/// only shorten the stored note and keep none of the plan's text, as when a plan appends to
/// a whole note just under the limit, so the rest of the plan goes over the stored note,
/// itself fitted: kept whole when it still fits beside the plan's other fields.
fn fit_plan<S: BookmarkStore>(
    store: &S,
    stored: Option<&Bookmark>,
    planned: &Bookmark,
) -> Bookmark {
    let fitted = store.fit(planned.clone());
    let Some(stored) = stored else {
        return fitted;
    };
    let only_shortens = fitted.note != planned.note
        && store
            .kept_of_cut(&fitted.note)
            .is_some_and(|kept| stored.note.starts_with(kept));
    if !only_shortens {
        return fitted;
    }
    debug!(
        "using the stored note of {} in place of the plan's, which would be cut inside it",
        stored.url
    );
    store.fit(Bookmark {
        note: stored.note.clone(),
        ..planned.clone()
    })
}

/// Whether writing `fitted`, a plan from [`fit_plan`], would store nothing that writing
/// `stored` back wouldn't: both, fitted, differ in nothing a write changes. That covers a
/// plan identical to the stored record, a note already stored cut (a cut note fits as it
/// is), and a change that lies wholly past the cut, which a write would lose while cutting
/// a whole note the user saved for nothing.
fn stores_nothing_new<S: BookmarkStore>(store: &S, stored: &Bookmark, fitted: &Bookmark) -> bool {
    store.fit(stored.clone()).diff(fitted).is_empty()
}

/// Log that `fitted`, about to be written, cuts `planned`'s note to fit the request URL.
/// The store's own warning can't see it, since it is handed the note already cut.
fn warn_if_cut(planned: &Bookmark, fitted: &Bookmark) {
    if fitted.note != planned.note {
        warn!(
            "note trimmed to fit the request URL for {}: {} bytes -> {}",
            planned.url,
            planned.note.len(),
            fitted.note.len()
        );
    }
}

/// Print one bookmark's dry-run preview, its `[dry-run] <url>` line and then its detail
/// `lines`, and keep it in `kept`.
fn emit_preview(kept: &mut Vec<String>, url: &Url, lines: impl IntoIterator<Item = String>) {
    for line in std::iter::once(preview::header(url)).chain(lines) {
        outln!("{line}");
        kept.push(line);
    }
}

/// Close `untouchable` under "a group whose target is untouchable doesn't move, so its
/// members keep their own URLs occupied". Iterated to a fixpoint because each refusal can
/// occupy the URL that refuses the next, which is what makes the result independent of the
/// order groups are written in.
///
/// Called once before any write, and again after each failed write -- a failure strands its
/// group exactly as a refusal does, and leaving that un-propagated is worse than not
/// protecting the stranded record at all: it refuses a group whose own members are then
/// left unprotected in turn, trading one lost record for several.
fn propagate_refusals(
    group_order: &[&Url],
    groups: &HashMap<&Url, Vec<Member<'_>>>,
    untouchable: &mut HashMap<Url, Refusal>,
) {
    loop {
        let newly_occupied: Vec<Url> = group_order
            .iter()
            .filter(|key| untouchable.contains_key(**key))
            .filter_map(|key| groups.get(*key))
            .flatten()
            .map(|member| member.stored.url.clone())
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
}

#[cfg(test)]
mod tests;
