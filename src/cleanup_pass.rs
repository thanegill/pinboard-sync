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
//! record per URL. A plan whose note the merged record can't hold whole, or whose tags
//! would cut the note there, is held back and left where it is. A plan that disagrees about
//! visibility with the record staying at its target is refused and left where it is; with
//! no record staying, plans that disagree with each other about `public` are all refused,
//! since a merge never picks a visibility. A bookmark planned to move *away* from a target
//! neither leads that merge nor blocks it. A write the store could never send
//! ([`BookmarkStore::can_write`]) is refused before any write, its members left in place.
//!
//! A rewrite never deletes an old URL that another plan targets, so chained rewrites can't
//! clobber each other. Every refusal decided before the first write is closed under
//! [`propagate_refusals`], which makes it order-independent; a write that did not land
//! re-closes it over the groups not yet written, so that is the one case where order shows.
//! Groups are written in [`write_order`], each after the group moving its target's occupant
//! away, so a failed move is protected before anything lands on the record it left behind.
//! A cycle of moves, such as a swap, has no such order. The dry-run lines and tallies go
//! into a [`PassOutcome`], and [`PassOutcome::into_result`] decides whether the run failed.

use std::collections::{HashMap, HashSet};

use anyhow::{anyhow, Result};
use log::{debug, error, info, warn};
use time::OffsetDateTime;
use url::Url;

use crate::bookmark::{
    warn_note_cut, AccountState, Bookmark, BookmarkStore, TooLongToSend, WriteMode,
};
use crate::preview;
use crate::source::{Halt, SourceError};
use crate::stdout::outln;
use crate::timefmt::DateOpts;

mod merge;

use merge::{holds_block, incumbent_of, merge_bookmarks};

/// What a pass decided about one bookmark. [`Plan::Unchanged`] and [`Plan::Skipped`] both
/// write nothing, but only the first is an answer from the source, so only it shows the
/// source is reachable: that is how the driver tells one dead link from an outage.
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
    /// Not this pass's bookmark (a deep link, a foreign URL): no lookup was made, so it
    /// says nothing about whether the source is up.
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
    /// A plan's `timestamp` is only the candidate source date, which the driver resolves
    /// with the pass's [`DateOpts`]; `public`/`read_later` come from the stored bookmark
    /// (for a merge, by [`merge_bookmarks`]'s rules). The driver skips a plan that matches
    /// `bookmark`, so a pass needn't check for changes.
    async fn plan(&self, bookmark: &Bookmark) -> Result<Plan, SourceError>;
}

/// What one pass did, `plan_failed` (the source not answering) kept apart from
/// `write_failed` (the store refusing a write). `must_use`: [`PassOutcome::into_result`]
/// decides the exit code, so dropping an outcome drops its failures.
#[must_use]
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PassOutcome {
    /// Bookmarks written, or in a dry run that would be.
    pub changed: usize,
    /// In a dry run, the per-bookmark preview lines as printed, less the closing tally, so
    /// tests can assert on what an operator reads before a real run.
    pub preview: Vec<String>,
    /// Rewrites declined, reported though not a failure: a target that must not be written
    /// ([`Refusal`]), or a plan held back over visibility, a cut note or a write too long
    /// to send. It counts abandoned writes, none twice: one per untouchable group, unless a
    /// cycle of moves wrote over a member (a failed write), and one per held-back plan,
    /// unless its group is refused whole. A write refused as too long, up front
    /// ([`BookmarkStore::can_write`]) or when sent, adds one besides its held-back plans.
    pub refused: usize,
    /// Bookmarks the source answered for ([`Plan::Bookmark`] or [`Plan::Unchanged`], never
    /// [`Plan::Skipped`]): the evidence that it is reachable.
    pub reached: usize,
    /// Bookmarks whose `plan` failed to read the source.
    pub plan_failed: usize,
    /// Bookmarks whose write to the store failed, or whose record a cycle of moves wrote
    /// over before their own move could land.
    pub write_failed: usize,
    /// Set when the source stopped the pass. [`PassOutcome::into_result`] hands it back as
    /// [`SourceError::Halted`], so `cleanup` tells a dead credential from a quota or block.
    pub halted: Option<Halt>,
}

/// Why a rewrite was left in place rather than written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refusal {
    /// The target holds a record the pass couldn't read (a failed lookup, or one a halt
    /// stopped it short of), so what is stored there may be stale.
    Unreadable,
    /// The target holds a bookmark whose own rewrite was refused, so it never moved. A plan
    /// held back over visibility or a cut note lands here too: its reason is logged where
    /// it was refused, and the next group needs to know only that the URL is still taken.
    OccupiedByRefused,
    /// The target holds a bookmark whose own rewrite *failed*: apart from
    /// [`Refusal::OccupiedByRefused`], since the writes, not the plan, are the problem.
    WriteFailed,
    /// The target holds a bookmark whose own rewrite the store refused as too long
    /// ([`crate::bookmark::WriteOutcome::too_long`]), so it is still there.
    TooLong,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Refusal::Unreadable => "occupied by a bookmark this pass could not read",
            Refusal::OccupiedByRefused => "occupied by a rewrite that was itself refused",
            Refusal::WriteFailed => "occupied by a bookmark whose own rewrite failed",
            Refusal::TooLong => "occupied by a bookmark whose rewrite is too long for Pinboard",
        })
    }
}

