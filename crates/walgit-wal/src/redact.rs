//! Client-visible text hygiene.
//!
//! A store error may quote the presigned URL it failed to fetch, whose query
//! string carries `X-Amz-Credential` / `X-Amz-Signature` / `x-id=…`. Task
//! records, SSE packets and the `remote: …` lines a git client prints are
//! client-visible, so they all pass through this one redaction before leaving
//! the process. Scheme+host+path survive (enough to identify the object), the
//! query string and fragment do not (enough to fetch or sign it).

/// Remove the query string (and fragment) from every `scheme://…` URL in
/// `msg`, keeping scheme, host and path.
///
/// No regex dependency: scan for `://`, then copy until whitespace or a quote/
/// angle bracket, dropping everything from the first `?` or `#` in that token.
/// URLs in prose are usually parenthesised (`reqwest`'s Display prints
/// `for url (https://…)`); a closing `)` inside the dropped tail disappears
/// with it, which is cosmetic and never leaves a signed parameter behind.
/// Text that is not a URL is copied untouched (a bare `?` in a sentence stays).
pub fn redact_url_queries(msg: &str) -> String {
    let mut out = String::with_capacity(msg.len());
    let mut chars = msg.char_indices().peekable();
    while let Some((_, c)) = chars.next() {
        // `:` followed by `//` starts a URL; the scheme itself was copied by
        // earlier iterations of this loop.
        if c == ':' {
            let mut look = chars.clone();
            let slashes =
                matches!(look.next(), Some((_, '/'))) && matches!(look.next(), Some((_, '/')));
            if slashes {
                out.push_str("://");
                chars.next();
                chars.next();
                let mut dropping = false;
                while let Some(&(_, c)) = chars.peek() {
                    if is_url_terminator(c) {
                        break;
                    }
                    chars.next();
                    if c == '?' || c == '#' {
                        dropping = true;
                    }
                    if !dropping {
                        out.push(c);
                    }
                }
                continue;
            }
        }
        out.push(c);
    }
    out
}

fn is_url_terminator(c: char) -> bool {
    c.is_whitespace() || matches!(c, '"' | '\'' | '`' | '<' | '>')
}

#[cfg(test)]
mod tests {
    use super::redact_url_queries;

    #[test]
    fn redaction_touches_only_url_queries() {
        // A question mark in prose is not a query string.
        assert_eq!(
            redact_url_queries("what? nothing to see"),
            "what? nothing to see"
        );
        // A URL without a query is unchanged.
        assert_eq!(
            redact_url_queries("see https://host/a/b.git for details"),
            "see https://host/a/b.git for details"
        );
        // Multiple URLs in one message.
        assert_eq!(
            redact_url_queries("a https://h/1?X-Amz-Signature=a b https://h/2?x=1 c"),
            "a https://h/1 b https://h/2 c"
        );
        // Fragments go too, and quotes terminate a URL token.
        assert_eq!(
            redact_url_queries("url \"https://h/p?a=1#frag\" tail"),
            "url \"https://h/p\" tail"
        );
    }
}
