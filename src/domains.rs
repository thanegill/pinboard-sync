//! Per-domain URL canonicalization rules for domains that are NOT sync sources
//! (i.e. anything other than reddit/github/hackernews). [`crate::source::url_key`]
//! delegates here for hosts carrying a case- or query-significant identifier that its
//! generic normalization would otherwise clobber: domains whose identity lives in the
//! query string (dropping the query would collapse distinct resources), or in a
//! case-sensitive path segment (lowercasing the path would).

use url::Url;

/// For a non-source domain whose identity a plain host+path key would clobber, the
/// suffix to append so distinct resources don't collapse to one key. Two cases:
/// - Identity in the query string, which `url_key` drops (e.g. YouTube `watch?v=AAA`
///   vs `watch?v=BBB`) — append the case-sensitive query value.
/// - Identity in a case-sensitive path segment, which `url_key` lowercases (e.g. the
///   `youtu.be/<id>` short link, whose 11-char base64url video id is case-sensitive) —
///   append the original-case path so `youtu.be/AbC` and `youtu.be/abc` stay distinct.
///
/// `None` for domains where dropping the query and lowercasing the path is the right
/// canonicalization (tracking params, GitHub `?tab=`/`Owner/Repo`, ...). `host` and
/// `path` are as normalized by `url_key` (path lowercased, trailing slash trimmed);
/// `url` is the original, so case can be recovered from it.
pub fn dedup_key_suffix(host: &str, path: &str, url: &Url) -> Option<String> {
    if crate::source::host_matches(host, "youtube.com") && path == "/watch" {
        let id = youtube_video_id(url)?;
        return Some(format!("?v={id}"));
    }
    if crate::source::host_matches(host, "youtu.be") {
        let id = url.path().trim_matches('/');
        if !id.is_empty() {
            return Some(format!("#{id}"));
        }
    }
    None
}

/// The `v` value from a YouTube `watch` URL's query, case-sensitively (video ids are
/// case-sensitive). `None` if absent.
fn youtube_video_id(url: &Url) -> Option<String> {
    url.query_pairs()
        .find(|(key, _)| key == "v")
        .map(|(_, value)| value.into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    fn suffix(s: &str) -> Option<String> {
        let u = url(s);
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
            crate::source::url_key(&url("https://youtu.be/AbCdEfGhIjK")),
            crate::source::url_key(&url("https://youtu.be/abcdefghijk"))
        );
        // A normal host whose path is a case-insensitive identifier still collapses.
        assert_eq!(
            crate::source::url_key(&url("https://github.com/Owner/Repo")),
            crate::source::url_key(&url("https://github.com/owner/repo"))
        );
    }
}