impl PassOutcome {
    /// The run's verdict. A failed lookup alone is logged and skipped, so one dead URL
    /// can't wedge a scheduled service; a [`Halt`], a failed write, or a majority of failed
    /// lookups (`plan_failed > reached.max(1)`) fails it.
    ///
    /// A majority, not every lookup, since permanent per-item conditions arrive as
    /// [`Plan`]s, leaving `plan_failed` to post-retry 5xx, network trouble and unlabelled
    /// throttling. The `max(1)` keeps one failure survivable for a pass of one or two, like
    /// the discussion-link pass. It returns a [`SourceError`], so `cleanup` fires the hook
    /// for a dead credential, and only for that.
    pub fn into_result(self) -> Result<(), SourceError> {
        if let Some(halt) = self.halted {
            // The halt leads, but the message must still carry the other failures: these
            // counts are their only record.
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

/// Combine two passes' `(name, verdict)`s, each reached on its own so neither vouches for
/// the other. Both failures are reported, the one that doesn't lead after
/// `; and for <name>: `. The first leads unless the second outranks it: a dead credential
/// outranks any other halt, since only the outer variant survives to decide the hook, and
/// any halt an ordinary failure, since why a pass stopped is the headline.
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

/// Run `pass` over `bookmarks`, the source's slice with each URL once, as
/// [`AccountState::snapshot`] gives it; in a dry run, print what would change. `noun` names
/// the source in the log. Never fails outright: every failure, a dead credential included,
/// is tallied into the [`PassOutcome`].
///
/// Residency is asked of `store`, whose view covers the whole account as earlier passes
/// left it: a plan can target a URL outside its filter (an HN story moved to its article),
/// and `cleanup --all` runs three sources on one account.
pub async fn run_pass<S: BookmarkStore + AccountState, C: CleanupPass>(
    store: &S,
    bookmarks: &[Bookmark],
    noun: &str,
    dates: DateOpts,
    pass: &C,
) -> PassOutcome {
    let mode = store.write_mode();
    let dry_prefix = if mode.is_dry_run() { "[dry-run] " } else { "" };
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
    let held_back = HoldBacks {
        store,
        group_order: &group_order,
        residents: &residents,
        untouchable: &mut untouchable,
        mode,
        outcome: &mut outcome,
        held_from: HashMap::new(),
    }
    .hold_back(&mut groups);

    // The URLs a planned write still owns, whose old record a rewrite never deletes. After
    // the hold-backs: an emptied group owns nothing, and keeping its key would leave a
    // bookmark moving away from it at both URLs for good.
    let targets: HashSet<&Url> = group_order
        .iter()
        .filter(|key| !groups[*key].is_empty())
        .copied()
        .collect();
    let mut writer = GroupWriter {
        store,
        targets,
        written: HashSet::new(),
        mode,
        outcome,
    };

    propagate_refusals(&group_order, &groups, &mut untouchable);

    for key in &group_order {
        let group = groups.remove(*key).expect("key came from group_order");
        // One abandoned write per held-back plan, unless the group is refused whole.
        let held_here = held_back.get(*key).copied().unwrap_or(0);
        // Nothing left to refuse or write; refusing it would count its plans twice. This
        // also guarantees `write_merge` a member to lead.
        if group.is_empty() {
            writer.outcome.refused += held_here;
            continue;
        }
        // Unlike the `targets` delete-guard, this protects a URL no plan may land on.
        if let Some(reason) = untouchable.get(key) {
            writer.skip_group(key, &group, *reason, held_here);
            continue;
        }
        writer.outcome.refused += held_here;
        // Pinboard keys on URL, so what is stored at the target joins the group.
        let resident = residents.get(key);
        let stranded = match (resident, &group[..]) {
            (None, [Member { stored, planned }]) => writer.write_single(stored, planned).await,
            _ => writer.write_merge(key, &group, resident).await,
        };
        // A failed or refused write left its bookmarks in place, where a later group would
        // overwrite them: protect them and re-close the refusals over the groups left.
        if !stranded.is_empty() {
            for (url, reason) in stranded {
                untouchable.entry(url).or_insert(reason);
            }
            propagate_refusals(&group_order, &groups, &mut untouchable);
        }
    }

    let outcome = writer.outcome;
    if mode.is_dry_run() {
        outln!("{dry_prefix}{} bookmark(s) would change.", outcome.changed);
    } else {
        info!("done: updated {} bookmark(s)", outcome.changed);
    }
    if outcome.refused > 0 {
        warn!(
            "{} rewrite(s) not made, each logged above with why",
            outcome.refused
        );
    }
    outcome
}

/// One of the plans heading for a target URL, with its stored record. A group holds its
/// members in snapshot order.
#[derive(Clone, Copy)]
struct Member<'p> {
    stored: &'p Bookmark,
    planned: &'p Bookmark,
}

/// A bookmark this pass planned: its stored record, and the end-state planned for it.
struct PlannedPair<'b> {
    stored: &'b Bookmark,
    planned: Bookmark,
}

/// The plans beside their stored records, and the URLs the pass couldn't read.
struct Planned<'b> {
    pairs: Vec<PlannedPair<'b>>,
    /// Where a lookup failed or a halt stopped short: nothing may be written there. Owned,
    /// since the fixpoint grows it.
    untouchable: HashMap<Url, Refusal>,
}

/// Plan every bookmark, tallying `reached`, `plan_failed` and `halted`, and resolve each
/// plan's date and privacy flags against its stored record.
async fn plan_all<'b, C: CleanupPass>(
    bookmarks: &'b [Bookmark],
    dates: DateOpts,
    pass: &C,
    outcome: &mut PassOutcome,
) -> Planned<'b> {
    let now = OffsetDateTime::now_utc().truncate_to_second();
    let mut pairs: Vec<PlannedPair<'_>> = Vec::new();
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
            // Every later lookup would fail too: stop, but still write the plans made. A
            // record never looked at must not be written over, so protect this one and
            // every one after it.
            Err(SourceError::Halted(halt)) => {
                outcome.halted = Some(halt);
                untouchable.extend(
                    bookmarks[index..]
                        .iter()
                        .map(|b| (b.url.clone(), Refusal::Unreadable)),
                );
                break;
            }
            // The pass goes on. Its end-state is unknown, so no rewrite may land on it.
            Err(e @ SourceError::Other(_)) => {
                outcome.plan_failed += 1;
                untouchable.insert(bookmark.url.clone(), Refusal::Unreadable);
                error!("looking up bookmark {}: {e:#}", bookmark.url);
                continue;
            }
        };

        planned.timestamp = dates.cleanup_date(planned.timestamp, now, bookmark.timestamp);

        // Cleanup never re-shapes privacy, so a plan can't flip it.
        planned.public = bookmark.public;
        planned.read_later = bookmark.read_later;

        pairs.push(PlannedPair {
            stored: bookmark,
            planned,
        });
    }
    Planned { pairs, untouchable }
}

