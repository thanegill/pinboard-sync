//! Dedup-key rules for non-source domains whose identity [`crate::keys::url_key`] would
//! clobber, in the query it drops or a case-sensitive path it lowercases.

use url::Url;

/// The suffix that keeps distinct resources apart under a host+path key:
/// - YouTube `watch?v=AAA`: the case-sensitive query value;
/// - the `youtu.be/<id>` short link: the original-case id.
///
/// `None` where dropping the query and lowercasing the path is right (tracking params,
/// GitHub `?tab=`). `host` and `path` are as `url_key` normalized them; `url` is the
/// original, for its case.
pub fn dedup_key_suffix(host: &str, path: &str, url: &Url) -> Option<String> {
    if crate::urls::host_matches(host, "youtube.com") && path == "/watch" {
        let id = youtube_video_id(url)?;
        return Some(format!("?v={id}"));
    }
    if crate::urls::host_matches(host, "youtu.be") {
        let id = url.path().trim_matches('/');
        if !id.is_empty() {
            return Some(format!("#{id}"));
        }
    }
    None
}

/// A YouTube `watch` URL's case-sensitive `v`.
fn youtube_video_id(url: &Url) -> Option<String> {
    url.query_pairs()
        .find(|(key, _)| key == "v")
        .map(|(_, value)| value.into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::url;

    fn suffix(s: &str) -> Option<String> {
        let u = url(s);
        #[expect(
            clippy::unwrap_in_result,
            reason = "a URL without a host is a broken test, not a `None` suffix"
        )]
        let host = u.host_str().unwrap().to_string();
        let path = u.path().trim_end_matches('/').to_ascii_lowercase();
        dedup_key_suffix(&host, &path, &u)
    }

    #[test]
    fn youtube_watch_preserves_video_id_case_sensitively() {
        assert_eq!(
            suffix("https://www.youtube.com/watch?v=dQw4w9WgXcQ").as_deref(),
            Some("?v=dQw4w9WgXcQ")
        );
        // A different-cased id is a different video and must yield a different suffix.
        assert_ne!(
            suffix("https://www.youtube.com/watch?v=dqw4w9wgxcq"),
            suffix("https://www.youtube.com/watch?v=dQw4w9WgXcQ")
        );
    }

    #[test]
    fn youtube_watch_drops_extra_query_params() {
        assert_eq!(
            suffix("https://www.youtube.com/watch?v=abc123&t=42&list=PL9").as_deref(),
            Some("?v=abc123")
        );
    }

    #[test]
    fn youtube_mobile_subdomain_matches() {
        assert_eq!(
            suffix("https://m.youtube.com/watch?v=abc123").as_deref(),
            Some("?v=abc123")
        );
    }

    #[test]
    fn non_watch_youtube_path_has_no_suffix() {
        assert_eq!(
            suffix("https://www.youtube.com/results?search_query=x"),
            None
        );
        assert_eq!(suffix("https://www.youtube.com/feed/subscriptions"), None);
    }

    #[test]
    fn non_youtube_host_has_no_suffix() {
        assert_eq!(suffix("https://vimeo.com/watch?v=abc123"), None);
        assert_eq!(suffix("https://github.com/owner/repo?tab=stars"), None);
    }

    #[test]
    fn youtu_be_short_link_preserves_id_case_sensitively() {
        // The 11-char base64url video id lives in the path, which url_key lowercases;
        // ids differing only in case are distinct videos and must yield distinct keys.
        assert_ne!(
            crate::keys::url_key(&url("https://youtu.be/AbCdEfGhIjK")),
            crate::keys::url_key(&url("https://youtu.be/abcdefghijk"))
        );
        // A normal host whose path is a case-insensitive identifier still collapses.
        assert_eq!(
            crate::keys::url_key(&url("https://github.com/Owner/Repo")),
            crate::keys::url_key(&url("https://github.com/owner/repo"))
        );
    }
}
