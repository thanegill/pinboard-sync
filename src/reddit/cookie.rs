//! The Reddit session cookie: how a configured value is read (a bare session value, a
//! named `reddit_session`, a quoted value or a whole copied `Cookie` header), the header
//! actually sent, and what to tell the operator when Reddit refuses it.

use log::warn;

use crate::secrets::{ConfiguredSecret, SecretSpec};

/// The `Cookie` header sent for a configured cookie, as [`session_cookie`] shapes it.
pub(super) struct SessionCookie {
    pub(super) header: String,
    pub(super) source: &'static str,
}

impl SessionCookie {
    /// What to [redact](crate::http::redact) from a response body that is logged or
    /// quoted: the whole header and its `reddit_session` value, without the whitespace and
    /// quotes a server would drop before echoing it.
    pub(super) fn secrets(&self) -> Vec<&str> {
        let session = self
            .header
            .split(';')
            .find_map(session_value)
            .map(|session| session.trim().trim_matches('"'))
            .filter(|session| !session.is_empty());
        [Some(self.header.as_str()), session]
            .into_iter()
            .flatten()
            .collect()
    }
}

impl TryFrom<ConfiguredSecret> for SessionCookie {
    type Error = NoSession;
    fn try_from(cookie: ConfiguredSecret) -> Result<Self, Self::Error> {
        Ok(Self {
            header: session_cookie(&cookie.value, cookie.source).ok_or(NoSession)?,
            source: cookie.source,
        })
    }
}

/// The `Cookie` header for a configured cookie. A value that already names a
/// `reddit_session` cookie, alone or in a whole copied `Cookie` header, is sent with only
/// its first non-empty `reddit_session` (see [`with_one_session`]); a bare session value,
/// including one ending in base64 `=` padding, becomes `reddit_session=<value>`, since
/// Reddit only recognises the session by that name. Other cookies without a
/// `reddit_session` are sent with a warning: naming them would only produce a garbled
/// header. Every form goes out [without blank pairs](without_blank_pairs), so a stray `;`
/// doesn't turn a bare value into other cookies. See [`NoSession`] for what counts as no
/// cookie. `source` names the setting in that warning.
fn session_cookie(raw: &str, source: &str) -> Option<String> {
    let value = without_blank_pairs(raw);
    match CookieForm::try_from(value.as_str()).ok()? {
        CookieForm::Named => Some(with_one_session(&value)),
        CookieForm::Bare => Some(format!("reddit_session={value}")),
        CookieForm::OtherCookies => {
            warn!("{}", unnamed_cookie_warning(source));
            Some(value)
        }
    }
}

pub(super) fn unnamed_cookie_warning(source: &str) -> String {
    format!(
        "the Reddit cookie from {source} doesn't name a reddit_session cookie (expected the \
         bare value or reddit_session=<value>, with no `Cookie:` prefix); sending it as is, \
         and Reddit will likely refuse it"
    )
}

/// What to do about a session Reddit refused, naming the setting the cookie came from, or
/// the settings to use when none was configured.
pub(super) fn refused_cookie_advice(cookie_source: Option<&str>) -> String {
    match cookie_source {
        Some(source) => format!(
            "The cookie from {source} has likely expired or isn't a reddit_session value; \
             re-copy reddit_session from a logged-in browser."
        ),
        None => format!(
            "No Reddit cookie is configured; set {} to reddit_session from a logged-in \
             browser.",
            SecretSpec::REDDIT_COOKIE.settings()
        ),
    }
}

/// Whether `raw` configures a Reddit session at all (see [`NoSession`]), so secret
/// resolution can skip one that doesn't as it skips a blank value.
pub fn is_configured_cookie(raw: &str) -> bool {
    CookieForm::try_from(without_blank_pairs(raw).as_str()).is_ok()
}

/// `raw` [`unquoted`], without its [blank](is_blank) cookie pairs, so with no `;` at either
/// end or next to another.
fn without_blank_pairs(raw: &str) -> String {
    unquoted(raw)
        .split(';')
        .filter(|pair| !is_blank(pair))
        .collect::<Vec<_>>()
        .join(";")
        .trim()
        .to_string()
}

/// `raw` trimmed of whitespace and `;`, without one pair of surrounding double quotes, as
/// an env file or shell assignment can leave them. A lone quote at either end is kept: it
/// isn't a pair.
fn unquoted(raw: &str) -> &str {
    let is_edge = |c: char| c == ';' || c.is_whitespace();
    let trimmed = raw.trim_matches(is_edge);
    trimmed
        .strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
        .map_or(trimmed, |inner| inner.trim_matches(is_edge))
}