/// The plans grouped by target URL, with the groups' first-appearance order (and snapshot
/// order within each group).
fn group_by_target<'p>(
    pairs: &'p [PlannedPair<'p>],
) -> (Vec<&'p Url>, HashMap<&'p Url, Vec<Member<'p>>>) {
    let mut group_order: Vec<&Url> = Vec::new();
    let mut groups: HashMap<&Url, Vec<Member<'_>>> = HashMap::new();
    for PlannedPair { stored, planned } in pairs {
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

/// The groups in write order: each after the group moving its target's occupant away, so a
/// URL is vacated before a record lands on it, and a failed move leaves its record where
/// [`propagate_refusals`] still protects it. Otherwise first-appearance order, so a dry
/// run's preview is deterministic.
///
/// A cycle, such as a swap, has no safe order: one write lands on a record not yet moved,
/// which is lost if a later write in the cycle fails or is refused as too long.
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

/// Every group's resident, before any write: the live view's record at its target that the
/// pass neither planned nor failed to read. A bookmark is never a resident of its own plan:
/// merging its stored record back in would resurrect what the plan removed. Reading them up
/// front is safe: group keys are distinct, and a write deletes only planned URLs.
fn residents_of<'p, S: AccountState>(
    store: &S,
    pairs: &[PlannedPair<'_>],
    group_order: &[&'p Url],
    untouchable: &HashMap<Url, Refusal>,
) -> HashMap<&'p Url, Bookmark> {
    let planned_originals: HashSet<&Url> = pairs.iter().map(|pair| &pair.stored.url).collect();
    group_order
        .iter()
        .filter(|key| !untouchable.contains_key(**key) && !planned_originals.contains(*key))
        .filter_map(|key| store.resident(key).map(|resident| (*key, resident)))
        .collect()
}

/// The hold-back phases' shared state: the store and the groups' residents they read, the
/// refusals they mark, the preview they add to and how many plans each target lost.
struct HoldBacks<'h, 'p, S> {
    store: &'h S,
    group_order: &'h [&'p Url],
    residents: &'h HashMap<&'p Url, Bookmark>,
    untouchable: &'h mut HashMap<Url, Refusal>,
    mode: WriteMode,
    outcome: &'h mut PassOutcome,
    held_from: HashMap<Url, usize>,
}

/// A plan [`HoldBacks::hold_back_visibility_mismatches`] drops from its group.
struct HeldBack<'p> {
    /// The held-back bookmark's stored URL, which it keeps.
    mover: Url,
    target: &'p Url,
    mover_public: bool,
    /// Whether the record staying at `target` is public, or `None` when nothing stays there
    /// and the plans heading for it disagreed with each other.
    staying: Option<bool>,
}

