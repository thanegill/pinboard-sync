//! Per-domain URL canonicalization rules for domains that are NOT sync sources
//! (i.e. anything other than reddit/github/hackernews). [`crate::source::url_key`]
//! delegates here for query-significant hosts: domains whose identity lives in the
//! query string, where dropping the query would collapse distinct resources onto one
//! dedup key.

use url::Url;

/// For a non-source domain whose identity lives in the query string, the suffix to
/// append to the host+path dedup key so distinct resources don't collapse to one key
/// (e.g. a YouTube `watch?v=AAA` vs `watch?v=BBB`). `None` for domains where dropping
/// the query is the right canonicalization (tracking params, GitHub `?tab=`, ...).
/// `host` and `path` are as normalized by `url_key` (path lowercased, trailing slash trimmed).
pub fn dedup_key_suffix(host: &str, path: &str, url: &Url) -> Option<String> {
    if crate::source::host_matches(host, "youtube.com") && path == "/watch" {
        let id = youtube_video_id(url)?;
        return Some(format!("?v={id}"));
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
}