/// How a configured cookie value is shaped; see [`session_cookie`].
#[derive(Debug, PartialEq, Eq)]
enum CookieForm {
    /// Names a non-[blank](is_blank) `reddit_session` cookie, as its first pair or after a
    /// `;`.
    Named,
    /// A session value alone: no `;`, and no `=` but trailing base64 padding.
    Bare,
    /// `name=value` pairs, none of them `reddit_session`.
    OtherCookies,
}

/// The cookie value configures no session: [blank](is_blank), or naming `reddit_session`
/// only with blank values.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct NoSession;

/// The shape of a cookie value, or [`NoSession`]. Expects [`without_blank_pairs`]'s
/// output: a quoted `name=value` reads as other cookies, and so does a bare value with a
/// stray `;`.
impl TryFrom<&str> for CookieForm {
    type Error = NoSession;
    fn try_from(value: &str) -> Result<Self, Self::Error> {
        if is_blank(value) {
            return Err(NoSession);
        }
        let mut sessions = value.split(';').filter_map(session_value).peekable();
        if sessions.peek().is_some() {
            return sessions
                .any(|session| !is_blank(session))
                .then_some(CookieForm::Named)
                .ok_or(NoSession);
        }
        Ok(
            if value.contains(';') || value.trim_end_matches('=').contains('=') {
                CookieForm::OtherCookies
            } else {
                CookieForm::Bare
            },
        )
    }
}

/// Whether a cookie or session value holds nothing but whitespace, double quotes and the
/// `;`/`=` separators, so it has no `name=value` pair a session could be.
fn is_blank(value: &str) -> bool {
    value
        .trim_matches(|c: char| matches!(c, '"' | ';' | '=') || c.is_whitespace())
        .is_empty()
}

/// The value of a `reddit_session=<value>` cookie pair, or `None` for any other pair.
fn session_value(pair: &str) -> Option<&str> {
    pair.trim_start().strip_prefix("reddit_session=")
}