impl<'p, S: BookmarkStore> HoldBacks<'_, 'p, S> {
    /// After leading each group with no record staying with a sendable mover where it has
    /// one, runs each hold-back on the groups the last left: visibility, cut notes, then
    /// writes that can't be sent. Returns how many plans each target lost.
    fn hold_back(mut self, groups: &mut HashMap<&'p Url, Vec<Member<'p>>>) -> HashMap<Url, usize> {
        self.lead_with_a_sendable_mover(groups);
        self.hold_back_visibility_mismatches(groups);
        self.hold_back_cut_notes(groups);
        self.hold_back_unwritable(groups);
        self.held_from
    }

    /// Drop from each group the plans that disagree about visibility with what stays at its
    /// target, marking each one's own URL occupied.
    ///
    /// A merge fuses notes and tags, so a bookmark joins only when it agrees with the
    /// record that stays about who may read it: widening would publish a private note,
    /// narrowing unshare a shared bookmark, and neither is ours to decide.
    ///
    /// Only that bookmark drops out, not its group: blocking the record that stays would
    /// let one mismatched duplicate freeze an unrelated bookmark's cleanup for good. Before
    /// the fixpoint, since a refused mover keeps its URL occupied as a refused group does.
    fn hold_back_visibility_mismatches(&mut self, groups: &mut HashMap<&'p Url, Vec<Member<'p>>>) {
        let residents = self.residents;
        let mut held_back: Vec<HeldBack> = Vec::new();
        for key in self.group_order {
            if self.untouchable.contains_key(*key) {
                continue;
            }
            let members = &groups[key];
            // The record that stays sets the visibility. With none (an empty target, or an
            // occupant moving away) the plans must agree with each other, or none writes: a
            // merge would pick a visibility with nothing to justify it.
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
            let describe = |public| if public { "public" } else { "private" };
            let (why, preview_tail) = match staying {
                Some(staying) => (
                    format!(
                        "it is {}, but {target} is {}",
                        describe(mover_public),
                        describe(staying)
                    ),
                    format!(", which is {}", describe(staying)),
                ),
                None => (
                    format!(
                        "the bookmarks moving there disagree about whether it should be \
                         public, and nothing is stored at {target} to settle it"
                    ),
                    " with bookmarks that disagree about visibility".to_string(),
                ),
            };
            self.hold_back_mover(
                &mover,
                target,
                &why,
                &preview_tail,
                Refusal::OccupiedByRefused,
            );
        }
    }

    /// Drop from each merging group the members that would make the merge lose text,
    /// marking each one's own URL occupied. The merge is built up from the record that
    /// leads it, adding the others in merge order and keeping each only when, with it, what
    /// the target will hold still has all of the note a lone rewrite of each member would
    /// keep, short of plan text the member doesn't store that the leading plan has too
    /// ([`Merge::would_lose`]), and the leading note keeps all it would keep on its own:
    /// other members' tags take room from the note too.
    ///
    /// Absorbing a member deletes it, and another group may write over its URL, so text the
    /// cut lost would be gone either way; a cut leading note loses its tail for good. Built
    /// up one member at a time, so a member that adds nothing is never held back for
    /// another's sake. Before any write, so the fixpoint closes over it and the outcome is
    /// order-free, a cycle of moves included.
    fn hold_back_cut_notes(&mut self, groups: &mut HashMap<&'p Url, Vec<Member<'p>>>) {
        let (store, residents) = (self.store, self.residents);
        for key in self.group_order {
            if self.untouchable.contains_key(*key) {
                continue;
            }
            let resident = residents.get(key);
            let members = groups.get_mut(key).expect("key came from group_order");
            // A lone plan with nothing stored at its target is a rewrite, which loses
            // nothing a rewrite of it alone wouldn't.
            if members.is_empty() || (resident.is_none() && members.len() == 1) {
                continue;
            }
            let lead = lead_of(members, key, resident.is_some());
            let mut merging: Vec<Member<'p>> = lead.map(|at| vec![members[at]]).unwrap_or_default();
            let lead_note = kept_text(store, merge_members(store, key, &merging, resident).kept());
            let mut held: Vec<Url> = Vec::new();
            for (at, member) in members.iter().enumerate() {
                if Some(at) == lead {
                    continue;
                }
                merging.push(*member);
                let merge = merge_members(store, key, &merging, resident);
                let (why, preview_tail) = if merge.would_lose(store, member) {
                    (
                        "its note would not all fit there, and merging it would lose the rest",
                        ", which has no room for its note",
                    )
                } else if merging.iter().any(|other| merge.would_lose(store, other))
                    || !kept_text(store, merge.kept()).starts_with(&lead_note)
                {
                    (
                        "merging it would cut the note of a bookmark merging there",
                        ", cutting a note there",
                    )
                } else {
                    continue;
                };
                merging.pop();
                self.hold_back_mover(
                    &member.stored.url,
                    key,
                    why,
                    preview_tail,
                    Refusal::OccupiedByRefused,
                );
                held.push(member.stored.url.clone());
            }
            members.retain(|member| !held.contains(&member.stored.url));
        }
    }

    /// Hold back the plans the store could never send ([`BookmarkStore::can_write`]),
    /// marking each one's URL occupied: the store would refuse them anyway, but only after
    /// the writes before them, a cycle of moves' included, and unseen by a dry run.
    ///
    /// A merge is built up from its lead as in [`Self::hold_back_cut_notes`], holding back
    /// each member whose joining would make it unsendable, so a duplicate can't freeze the
    /// cleanup of the record that stays. A group whose lead can't be sent even alone, or a
    /// lone plan that can't, is emptied, counting one refused write. Settled last, once the
    /// other hold-backs have fixed what each group merges, and before the fixpoint.
    fn hold_back_unwritable(&mut self, groups: &mut HashMap<&'p Url, Vec<Member<'p>>>) {
        let (store, residents) = (self.store, self.residents);
        for key in self.group_order {
            if self.untouchable.contains_key(*key) {
                continue;
            }
            let resident = residents.get(key);
            let members = groups.get_mut(key).expect("key came from group_order");
            if members.is_empty() {
                continue;
            }
            let sendable = |merging: &[Member<'p>]| {
                let merge = merge_members(store, key, merging, resident);
                if !merge.rewrite {
                    return Ok(());
                }
                store.can_write(&merge.fitted)
            };
            let mut held: Vec<Url> = Vec::new();
            let lead = lead_of(members, key, resident.is_some());
            let mut merging: Vec<Member<'p>> = lead.map(|at| vec![members[at]]).unwrap_or_default();
            let lead_sendable = match (resident, &members[..]) {
                (None, [Member { stored, planned }]) => {
                    let fitted = fit_plan(store, Some(stored), planned);
                    if stores_nothing_new(store, stored, &fitted) {
                        Ok(())
                    } else {
                        store.can_write(&fitted)
                    }
                }
                _ => sendable(&merging),
            };
            if let Err(why) = lead_sendable {
                self.refuse_unwritable(key, members, why);
                continue;
            }
            for (at, member) in members.iter().enumerate() {
                if Some(at) == lead {
                    continue;
                }
                merging.push(*member);
                let Err(why) = sendable(&merging) else {
                    continue;
                };
                merging.pop();
                self.hold_back_mover(
                    &member.stored.url,
                    key,
                    &format!("the merge would be {UNWRITABLE} ({why})"),
                    &format!(", {UNWRITABLE}"),
                    Refusal::TooLong,
                );
                held.push(member.stored.url.clone());
            }
            members.retain(|member| !held.contains(&member.stored.url));
        }
    }

    /// Put first, in each group with no record staying, the first mover the store could
    /// send alone ([`BookmarkStore::can_write`]). The first mover leads, so an unsendable
    /// one would otherwise take the rest down with it, and which did would depend on
    /// snapshot order; behind a sendable lead it may still join. Runs first: the hold-backs
    /// build on a lead.
    fn lead_with_a_sendable_mover(&self, groups: &mut HashMap<&'p Url, Vec<Member<'p>>>) {
        for key in self.group_order {
            let members = groups.get_mut(key).expect("key came from group_order");
            if self.untouchable.contains_key(*key)
                || self.residents.contains_key(key)
                || members.len() < 2
                || incumbent_of(members, key).is_some()
            {
                continue;
            }
            let sendable_alone = members.iter().position(|member| {
                let alone = merge_members(self.store, key, std::slice::from_ref(member), None);
                self.store.can_write(&alone.fitted).is_ok()
            });
            if let Some(at) = sendable_alone {
                members[..=at].rotate_right(1);
            }
        }
    }

    /// Hold `mover` back from its merge at `target` for `why`, counting it against the
    /// target and previewing `preview_tail` after the target's URL, and mark its own URL
    /// occupied for `reason`: it didn't move, so no other rewrite may land there.
    fn hold_back_mover(
        &mut self,
        mover: &Url,
        target: &Url,
        why: &str,
        preview_tail: &str,
        reason: Refusal,
    ) {
        *self.held_from.entry(target.clone()).or_default() += 1;
        warn!("not merging {mover} into {target}: {why}");
        if self.mode.is_dry_run() {
            emit_preview(
                &mut self.outcome.preview,
                mover,
                [preview::detail(format!(
                    "(refused: would merge into {target}{preview_tail})"
                ))],
            );
        }
        self.untouchable.entry(mover.clone()).or_insert(reason);
    }

    /// Log, preview and empty `members`, a group whose leading write to `key` could never
    /// be sent (`why`), counting one refused write and marking each member's URL occupied.
    fn refuse_unwritable(&mut self, key: &Url, members: &mut Vec<Member<'_>>, why: TooLongToSend) {
        let from: Vec<&str> = members
            .iter()
            .map(|member| member.stored.url.as_str())
            .collect();
        warn!(
            "skipping cleanup write to {key} from {}: {UNWRITABLE} ({why})",
            from.join(" ")
        );
        if self.mode.is_dry_run() {
            emit_preview(
                &mut self.outcome.preview,
                key,
                [preview::detail(format!("(refused: {UNWRITABLE})"))],
            );
        }
        *self.held_from.entry(key.clone()).or_default() += 1;
        for member in members.drain(..) {
            self.untouchable
                .entry(member.stored.url.clone())
                .or_insert(Refusal::TooLong);
        }
    }
}

