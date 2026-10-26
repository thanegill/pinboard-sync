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
//! the merge nor blocks it; its own plan is what preserves it. With no record staying at a
//! target, the plans must still agree with each other about `public`, or none of them
//! writes — a merge never picks a visibility. A rewrite's old URL is never
//! deleted when it is itself the target of another planned write in the same pass, so
//! colliding/chained rewrites can't clobber each other's record. Every refusal decided
//! before the first write is closed under [`propagate_refusals`], which makes that much
//! order-independent; a write that *fails* re-closes it too, but only over the groups not
//! yet written, so a failed run is the one case where order shows.
//! `run_pass` renders the dry-run lines and tallies into a [`PassOutcome`], which decides
//! on its own whether the run failed — see [`PassOutcome::into_result`].

use std::collections::{HashMap, HashSet};

use anyhow::{anyhow, Result};
use log::{debug, error, info, warn};
use time::OffsetDateTime;

use crate::bookmark::{AccountState, Bookmark, BookmarkStore};
use crate::source::SourceError;
use crate::timefmt::DateOpts;

mod merge;

use merge::{incumbent_of, merge_bookmarks};

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
pub trait CleanupPass {
    /// The end-state for `bookmark` as a [`Plan`] — distinguishing an answer from the
    /// source ([`Plan::Bookmark`], [`Plan::Unchanged`]) from a bookmark this pass does
    /// not handle ([`Plan::Skipped`]), which the driver needs to tell one dead link
    /// apart from an unreachable source.
    /// [`SourceError::Other`] marks a per-item failure (logged and counted; the
    /// pass continues with the next bookmark), while [`SourceError::ReauthRequired`],
    /// [`SourceError::RateLimited`] and [`SourceError::Blocked`] stop the pass: a dead
    /// credential, an exhausted quota and an edge block all fail every remaining lookup
    /// too. A plan's
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
    /// Under `dry_run`, the per-bookmark preview lines as printed (not the closing tally).
    /// Kept rather than only written to stdout so it can be asserted on: a preview is the
    /// one thing an operator reads before letting this tool loose on their account, and
    /// while it was a bare `println!` the whole of it could be deleted with every test
    /// still green.
    pub preview: Vec<String>,
    /// Rewrites this pass declined to make: the target must not be written at all (see
    /// [`Refusal`]), or a plan was held back over visibility — because it disagreed with
    /// the record staying at its target, or because nothing is staying there and the plans
    /// heading for it disagreed with each other, in which case every one of them is held
    /// back. Not a failure — leaving them alone is the safe
    /// outcome — but it is work that did not happen, so it is reported rather than only
    /// logged per-item.
    ///
    /// Counted per *group* when the target itself is untouchable (one refusal however many
    /// bookmarks were heading there) and per *plan* when a single member is held back over
    /// visibility, so this is a count of abandoned writes rather than of bookmarks.
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
/// Variants rather than one message because only one of them is an auth problem:
/// [`PassOutcome::into_result`] maps each to its own [`SourceError`], which is what lets
/// `main` fire the `--on-auth-failure` hook for [`Halt::Reauth`] and withhold it for the
/// others, since no credential change clears a quota or an edge block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Halt {
    /// A credential needs refreshing.
    Reauth(String),
    /// The service is refusing requests until a reset; no credential change helps.
    RateLimited(String),
    /// The service's edge is refusing the request by policy; no credential change helps.
    Blocked(String),
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

/// The halt an error calls for, or the error back when it is a per-item failure. Matched
/// exhaustively so a new [`SourceError`] variant has to choose a side here.
impl TryFrom<SourceError> for Halt {
    type Error = SourceError;
    fn try_from(error: SourceError) -> Result<Self, Self::Error> {
        match error {
            SourceError::ReauthRequired(message) => Ok(Halt::Reauth(message)),
            SourceError::RateLimited(message) => Ok(Halt::RateLimited(message)),
            SourceError::Blocked(message) => Ok(Halt::Blocked(message)),
            other @ SourceError::Other(_) => Err(other),
        }
    }
}

impl Halt {
    /// The operator-facing explanation, without a variant prefix.
    fn message(&self) -> &str {
        match self {
            Halt::Reauth(message) | Halt::RateLimited(message) | Halt::Blocked(message) => message,
        }
    }
}

impl PassOutcome {
    /// The run's verdict. A link we could not look up is *not* a failed run: it is
    /// logged, skipped, and every other bookmark is still cleaned up, so one
    /// permanently dead URL cannot wedge a scheduled service into a failed state
    /// forever. Three things do fail the run: a [`Halt`] (a dead credential, a rate
    /// limit or an edge block), a write we could not make (the destination is what we
    /// sync *to*), and lookups failing in the *majority*, which is an outage wearing a
    /// per-item disguise.
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
    /// halted credential must, a rate limit or an edge block must not.
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
                Halt::Blocked(_) => SourceError::Blocked(message),
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