/// `value`'s cookie pairs, in order, without any `reddit_session` but the first non-empty
/// one. Sent several, Reddit would pick one by its own rule, so an empty pair could mask
/// the real session. Expects [`without_blank_pairs`]'s output.
fn with_one_session(value: &str) -> String {
    let mut session_kept = false;
    let pairs: Vec<&str> = value
        .split(';')
        .filter(|pair| match session_value(pair) {
            None => true,
            Some(session) if session_kept || is_blank(session) => false,
            Some(_) => {
                session_kept = true;
                true
            }
        })
        .collect();
    pairs.join(";").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reddit::TEST_COOKIE_SOURCE;

    /// Asserts that each `(raw, sent)` cookie counts as a session and is sent as `sent`.
    fn assert_sends(cases: &[(&str, &str)]) {
        for (raw, sent) in cases {
            assert!(is_configured_cookie(raw), "{raw:?}");
            assert_eq!(
                session_cookie(raw, TEST_COOKIE_SOURCE).as_deref(),
                Some(*sent),
                "{raw:?}"
            );
        }
    }

    /// Asserts that each configured value sets no session, so nothing is sent for it.
    fn assert_no_session(values: &[&str]) {
        for unset in values {
            assert_eq!(session_cookie(unset, TEST_COOKIE_SOURCE), None, "{unset:?}");
            assert!(!is_configured_cookie(unset), "{unset:?}");
        }
    }

    #[test]
    fn a_cookie_secret_is_the_session_as_a_server_would_echo_it() {
        let quoted = SessionCookie {
            header: "reddit_session=\"abc\" ; a=1".into(),
            source: TEST_COOKIE_SOURCE,
        };
        assert_eq!(quoted.secrets(), [quoted.header.as_str(), "abc"]);
        let unnamed = SessionCookie {
            header: "a=1".into(),
            source: TEST_COOKIE_SOURCE,
        };
        assert_eq!(unnamed.secrets(), ["a=1"]);
    }

    #[test]
    fn names_a_bare_session_value_and_keeps_named_cookies() {
        let cases = [
            ("  abc.def-ghi\n", "reddit_session=abc.def-ghi"),
            ("YWJjZA=", "reddit_session=YWJjZA="),
            ("YWJj==", "reddit_session=YWJj=="),
            ("reddit_session=abc\n", "reddit_session=abc"),
            ("reddit_session=YWJj==", "reddit_session=YWJj=="),
            (
                " reddit_session=abc; token_v2=x ",
                "reddit_session=abc; token_v2=x",
            ),
            (" a=1;reddit_session=abc ", "a=1;reddit_session=abc"),
            ("a=1;  reddit_session=abc", "a=1;  reddit_session=abc"),
            ("a=1; b=2", "a=1; b=2"),
            ("\"abc\"", "reddit_session=abc"),
            (" \"reddit_session=abc\"\n", "reddit_session=abc"),
        ];
        assert_sends(&cases);
        assert_no_session(&[
            " \n",
            "\"\"",
            "reddit_session=",
            " reddit_session= \n",
            "\"reddit_session=\"",
            "a=1; reddit_session=",
            "reddit_session= ; a=1",
        ]);
        assert!(is_configured_cookie("abc"));
    }

    #[test]
    fn strips_only_a_matched_quote_pair_and_treats_bare_quotes_as_unset() {
        let cases = [
            ("\"abc", "reddit_session=\"abc"),
            ("abc\"", "reddit_session=abc\""),
            ("\"\"abc\"\"", "reddit_session=\"abc\""),
            (
                "reddit_session=\"; reddit_session=abc",
                "reddit_session=abc",
            ),
        ];
        assert_sends(&cases);
        assert_no_session(&[
            "\"",
            " \" \n",
            "\"\"\"",
            "\"\"\"\"",
            "\" \"",
            "reddit_session=\"",
            "reddit_session=\"\"",
            "a=1; reddit_session=\" \"",
        ]);
    }

    #[test]
    fn a_cookie_of_only_separators_is_no_cookie() {
        assert_no_session(&[
            ";",
            "=",
            "=;",
            " ; = ;\n",
            "\"=\"",
            "reddit_session==",
            "reddit_session=;=",
            "a=1; reddit_session= =",
        ]);
        let cases = [
            ("reddit_session==; reddit_session=abc", "reddit_session=abc"),
            ("reddit_session=abc; =; ;", "reddit_session=abc"),
            ("abc=", "reddit_session=abc="),
            ("abc;", "reddit_session=abc"),
            (" ;abc; \n", "reddit_session=abc"),
            ("YWJj==;", "reddit_session=YWJj=="),
            ("\"abc;\"", "reddit_session=abc"),
            ("\"abc\";", "reddit_session=abc"),
            (";;reddit_session=x;", "reddit_session=x"),
            ("a=1;;", "a=1"),
            ("a=1; ;b=2;", "a=1;b=2"),
        ];
        assert_sends(&cases);
    }

    #[test]
    fn sends_only_the_first_non_empty_session_whatever_the_order() {
        let cases = [
            ("reddit_session=; reddit_session=abc", "reddit_session=abc"),
            ("reddit_session=abc; reddit_session=", "reddit_session=abc"),
            (
                "reddit_session=abc;reddit_session=def",
                "reddit_session=abc",
            ),
            (
                "a=1; reddit_session= ; reddit_session=abc; b=2; reddit_session=def",
                "a=1; reddit_session=abc; b=2",
            ),
            ("reddit_session=abc;", "reddit_session=abc"),
            ("a=1;; reddit_session=abc", "a=1; reddit_session=abc"),
        ];
        assert_sends(&cases);
        assert!(!is_configured_cookie("reddit_session=; reddit_session= "));
    }

    #[test]
    fn names_a_session_cookie_only_by_its_name() {
        let cases = [
            ("abc.def-ghi", CookieForm::Bare),
            ("YWJjZA=", CookieForm::Bare),
            ("YWJj==", CookieForm::Bare),
            ("reddit_session=abc", CookieForm::Named),
            ("reddit_session=abc; token_v2=x", CookieForm::Named),
            ("a=1; reddit_session=abc", CookieForm::Named),
            ("a=1;reddit_session=abc", CookieForm::Named),
            ("a=reddit_session=1", CookieForm::OtherCookies),
            ("a=1; b=x reddit_session=2", CookieForm::OtherCookies),
            ("a=1; b=2", CookieForm::OtherCookies),
            ("token_v2=x", CookieForm::OtherCookies),
        ];
        for (trimmed, form) in cases {
            assert_eq!(CookieForm::try_from(trimmed), Ok(form), "{trimmed:?}");
        }
        assert_eq!(CookieForm::try_from(""), Err(NoSession));
    }
}