/// Why [`HoldBacks::hold_back_unwritable`] refuses a write.
const UNWRITABLE: &str = "too long for Pinboard even with no note";

/// The index among `members` of the one that leads their merge at `key`: none when a
/// resident leads, else the incumbent, else the first.
fn lead_of(members: &[Member<'_>], key: &Url, resident: bool) -> Option<usize> {
    if resident {
        return None;
    }
    incumbent_of(members, key).or(Some(0))
}

/// What of `note` a store keeps: all of it, or the part before the cut.
fn kept_text<S: BookmarkStore>(store: &S, note: &str) -> String {
    store.kept_of_cut(note).unwrap_or(note).to_string()
}

/// The writing phase's shared state: the store, the URLs planned writes still own, the
/// write mode and the tally the writes go into.
struct GroupWriter<'s, 'p, S> {
    store: &'s S,
    /// URLs a write's record will live at, so a rewrite never deletes them.
    targets: HashSet<&'p Url>,
    /// The URLs this pass has written: a bookmark stored at one and still planned to move
    /// was written over by a cycle of moves.
    written: HashSet<Url>,
    mode: WriteMode,
    outcome: PassOutcome,
}

impl<S: BookmarkStore> GroupWriter<'_, '_, S> {
    /// Tally a write refused as too long, its members stored at `stored`: a refusal, unless
    /// a cycle of moves already wrote over one of them, whose record is then lost.
    fn tally_too_long<'u>(&mut self, what: &str, stored: impl IntoIterator<Item = &'u Url>) {
        match self.written_over(stored) {
            None => {
                self.outcome.refused += 1;
                warn!("{what}: the write is too long for Pinboard");
            }
            Some(lost) => {
                self.outcome.write_failed += 1;
                error!(
                    "{what}: the write is too long for Pinboard, after a cycle of moves \
                     wrote over the record at {lost}"
                );
            }
        }
    }

    /// The URLs among `stored` this pass already wrote, space-separated, or `None`: a cycle
    /// of moves wrote over them, so a group that then doesn't write loses them.
    fn written_over<'u>(&self, stored: impl IntoIterator<Item = &'u Url>) -> Option<String> {
        let lost: Vec<&str> = stored
            .into_iter()
            .filter(|url| self.written.contains(*url))
            .map(Url::as_str)
            .collect();
        (!lost.is_empty()).then(|| lost.join(" "))
    }

    /// Tally a group whose target must not be written, `held` plans held back from it: one
    /// refusal covering them, unless a cycle of moves wrote over a member
    /// ([`Self::written_over`]); then it is a failed write, and each held plan a refusal.
    fn skip_group(&mut self, key: &Url, group: &[Member<'_>], reason: Refusal, held: usize) {
        let Some(lost) = self.written_over(group.iter().map(|member| &member.stored.url)) else {
            self.refuse_group(key, reason);
            return;
        };
        self.outcome.write_failed += 1;
        self.outcome.refused += held;
        error!(
            "skipping cleanup write to {key}: {reason}, after a cycle of moves wrote over the \
             record at {lost}"
        );
    }

    /// Count, log and preview a group whose target must not be written at all.
    fn refuse_group(&mut self, key: &Url, reason: Refusal) {
        self.outcome.refused += 1;
        warn!("skipping cleanup write to {key}: {reason}");
        // A refusal is part of what a dry run previews.
        if self.mode.is_dry_run() {
            emit_preview(
                &mut self.outcome.preview,
                key,
                [preview::detail(format!("(refused: {reason})"))],
            );
        }
    }

    /// Write a lone plan with nothing stored at its target, unless it stores nothing new
    /// ([`stores_nothing_new`]), deleting the old URL when it moved and no other plan
    /// targets it. Returns the stored URLs a failed or too-long write left occupied, with
    /// why, for the caller to protect.
    async fn write_single(
        &mut self,
        original: &Bookmark,
        planned: &Bookmark,
    ) -> Vec<(Url, Refusal)> {
        let fitted = fit_plan(self.store, Some(original), planned);
        if stores_nothing_new(self.store, original, &fitted) {
            return Vec::new();
        }
        let changes = original.diff(&fitted);
        warn_if_cut(self.store, planned, &fitted);
        let planned = &fitted;
        let delete_old = planned.url != original.url && !self.targets.contains(&original.url);

        if self.mode.is_dry_run() {
            let lines = changes
                .iter()
                .map(|(label, value)| preview::field(label, value));
            emit_preview(&mut self.outcome.preview, &original.url, lines);
        }

        // Through the store even in a dry run: it withholds the network, and advances the
        // view a later pass previews from.
        let write = self
            .store
            .apply_update(planned, delete_old.then_some(&original.url))
            .await;
        if write.wrote {
            self.written.insert(planned.url.clone());
        }
        match write.error {
            None if write.too_long => {
                self.tally_too_long(&format!("not rewriting {}", original.url), [&original.url]);
                vec![(original.url.clone(), Refusal::TooLong)]
            }
            None => {
                self.outcome.changed += 1;
                debug!(
                    "updated {} -> {} [{}]",
                    original.url,
                    planned.url,
                    planned.tags.joined()
                );
                Vec::new()
            }
            // Half-applied: the record landed and only the old URL's delete failed.
            Some(e) if write.wrote => {
                self.outcome.changed += 1;
                self.outcome.write_failed += 1;
                error!("rewrote {} to {} but: {e:#}", original.url, planned.url);
                Vec::new()
            }
            // The move didn't happen, so the record is still at its old URL.
            Some(e) => {
                self.outcome.write_failed += 1;
                error!("rewriting bookmark {}: {e:#}", original.url);
                vec![(original.url.clone(), Refusal::WriteFailed)]
            }
        }
    }

    /// Field-merge a group, with any record staying at its target, write it and delete the
    /// absorbed URLs, so a later run sees one bookmark and converges. A merge storing
    /// nothing new ([`stores_nothing_new`]) only deletes, and with nothing to absorb is
    /// skipped. Returns the stored URLs a failed or too-long write left occupied, with why.
    async fn write_merge(
        &mut self,
        key: &Url,
        group: &[Member<'_>],
        resident: Option<&Bookmark>,
    ) -> Vec<(Url, Refusal)> {
        let Merge {
            merged,
            fitted,
            rewrite,
            ..
        } = merge_members(self.store, key, group, resident);
        let target = &fitted.url;

        // Never the target or another write's target, whose record that would clobber.
        let mut old_urls: Vec<&Url> = Vec::new();
        for Member { stored, .. } in group {
            if stored.url != *target && !self.targets.contains(&stored.url) {
                old_urls.push(&stored.url);
            }
        }

        // Nothing to write or absorb: returning keeps a converged state from rewriting
        // itself every run.
        if !rewrite && old_urls.is_empty() {
            return Vec::new();
        }
        if rewrite {
            warn_if_cut(self.store, &merged, &fitted);
        }
        let merged = &fitted;

        if self.mode.is_dry_run() {
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
            emit_preview(&mut self.outcome.preview, target, lines);
        }

        let done = if rewrite { "merged" } else { "absorbed into" };
        // Always through the store -- see `write_single`.
        let write = if rewrite {
            self.store.apply_merge(merged, &old_urls).await
        } else {
            self.store.apply_absorb(target, &old_urls).await
        };
        if rewrite && write.wrote {
            self.written.insert(target.clone());
        }
        match write.error {
            // Nothing was written or absorbed: every member is still at its URL.
            None if write.too_long => {
                self.tally_too_long(
                    &format!("not merging into {target}"),
                    group.iter().map(|member| &member.stored.url),
                );
                group
                    .iter()
                    .map(|member| (member.stored.url.clone(), Refusal::TooLong))
                    .collect()
            }
            None => {
                self.outcome.changed += 1;
                let absorbed: Vec<&str> = write.deleted.iter().map(Url::as_str).collect();
                debug!("{done} {target} <- [{}]", absorbed.join(" "));
                Vec::new()
            }
            // Half-applied: the record is at the target and an absorbed URL survived. Name
            // that, not the merge, or the operator chases a record that is fine.
            Some(e) if write.wrote => {
                if rewrite || !write.deleted.is_empty() {
                    self.outcome.changed += 1;
                }
                self.outcome.write_failed += 1;
                let stranded = old_urls.len() - write.deleted.len();
                error!("{done} {target} but {stranded} absorbed URL(s) remain: {e:#}");
                Vec::new()
            }
            // Every member, not `old_urls`, which leaves out the URLs other groups target:
            // exactly the ones in danger now.
            Some(e) => {
                self.outcome.write_failed += 1;
                error!("merging into {target}: {e:#}");
                group
                    .iter()
                    .map(|member| (member.stored.url.clone(), Refusal::WriteFailed))
                    .collect()
            }
        }
    }
}

/// A collision group merged into the one record its target would hold.
struct Merge<'p> {
    /// The field-merge, before the store's fit.
    merged: Bookmark,
    /// `merged` as the store would keep it: what a rewrite writes.
    fitted: Bookmark,
    /// Whether writing `fitted` stores anything new; if not, the stored record stays.
    rewrite: bool,
    /// The record stored at the target: the resident, or the incumbent's stored record.
    stored_at_target: Option<&'p Bookmark>,
    /// With no record staying, the member whose note the merge starts from: the merge is
    /// fitted against its stored record, as a lone rewrite of it would be.
    base: Option<&'p Url>,
    /// The note of the record that leads the merge: the resident's stored note, or the
    /// incumbent's or base's plan.
    lead_note: String,
    /// The stored record `fitted` was fitted against: the one at the target, or the base's.
    fitted_from: Option<&'p Bookmark>,
}

