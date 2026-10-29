//! Keep camera credentials out of API responses, logs and events.

/// Placeholder returned instead of a stored password.
pub const MASK: &str = "********";

/// Remove `user:pass@` from a `scheme://user:pass@host/...` URL.
pub fn strip_userinfo(url: &str) -> String {
    let Some(scheme_end) = url.find("://") else {
        return url.to_string();
    };
    let rest = &url[scheme_end + 3..];
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    match rest[..authority_end].rfind('@') {
        Some(at) => format!("{}{}", &url[..scheme_end + 3], &rest[at + 1..]),
        None => url.to_string(),
    }
}

/// Strip userinfo from every URL embedded in free text (error messages
/// from ffmpeg/reqwest may echo the full camera URL).
pub fn scrub_credentials(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find("://") {
        // Start of the scheme: walk back over scheme characters.
        let scheme_start = rest[..pos]
            .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.'))
            .map_or(0, |i| i + 1);
        out.push_str(&rest[..scheme_start]);
        let tail = &rest[scheme_start..];
        let url_end = tail
            .find(|c: char| c.is_whitespace() || c == '"' || c == '\'')
            .unwrap_or(tail.len());
        out.push_str(&strip_userinfo(&tail[..url_end]));
        rest = &tail[url_end..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_userinfo() {
        assert_eq!(
            strip_userinfo("rtsp://u:p@host:554/s?x=@"),
            "rtsp://host:554/s?x=@"
        );
        assert_eq!(strip_userinfo("http://host/a@b"), "http://host/a@b");
        assert_eq!(strip_userinfo("no url"), "no url");
    }

    #[test]
    fn scrubs_text() {
        let msg =
            "Error opening input file rtsp://admin:se%40cret@10.0.0.5/live: 401, also http://x:y@h";
        assert_eq!(
            scrub_credentials(msg),
            "Error opening input file rtsp://10.0.0.5/live: 401, also http://h"
        );
    }
}
