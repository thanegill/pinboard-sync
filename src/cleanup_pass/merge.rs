//! How a collision group becomes one record: which member is the incumbent already
//! stored at the target, and the field-merge rules [`run_pass`](super::run_pass) writes.

use url::Url;

use crate::bookmark::Bookmark;

/// The index within `members` of the group's *incumbent*: the member whose stored URL is
/// already the group's target. It is the record that stays at that URL, so it plays the
/// same role a resident does — its plan leads the merge, and its own reading state and date
/// survive the merge's field rules. A resident is by definition unplanned, so a group has
/// at most one of the two, and being inside the pass's slice is an implementation detail
/// that must not decide whether a bookmark is protected.
pub(super) fn incumbent_of(members: &[(&Bookmark, &Bookmark)], target: &Url) -> Option<usize> {
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
pub(super) fn merge_bookmarks(group: &[&Bookmark]) -> Bookmark {
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