impl Merge<'_> {
    /// The note the target will hold: the stored one when the merge only absorbs.
    fn kept(&self) -> &str {
        match self.stored_at_target {
            Some(stored) if !self.rewrite => &stored.note,
            _ => &self.fitted.note,
        }
    }

    /// Whether merging `member` would lose text: it leaves its URL, and the target will not
    /// hold all of the note it would keep on its own, as a lone rewrite of it fits it. A
    /// plan longer than any write can store loses only what it could keep.
    fn would_lose<S: BookmarkStore>(&self, store: &S, member: &Member<'_>) -> bool {
        if member.stored.url == self.fitted.url || self.base == Some(&member.stored.url) {
            return false;
        }
        let alone = fit_plan(store, Some(member.stored), member.planned);
        let fitted_from = self.fitted_from.map_or("", |record| record.note.as_str());
        !holds_what_it_keeps(
            store,
            self.kept(),
            [fitted_from, &self.merged.note],
            &self.lead_note,
            &member.stored.note,
            &member.planned.note,
            &alone.note,
        )
    }
}

/// Whether `held`, the note a target keeps, holds `note`, a member's note as a lone rewrite
/// of it would keep it: each of its blocks, and for a cut note the start of the block the
/// cut fell in, credited only to that same block, held whole or cut inside it no earlier.
/// The notes each was cut from say which block a cut fell in: `held_sources` for the
/// target, and the member's `stored` and `planned` for `note`. The stored note comes
/// first in each: a cut text that starts it is its own, since [`fit_plan`] then put it in
/// place of the plan's, which may start the same way but go on in another block.
///
/// A block or fragment the note of the record leading the merge, `carried`, also has is
/// excused when the member does not store it itself: what a plan adds and the lead's plan
/// adds too is the lead's to keep, while what the member stores is lost with it. The
/// member stores the start of a block, as far as `note` keeps it, that the target does
/// not hold when a block of its stored note the target does not hold starts the same way
/// at the same place, or begins that kept start or begins with it anywhere, or when the
/// block begins with its stored cut: when `note` starts with the stored note's kept text,
/// only the block in the cut's place, the one it was cut from; otherwise any block.
fn holds_what_it_keeps<S: BookmarkStore>(
    store: &S,
    held: &str,
    held_sources: [&str; 2],
    carried: &str,
    stored: &str,
    planned: &str,
    note: &str,
) -> bool {
    let held = Kept::of(store, held, held_sources);
    let carried = Kept::of(store, carried, [carried, ""]);
    let stored_kept = Kept::of(store, stored, [stored, ""]);
    let kept = Kept::of(store, note, [stored, planned]);
    let note_text = store.kept_of_cut(note).unwrap_or(note);
    let stored_cut_index = store
        .kept_of_cut(stored)
        .filter(|stored_text| note_text.starts_with(stored_text))
        .map(|_| stored_kept.whole_count());
    let stores = |block: &str, index: usize, kept_of_block: &str| {
        let start_lost =
            |start: &str| !start.is_empty() && !held.holds_start(&kept, index, block, start);
        let stored_in_place = stored_kept
            .block_at(index)
            .filter(|own| !held.holds(own))
            .is_some_and(|own| {
                start_lost(kept_of_block[..shared_prefix_len(kept_of_block, own)].trim_end())
            });
        stored_in_place
            || (stored_cut_index.is_none_or(|own| own == index)
                && block.starts_with(stored_kept.cut)
                && start_lost(stored_kept.cut))
            || stored_kept
                .whole_blocks()
                .map(str::trim)
                .filter(|own| !held.holds(own))
                .any(|own| {
                    (kept_of_block.starts_with(own) && start_lost(own))
                        || (own.starts_with(kept_of_block) && start_lost(kept_of_block))
                })
    };
    for (index, raw) in kept.whole_blocks().enumerate() {
        let block = raw.trim();
        if !held.holds(block) && (stores(block, index, block) || !carried.holds(block)) {
            return false;
        }
    }
    if kept.cut.is_empty() {
        return true;
    }
    let index = kept.whole_count();
    match kept.cut_block {
        Some(block) => {
            held.holds_start(&kept, index, block, kept.cut)
                || (!stores(block, index, kept.cut)
                    && carried.holds_start(&kept, index, block, kept.cut))
        }
        // Cut from a note already cut there, so only this start of the block is known.
        // Blocks before it count in any order: a merge appends what its lead lacks.
        None => held.starts_like(&kept),
    }
}