    let mut outcome = PassOutcome::default();
    let Planned {
        pairs,
        mut untouchable,
    } = plan_all(bookmarks, dates, pass, &mut outcome).await;
    let (group_order, mut groups) = group_by_target(&pairs);
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
    let targets: HashSet<&url::Url> = group_order
        .iter()
        .filter(|key| !groups[*key].is_empty())
        .copied()
        .collect();

    // A group whose target is occupied won't move, so its *own* URL stays occupied too —
    // and a second group heading there would replace the record the refusal just saved.
    // Settled before any write so the outcome doesn't depend on group order.
    propagate_refusals(&group_order, &groups, &mut untouchable);

    for key in &group_order {
        let group = groups.remove(*key).expect("key came from group_order");
        // The visibility partition can empty a group outright, when every plan heading here
        // was held back. There is nothing left to refuse *or* write, so this comes before
        // the untouchable check: those plans were already counted as they were held back,
        // and reporting the group again would inflate `refused` past the number of writes
        // actually abandoned. It also keeps `merge_bookmarks` from being handed an empty
        // slice to index — today the survivor always supplies a first element, but that is
        // a property of two distant pieces of code agreeing, and this pass has a history of
        // one of them moving.
        if group.is_empty() {
            continue;
        }
        // The target must not be written, so leave the rewriting bookmark(s) at their old
        // URLs. Separate from the `targets` delete-guard — that protects a URL some *plan*
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
            (None, [(original, planned)]) => {
                write_single(store, original, planned, &targets, dry_run, &mut outcome).await
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
        // one of their URLs would overwrite it. Protect them and re-close the refusal set —
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
        println!("{dry_prefix}{} bookmark(s) would change.", outcome.changed);
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
/// `(stored, planned)` pair. A group holds its members in snapshot order.
type Member<'p> = (&'p Bookmark, &'p Bookmark);

/// Phase 1's result: each planned bookmark beside its stored record, and the URLs whose
/// record this pass could not establish.
struct Planned<'b> {
    pairs: Vec<(&'b Bookmark, Bookmark)>,
    /// A lookup failed there, or a halt stopped the pass short of it. Nothing may be
    /// written there at all. Owned rather than borrowed because the fixpoint grows it from
    /// the group keys.
    untouchable: HashMap<url::Url, Refusal>,
}

/// Plan every bookmark, tallying `reached`, `plan_failed` and `halted` into `outcome`, and
/// resolve each plan's date and privacy flags against its stored record.
async fn plan_all<'b, C: CleanupPass>(
    bookmarks: &'b [Bookmark],
    dates: DateOpts,
    pass: &C,
    outcome: &mut PassOutcome,
) -> Planned<'b> {
    let now = OffsetDateTime::now_utc();
    let mut pairs: Vec<(&Bookmark, Bookmark)> = Vec::new();
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
            Err(error) => match Halt::try_from(error) {
                // Every remaining lookup would fail the same way, so stop planning, but
                // fall through to write the plans already made rather than discard them.
                // This one and everything after it goes unplanned, so protect all of them:
                // a record we never looked at must not be written over.
                Ok(halt) => {
                    outcome.halted = Some(halt);
                    untouchable.extend(
                        bookmarks[index..]
                            .iter()
                            .map(|b| (b.url.clone(), Refusal::Unreadable)),
                    );
                    break;
                }
                // Log and skip a single failed plan so the rest of the pass still runs. Its
                // end-state is unknown, which makes it as untouchable as a skipped one:
                // another bookmark rewriting onto its URL would clobber the record.
                Err(e) => {
                    outcome.plan_failed += 1;
                    untouchable.insert(bookmark.url.clone(), Refusal::Unreadable);
                    error!("looking up bookmark {}: {e:#}", bookmark.url);
                    continue;
                }
            },
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
) -> (Vec<&'p url::Url>, HashMap<&'p url::Url, Vec<Member<'p>>>) {
    let mut group_order: Vec<&url::Url> = Vec::new();
    let mut groups: HashMap<&url::Url, Vec<Member<'_>>> = HashMap::new();
    for pair in pairs {
        let (original, planned) = (pair.0, &pair.1);
        let key = &planned.url;
        groups
            .entry(key)
            .or_insert_with(|| {
                group_order.push(key);
                Vec::new()
            })
            .push((original, planned));
    }
    (group_order, groups)
}

/// Every group's resident, resolved once, before any write. A resident is a record the live
/// view holds at a group's target that this pass neither planned (a bookmark is never a
/// resident of its own plan — merging with itself would resurrect what the plan removed)
/// nor failed to read. Precomputing is equivalent to looking each up lazily: group keys are
/// distinct, and every URL a write deletes was planned, so no write can change a record
/// read here.
fn residents_of<'p, S: AccountState>(
    store: &S,
    pairs: &[(&Bookmark, Bookmark)],
    group_order: &[&'p url::Url],
    untouchable: &HashMap<url::Url, Refusal>,
) -> HashMap<&'p url::Url, Bookmark> {
    // The stored URLs of the bookmarks this pass actually planned. They are excluded from
    // residency: a bookmark that plans to stay at (or move within) its own URL would
    // otherwise find itself sitting there and merge its stored record back into its own
    // plan, undoing the very changes the plan computed.
    let planned_originals: HashSet<&url::Url> =
        pairs.iter().map(|(original, _)| &original.url).collect();
    group_order
        .iter()
        .filter(|key| !untouchable.contains_key(**key) && !planned_originals.contains(*key))
        .filter_map(|key| store.resident(key).map(|resident| (*key, resident)))
        .collect()
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
    group_order: &[&'p url::Url],
    groups: &mut HashMap<&'p url::Url, Vec<Member<'p>>>,
    residents: &HashMap<&'p url::Url, Bookmark>,
    untouchable: &mut HashMap<url::Url, Refusal>,
    dry_run: bool,
    outcome: &mut PassOutcome,
) {
    let mut refused_movers: Vec<(url::Url, &url::Url, bool, Option<bool>)> = Vec::new();
    for key in group_order {
        if untouchable.contains_key(*key) {
            continue;
        }
        let members = &groups[key];
        // What the joining plans have to agree with. A record that stays at the target sets
        // it. With none — a target nothing occupies, or one whose occupant is itself moving
        // away — there is nothing to defer to, so the plans need only agree with *each
        // other*; when they don't, no member may write, because merging would then pick a
        // visibility on the user's behalf with nothing to justify the choice.
        let staying = residents
            .get(key)
            .map(|resident| resident.public)
            .or_else(|| incumbent_of(members, key).map(|at| members[at].1.public));
        let agreed = match staying {
            Some(public) => Some(public),
            None => members
                .first()
                .map(|(_, planned)| planned.public)
                .filter(|first| members.iter().all(|(_, p)| p.public == *first)),
        };
        let members = groups.get_mut(key).expect("key came from group_order");
        let (kept, refused): (Vec<_>, Vec<_>) = match agreed {
            Some(public) => members
                .iter()
                .copied()
                .partition(|(_, planned)| planned.public == public),
            None => (Vec::new(), members.clone()),
        };
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
                &[("", format!("(refused: {reason})"))],
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
fn refuse_group(key: &url::Url, reason: Refusal, dry_run: bool, outcome: &mut PassOutcome) {
    outcome.refused += 1;
    warn!("skipping cleanup write to {key}: {reason}");
    // Also on stdout: a dry run is a preview, and work the real run would refuse is part
    // of what the operator is previewing.
    if dry_run {
        emit_preview(
            &mut outcome.preview,
            key,
            &[("", format!("(refused: {reason})"))],
        );
    }
}

/// Write a lone plan with nothing stored at its target: skip it when nothing a write would
/// change differs, else rewrite it (deleting the old URL when it moved and no other plan
/// targets it) and tally the result. Returns the stored URLs a failed write left occupied,
/// for the caller to protect: empty unless the write failed.
async fn write_single<S: BookmarkStore>(
    store: &S,
    original: &Bookmark,
    planned: &Bookmark,
    targets: &HashSet<&url::Url>,
    dry_run: bool,
    outcome: &mut PassOutcome,
) -> Vec<url::Url> {
    // The written fields that differ; empty means nothing a write would change.
    let changes = original.diff(planned);
    if changes.is_empty() {
        return Vec::new();
    }
    // Delete the old URL only when it changed and no other planned write targets it;
    // otherwise that write's record lives there and must not be removed.
    let delete_old = planned.url != original.url && !targets.contains(&original.url);

    if dry_run {
        emit_preview(&mut outcome.preview, &original.url, &changes);
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
                planned.tags.join(" ")
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

/// Field-merge a group — with the record staying at its target, if any — into one record,
/// write it, and delete the absorbed URLs, so a later run sees a single bookmark at the
/// target and converges. Skipped when the merge reproduces what is stored there and there
/// is nothing to absorb. Returns the stored URLs a failed write left occupied, for the
/// caller to protect: empty unless the write failed.
async fn write_merge<S: BookmarkStore>(
    store: &S,
    key: &url::Url,
    group: &[Member<'_>],
    resident: Option<&Bookmark>,
    targets: &HashSet<&url::Url>,
    dry_run: bool,
    outcome: &mut PassOutcome,
) -> Vec<url::Url> {
    // The record that stays at this URL: an unplanned resident, else the member already
    // stored here. Whichever it is leads the merge, so its title and note are the base the
    // others extend. For an incumbent that is its *plan*, not its stored record — the plan
    // is this pass's intended end-state for it, and merging its stored record back in would
    // resurrect exactly what the plan removed.
    // A member is only lifted out of the tail when it is itself the survivor, so the two
    // can't disagree about which index to drop — the alternative, filtering by `incumbent`
    // independently of which branch `survivor` took, would silently lose a member if both
    // were ever `Some`.
    let (survivor, lifted) = match resident {
        Some(resident) => (Some(resident), None),
        None => match incumbent_of(group, key) {
            Some(at) => (Some(group[at].1), Some(at)),
            None => (None, None),
        },
    };
    let plans: Vec<&Bookmark> = survivor
        .into_iter()
        .chain(
            group
                .iter()
                .enumerate()
                .filter(|(at, _)| Some(*at) != lifted)
                .map(|(_, (_, planned))| *planned),
        )
        .collect();
    let mut merged = merge_bookmarks(&plans);
    if let Some(survivor) = survivor {
        // The survivor's own record-level state survives the merge's field rules: it is the
        // bookmark that stays here, and cleanup never re-shapes those (see `write_single`).
        // Its date in particular must not shift to a mover's under `merge_bookmarks`'s
        // earliest-wins rule, which is meant for two plans — with dating off, nothing may
        // re-date a bookmark at all. `public` needs no such restore: a group only reaches
        // this point when every member already matches it, so the all()-rule cannot change
        // it.
        merged.read_later = survivor.read_later;
        // `None` means the stored date was absent or wouldn't parse, and writing it back
        // omits `dt` entirely — under `replace=yes` Pinboard then re-dates the record to
        // now. Keep the earliest date the merge found instead of losing the record's age;
        // before this path such a bookmark was refused, never written.
        merged.timestamp = survivor.timestamp.or(merged.timestamp);
    }
    let target = &merged.url;

    // Absorbed URLs to delete: an original that is neither the target nor another planned
    // write's target (deleting the latter would clobber that write's record).
    let mut old_urls: Vec<&url::Url> = Vec::new();
    for (original, _) in group {
        if original.url != *target
            && !targets.contains(&original.url)
            && !old_urls.contains(&&original.url)
        {
            old_urls.push(&original.url);
        }
    }

    // Nothing to do when the merge reproduces what is already stored at the target and
    // there is nothing to absorb — otherwise a converged state would rewrite itself every
    // run. Compared against the *stored* record, which for an incumbent is not the plan
    // that led the merge.
    let stored_at_target = resident.or_else(|| lifted.map(|at| group[at].0));
    if stored_at_target.is_some_and(|stored| stored.diff(&merged).is_empty()) && old_urls.is_empty()
    {
        return Vec::new();
    }

    if dry_run {
        let note_value = match merged.note.is_empty() {
            true => "(removed)".to_string(),
            false => merged.note.clone(),
        };
        let mut detail = vec![
            ("title", merged.title.clone()),
            ("notes", note_value),
            ("tags", format!("[{}]", merged.tags.join(" "))),
        ];
        if let Some(date) = merged.timestamp.and_then(crate::timefmt::to_rfc3339) {
            detail.push(("date", date));
        }
        detail.push(("public", merged.public.to_string()));
        detail.push(("toread", merged.read_later.to_string()));
        detail.extend(old_urls.iter().map(|old| ("", format!("absorb {old}"))));
        emit_preview(&mut outcome.preview, target, &detail);
    }

    // Always through the store — see `write_single`.
    let write = store.apply_merge(&merged, &old_urls).await;
    match write.error {
        None => {
            outcome.changed += 1;
            let absorbed: Vec<String> = write.deleted.iter().map(|u| u.to_string()).collect();
            debug!("merged {target} <- [{}]", absorbed.join(" "));
            Vec::new()
        }
        // Half-applied: the merged record landed and an absorbed URL survived. Count the
        // change, exactly as the rewrite path does — the account did change — and name what
        // is actually wrong, since "merging into {target} failed" would send the operator
        // after a record that is fine.
        Some(e) if write.wrote => {
            outcome.changed += 1;
            outcome.write_failed += 1;
            let stranded = old_urls.len() - write.deleted.len();
            error!("merged {target} but {stranded} absorbed URL(s) remain: {e:#}");
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
                .map(|(original, _)| original.url.clone())
                .collect()
        }
    }
}

/// Print one bookmark's dry-run preview and keep it in `preview`. A `(label, value)` pair
/// renders as an aligned `label -> value`; an empty label renders the value alone, for the
/// lines that are not field changes (`absorb <url>`, `(refused: ...)`).
fn emit_preview(preview: &mut Vec<String>, url: &url::Url, detail: &[(&str, String)]) {
    let mut push = |line: String| {
        println!("{line}");
        preview.push(line);
    };
    push(format!("[dry-run] {url}"));
    for (label, value) in detail {
        push(match label.is_empty() {
            true => format!("          {value}"),
            false => format!("          {label:<6}-> {value}"),
        });
    }
}

/// Close `untouchable` under "a group whose target is untouchable doesn't move, so its
/// members keep their own URLs occupied". Iterated to a fixpoint because each refusal can
/// occupy the URL that refuses the next, which is what makes the result independent of the
/// order groups are written in.
///
/// Called once before any write, and again after each failed write — a failure strands its
/// group exactly as a refusal does, and leaving that un-propagated is worse than not
/// protecting the stranded record at all: it refuses a group whose own members are then
/// left unprotected in turn, trading one lost record for several.
fn propagate_refusals(
    group_order: &[&url::Url],
    groups: &HashMap<&url::Url, Vec<Member<'_>>>,
    untouchable: &mut HashMap<url::Url, Refusal>,
) {
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
}

#[cfg(test)]
mod tests;
