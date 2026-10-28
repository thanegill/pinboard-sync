//! How a collision group becomes one record: which member is the incumbent already
//! stored at the target, and the field-merge rules [`run_pass`](super::run_pass) writes.

use url::Url;

use super::Member;
use crate::bookmark::Bookmark;
use crate::tags::Tags;

/// The index within `members` of the group's *incumbent*: the member whose stored URL is
/// already the group's target. It is the record that stays at that URL, so it plays the
/// same role a resident does -- its plan leads the merge, and its own reading state and date
/// survive the merge's field rules. A resident is by definition unplanned, so a group has
/// at most one of the two, and being inside the pass's slice is an implementation detail
/// that must not decide whether a bookmark is protected.
pub(super) fn incumbent_of(members: &[Member<'_>], target: &Url) -> Option<usize> {
    members
        .iter()
        .position(|member| member.stored.url == *target)
}

/// Field-merge the PLANNED bookmarks of a collision group, `lead` then `rest` (all sharing
/// `lead`'s URL, in stable order), into one bookmark. Tags are an order-preserving union
/// (first occurrence wins, case-sensitive, empty tags dropped as by `Tags::push_all`);
/// the note is `lead`'s verbatim, extended with whichever blank-line-separated blocks of
/// the rest it doesn't already hold; title is the first non-empty; timestamp the earliest
/// present. Privacy never widens: the merge is `public` only if every merged member is
/// public (a single private member keeps it private), and `read_later` if any member is, so
/// a merge can never republish a private bookmark as public.
pub(super) fn merge_bookmarks(lead: &Bookmark, rest: &[&Bookmark]) -> Bookmark {
    let group = || std::iter::once(lead).chain(rest.iter().copied());
    let url = lead.url.clone();

    let mut tags = Tags::default();
    for b in group() {
        tags.push_all(&b.tags);
    }

    // The first member's note is the base, carried byte for byte: it is the record that
    // stays, and its spacing is the user's. Later members contribute only the
    // blank-line-separated blocks the base doesn't already have. Blocks are compared
    // trimmed so re-merging an already-merged note is a no-op even when the base ends in a
    // newline, which otherwise re-splits as a block the plain comparison can't recognise --
    // and any group whose old URL is never absorbed re-merges every run. A note long enough
    // that `PinboardClient::fit_extended` trims it comes back as a fragment no block
    // matches, so the cut blocks are re-appended here; the driver fits the merged record
    // before diffing it, which almost always trims them off again.
    let mut note = lead.note.clone();
    for b in rest {
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

    let title = group()
        .map(|b| b.title.as_str())
        .find(|t| !t.is_empty())
        .unwrap_or_default()
        .to_string();

    let timestamp = group().filter_map(|b| b.timestamp).min();

    Bookmark {
        url,
        title,
        note,
        tags,
        timestamp,
        public: group().all(|b| b.public),
        read_later: group().any(|b| b.read_later),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{stored_at, strings, tag_list, url as u};

    fn bookmark(url: &str) -> Bookmark {
        Bookmark {
            title: "Title".into(),
            note: "notes".into(),
            tags: tag_list(&["a", "b"]),
            timestamp: crate::timefmt::from_unix(1_577_836_800), // 2020-01-01T00:00:00Z
            ..stored_at(url)
        }
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

        let once = merge_bookmarks(&resident, &[&mover]);
        assert_eq!(once.note, "R\n\nM");

        // Second run: the resident now holds the merged note, the mover still holds "M".
        let twice = merge_bookmarks(&once, &[&mover]);
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

        let once = merge_bookmarks(&resident, &[&mover]);
        assert_eq!(once.note, "one\n\n\n\ntwo  \n\nthree");

        // And re-merging that unusual shape still has to settle, or the note grows every
        // run for any group whose old URL is never absorbed.
        let twice = merge_bookmarks(&once, &[&mover]);
        assert_eq!(twice.note, once.note, "the note must not grow on re-merge");
    }

    #[test]
    fn merge_bookmarks_keeps_a_later_members_block_verbatim() {
        // Carrying the first member's note byte for byte is only half of it: an absorbed
        // member's blocks are the user's writing too. Comparing them trimmed is what makes
        // re-merging settle, but appending them trimmed dedents a Markdown code block --
        // Pinboard renders notes as markup, so that changes what the note says.
        let mut resident = bookmark("https://t/");
        resident.note = "A".into();
        let mut mover = bookmark("https://t/");
        mover.note = "    let x = 1;\n    let y = 2;".into();

        let once = merge_bookmarks(&resident, &[&mover]);
        assert_eq!(once.note, "A\n\n    let x = 1;\n    let y = 2;");
        let twice = merge_bookmarks(&once, &[&mover]);
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

        let once = merge_bookmarks(&resident, &[&mover]);
        assert_eq!(once.note, "base\n\n\nextra");
        let twice = merge_bookmarks(&once, &[&mover]);
        assert_eq!(twice.note, once.note, "the note must not grow on re-merge");
    }

    #[test]
    fn merge_bookmarks_tag_union_is_case_sensitive() {
        // Pinboard treats tags case-sensitively, so `Rust` and `rust` are two tags and the
        // union must keep both. Folding them would silently drop one of the user's.
        let mut first = bookmark("https://t/");
        first.tags = tag_list(&["Rust"]);
        let mut second = bookmark("https://t/");
        second.tags = tag_list(&["rust", "Rust"]);

        assert_eq!(
            merge_bookmarks(&first, &[&second]).tags,
            strings(&["Rust", "rust"])
        );
    }

    #[test]
    fn merge_bookmarks_drops_an_empty_planned_tag() {
        let mut first = bookmark("https://t/");
        first.tags = tag_list(&["", "a"]);
        let mut second = bookmark("https://t/");
        second.tags = tag_list(&["b", ""]);

        assert_eq!(
            merge_bookmarks(&first, &[&second]).tags,
            strings(&["a", "b"])
        );
    }

    #[test]
    fn merge_bookmarks_does_not_lead_an_empty_note_with_a_blank_line() {
        // A survivor with no note contributes nothing to lead with, so the first real block
        // must start the note rather than being pushed down by a separator.
        let mut empty = bookmark("https://t/");
        empty.note = String::new();
        let mut has_note = bookmark("https://t/");
        has_note.note = "only note".into();

        assert_eq!(merge_bookmarks(&empty, &[&has_note]).note, "only note");
    }

    #[test]
    fn merge_bookmarks_applies_field_rules() {
        let ts_early = crate::timefmt::from_unix(1_000);
        let ts_late = crate::timefmt::from_unix(2_000);
        let a = Bookmark {
            url: u("https://collide/"),
            title: String::new(),
            note: "from A".into(),
            tags: tag_list(&["x", "shared"]),
            timestamp: ts_late,
            public: false,
            read_later: true,
        };
        let b = Bookmark {
            url: u("https://collide/"),
            title: "Title B".into(),
            note: "from B".into(),
            tags: tag_list(&["shared", "y"]),
            timestamp: ts_early,
            public: true,
            read_later: false,
        };
        let c = Bookmark {
            url: u("https://collide/"),
            title: "Title C".into(),
            // A duplicate of B's note must not be repeated in the join.
            note: "from B".into(),
            tags: tag_list(&["z"]),
            timestamp: None,
            public: false,
            read_later: false,
        };

        let merged = merge_bookmarks(&a, &[&b, &c]);
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
        let merged = merge_bookmarks(&a, &[&b]);
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
        assert!(!merge_bookmarks(&public, &[&private]).public);
        // Order-independent: private first is equally private.
        assert!(!merge_bookmarks(&private, &[&public]).public);
    }
}