/// The length of the longest common prefix of `a` and `b`, on a char boundary.
fn shared_prefix_len(a: &str, b: &str) -> usize {
    a.char_indices()
        .zip(b.chars())
        .find(|((_, x), y)| x != y)
        .map_or(a.len().min(b.len()), |((at, _), _)| at)
}

/// A note as a store keeps it: its whole blocks, and for a cut note the start of the block
/// the cut fell in (trimmed; empty when the note is whole or the cut fell between blocks),
/// with that block whole when a note it was cut from has it.
struct Kept<'n> {
    whole: &'n str,
    cut: &'n str,
    cut_block: Option<&'n str>,
}

impl<'n> Kept<'n> {
    /// `note` as kept, with `sources`, the notes it may have been cut from, to find its cut
    /// block in. A block of a source that was itself cut there is not known whole.
    fn of<S: BookmarkStore>(store: &S, note: &'n str, sources: [&'n str; 2]) -> Self {
        let Some(kept) = store.kept_of_cut(note) else {
            return Kept {
                whole: note,
                cut: "",
                cut_block: None,
            };
        };
        let (whole, cut) = kept.rsplit_once("\n\n").unwrap_or(("", kept));
        Kept {
            whole,
            cut: cut.trim(),
            cut_block: block_cut_from(kept, sources)
                .filter(|block| store.kept_of_cut(block).is_none()),
        }
    }

    /// Its whole blocks, untrimmed, in order.
    fn whole_blocks(&self) -> impl Iterator<Item = &'n str> {
        (!self.whole.is_empty())
            .then_some(self.whole)
            .into_iter()
            .flat_map(|whole| whole.split("\n\n"))
    }

    /// How many whole blocks it holds.
    fn whole_count(&self) -> usize {
        self.whole_blocks().count()
    }

    /// Its blocks, whole then the cut one, trimmed, in order.
    fn blocks(&self) -> impl Iterator<Item = &'n str> {
        self.whole_blocks()
            .map(str::trim)
            .chain((!self.cut.is_empty()).then_some(self.cut))
    }

    /// Its block at `index`, whole or the cut one, trimmed.
    fn block_at(&self, index: usize) -> Option<&'n str> {
        self.blocks().nth(index)
    }

    /// Whether it starts with `other`'s whole blocks, in any order and as many times each,
    /// then a block that starts with `other`'s cut, blank blocks aside ([`sorted_blocks`]).
    fn starts_like(&self, other: &Kept<'_>) -> bool {
        let own = sorted_blocks(other.whole_blocks());
        let mut blocks = self.blocks().filter(|block| !block.is_empty());
        sorted_blocks(blocks.by_ref().take(own.len())) == own
            && blocks
                .next()
                .is_some_and(|block| block.starts_with(other.cut))
    }

    /// Whether it holds `block` whole.
    fn holds(&self, block: &str) -> bool {
        holds_block(self.whole, block)
    }

    /// Whether it holds `block`, `note`'s block at `index`, up to the end of `fragment`, a
    /// start of it: whole, or cut inside that same block no earlier. Its cut is that same
    /// block when it was cut from it, or, when its own block isn't known whole, when
    /// `block` starts with its cut and its whole blocks are `note`'s before `index`, in any
    /// order and as many times each, blank ones aside ([`sorted_blocks`]).
    fn holds_start(&self, note: &Kept<'_>, index: usize, block: &str, fragment: &str) -> bool {
        if self.holds(block) {
            return true;
        }
        if self.cut.is_empty() || !self.cut.starts_with(fragment) {
            return false;
        }
        match self.cut_block {
            Some(own) => own == block,
            None => {
                block.starts_with(self.cut)
                    && sorted_blocks(self.whole_blocks())
                        == sorted_blocks(note.blocks().take(index))
            }
        }
    }
}

