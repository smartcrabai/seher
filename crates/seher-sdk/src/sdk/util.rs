//! Small, dependency-free helpers shared across SDK backends.

/// Encode a session id into a filesystem-safe file name.
///
/// Session ids are usually UUIDs, but this prevents path-separator injection
/// when the id comes from an untrusted source (e.g. a library caller passing an
/// arbitrary resume id). Every character that is not an ASCII letter, digit,
/// hyphen, or underscore is replaced with `-`.
#[must_use]
pub fn encode_session_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_session_id_allows_alnum_dash_underscore() {
        assert_eq!(encode_session_id("abc-123_xyz"), "abc-123_xyz");
    }

    #[test]
    fn encode_session_id_replaces_special_chars() {
        assert_eq!(encode_session_id("../etc/passwd"), "---etc-passwd");
        assert_eq!(encode_session_id("a/b\\c.d"), "a-b-c-d");
    }
}