/// `blocks`, trimmed and sorted, less the blank ones, which take no place, as in
/// [`holds_block`]: a merge never appends one, and the lead's may sit anywhere.
fn sorted_blocks<'n>(blocks: impl Iterator<Item = &'n str>) -> Vec<&'n str> {
    let mut blocks: Vec<&str> = blocks
        .map(str::trim)
        .filter(|block| !block.is_empty())
        .collect();
    blocks.sort_unstable();
    blocks
}

/// The whole block of one of `sources` that `kept`, a cut note's kept text, was cut inside:
/// in the first source `kept` starts, the block running from the boundary before the cut.
fn block_cut_from<'n>(kept: &str, sources: [&'n str; 2]) -> Option<&'n str> {
    let source = sources
        .into_iter()
        .find(|source| source.starts_with(kept))?;
    let start = source[..kept.len()]
        .rfind("\n\n")
        .map_or(0, |boundary| boundary + 2);
    let block = &source[start..];
    Some(block.split("\n\n").next().unwrap_or(block).trim())
}

/// Merge `members` heading for `key`, with `resident` if one is stored there, and fit the
/// result to what `store` would keep.
fn merge_members<'p, S: BookmarkStore>(
    store: &S,
    key: &Url,
    members: &[Member<'p>],
    resident: Option<&'p Bookmark>,
) -> Merge<'p> {
    // The record that stays leads: the resident, else the member stored here, whose *plan*
    // leads, since merging its stored record back in would resurrect what the plan removed.
    let (survivor, lifted) = match resident {
        Some(resident) => (Some(resident), None),
        None => match incumbent_of(members, key) {
            Some(at) => (Some(members[at].planned), Some(at)),
            None => (None, None),
        },
    };
    let mut movers = members
        .iter()
        .enumerate()
        .filter(|(at, _)| Some(*at) != lifted)
        .map(|(_, member)| member);
    let base = if survivor.is_none() {
        movers.next()
    } else {
        None
    };
    let lead = survivor
        .or(base.map(|member| member.planned))
        .expect("run_pass skips an empty group");
    let lead_note = lead.note.clone();
    let rest: Vec<&Bookmark> = movers.map(|member| member.planned).collect();
    let mut merged = merge_bookmarks(lead, &rest);
    if let Some(survivor) = survivor {
        // The survivor keeps its own flags and date over the merge's rules: cleanup never
        // re-shapes them, and with dating off nothing may re-date a bookmark. `public`
        // already matches every member, so the all()-rule can't change it.
        merged.read_later = survivor.read_later;
        // With no stored date, an undated write would make Pinboard date it now: keep the
        // earliest the merge found.
        merged.timestamp = survivor.timestamp.or(merged.timestamp);
    }
    // The stored record, not the incumbent's plan: what the merge is fitted against.
    let stored_at_target = resident.or_else(|| lifted.map(|at| members[at].stored));
    let fitted_from = stored_at_target.or(base.map(|member| member.stored));
    let fitted = fit_plan(store, fitted_from, &merged);
    // A merge that stores nothing new only absorbs: rewriting would store nothing, and cut
    // a whole note longer than a write allows.
    let rewrite =
        !stored_at_target.is_some_and(|stored| stores_nothing_new(store, stored, &fitted));
    Merge {
        merged,
        fitted,
        rewrite,
        stored_at_target,
        base: base.map(|member| &member.stored.url),
        lead_note,
        fitted_from,
    }
}

/// `planned` as `store` would keep it. When the cut would leave only part of the stored
/// note (a plan appending to a note just under the limit), writing it would shorten the
/// stored note and keep none of the plan's text, so the plan's other fields go over the
/// stored note instead, fitted, whole if it still fits beside them. A note dropped whole,
/// with no room even for the cut marker, counts as such a cut.
fn fit_plan<S: BookmarkStore>(
    store: &S,
    stored: Option<&Bookmark>,
    planned: &Bookmark,
) -> Bookmark {
    let fitted = store.fit(planned.clone());
    let Some(stored) = stored else {
        return fitted;
    };
    let kept = if fitted.note.is_empty() {
        Some("")
    } else {
        store.kept_of_cut(&fitted.note)
    };
    let only_shortens =
        fitted.note != planned.note && kept.is_some_and(|kept| stored.note.starts_with(kept));
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

/// Whether writing `fitted`, from [`fit_plan`], stores nothing that writing `stored` back
/// wouldn't: an identical plan, a note already stored cut, or a change wholly past the cut,
/// which a write would lose while cutting a whole note for nothing.
fn stores_nothing_new<S: BookmarkStore>(store: &S, stored: &Bookmark, fitted: &Bookmark) -> bool {
    store.fit(stored.clone()).diff(fitted).is_empty()
}

/// Log that `fitted` cuts `planned`'s note: the store, handed it already cut, can't.
fn warn_if_cut<S: BookmarkStore>(store: &S, planned: &Bookmark, fitted: &Bookmark) {
    warn_note_cut(store, &planned.url, &planned.note, &fitted.note);
}

/// Print one bookmark's dry-run preview, its `[dry-run] <url>` line and then its detail
/// `lines`, and keep it in `kept`.
fn emit_preview(kept: &mut Vec<String>, url: &Url, lines: impl IntoIterator<Item = String>) {
    for line in std::iter::once(preview::header(url)).chain(lines) {
        outln!("{line}");
        kept.push(line);
    }
}

/// Close `untouchable` under "a refused group's members keep their URLs occupied", to a
/// fixpoint, since each refusal can occupy the URL that refuses the next: that makes the
/// result independent of write order.
///
/// Called before any write and after each write that didn't land, which strands its group
/// as a refusal does. Skipping that is worse than no protection: it refuses a group whose
/// own members are then left unprotected, one lost record for several.
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
