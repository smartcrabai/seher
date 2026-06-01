use std::sync::LazyLock;

use regex::Regex;
use unicode_normalization::UnicodeNormalization;
use unicode_width::UnicodeWidthChar;

const MAX_NEEDLE_CELLS: usize = 32;
const MAX_RESET_INFO_LENGTH: usize = 80;

fn make_regex(pattern: &str) -> Regex {
    Regex::new(pattern).unwrap_or_else(|e| panic!("invalid static regex: {e}"))
}

// ── ANSI escape stripping ────────────────────────────────────────────────────

static ANSI_ESCAPE: LazyLock<Regex> = LazyLock::new(|| make_regex(r"\x1b\[[\d;?]*[A-Za-z]"));

fn strip_ansi(s: &str) -> String {
    ANSI_ESCAPE.replace_all(s, "").into_owned()
}

// ── Session-limit detection ──────────────────────────────────────────────────

static SESSION_LIMIT_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    make_regex(
        r"(?i)you['']ve[ \t]+hit[ \t]+your[ \t]+(?:weekly[ \t]+|usage[ \t]+)?(?:session|usage)[ \t]+limit",
    )
});

static RESET_INFO_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| make_regex(r"(?i)resets\s+([^\n\r\xB7]+)"));

/// Returns `Some(reset_info)` when the screen capture contains the Claude TUI
/// session-limit banner. `reset_info` is `None` when the banner is present but
/// the reset time cannot be extracted or is too long.
///
/// Only call this BEFORE the user's prompt is on screen to avoid false positives.
#[expect(
    clippy::option_option,
    reason = "outer Option = banner present, inner Option = reset time extractable"
)]
pub fn detect_session_limit(screen: &str) -> Option<Option<String>> {
    let clean = strip_ansi(screen);
    if !SESSION_LIMIT_PATTERN.is_match(&clean) {
        return None;
    }
    let reset_info = RESET_INFO_PATTERN.captures(&clean).and_then(|cap| {
        let raw = cap[1].trim();
        if raw.is_empty() || raw.len() > MAX_RESET_INFO_LENGTH {
            None
        } else {
            Some(raw.to_string())
        }
    });
    Some(reset_info)
}

// ── Paste-visible detection ──────────────────────────────────────────────────

// Collapsed paste patterns — Claude TUI collapses long pastes into a citation.
static COLLAPSED_PASTE_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
        make_regex(r"\[Pasted\s+text\s+#\d+\s+\+\d+\s+lines\]"),
        make_regex(r"\[Pasted\s+#\d+\]"),
        // Tentative Japanese localization patterns
        make_regex(r"\[ペースト\s*#?\d*\s*\+\d+\s*行\]"),
        make_regex(r"\[貼り付け\s*#?\d*\s*\+\d+\s*行\]"),
    ]
});

// Characters trimmed from the trailing end of a prompt before building the suffix needle.
// Markdown decoration, CJK/Latin punctuation, whitespace.
static TRAILING_TRIM_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| make_regex(r"(?u)[\s*`_~。．、，！？!?,\\.;:　・]+$"));

static LEADING_TRIM_PATTERN: LazyLock<Regex> = LazyLock::new(|| make_regex(r"(?u)^[\s*`_~]+"));

#[derive(Debug, Clone)]
pub struct PasteNeedles {
    pub prefix: String,
    pub suffix: String,
}

#[must_use]
pub fn build_needles(prompt: &str) -> PasteNeedles {
    PasteNeedles {
        prefix: prefix_needle(prompt),
        suffix: suffix_needle(prompt),
    }
}

fn suffix_needle(prompt: &str) -> String {
    let trimmed = prompt.trim_end();
    if trimmed.is_empty() {
        return String::new();
    }
    let stripped = TRAILING_TRIM_PATTERN.replace(trimmed, "");
    let source = if stripped.is_empty() {
        trimmed
    } else {
        &stripped
    };
    let last_line = source.split('\n').next_back().unwrap_or("");
    if last_line.is_empty() {
        return String::new();
    }
    take_suffix_by_cell_width(last_line, MAX_NEEDLE_CELLS)
}

fn prefix_needle(prompt: &str) -> String {
    let trimmed = prompt.trim_start();
    if trimmed.is_empty() {
        return String::new();
    }
    let stripped = LEADING_TRIM_PATTERN.replace(trimmed, "");
    let source = if stripped.is_empty() {
        trimmed
    } else {
        &stripped
    };
    let first_line = source.split('\n').next().unwrap_or("");
    if first_line.is_empty() {
        return String::new();
    }
    take_prefix_by_cell_width(first_line, MAX_NEEDLE_CELLS)
}

/// Cell width of a single Unicode scalar value.
/// Zero-width chars (combining marks, ZWJ, BOM, variation selectors) → 0.
/// Others use `unicode-width` which covers CJK wide chars (2 cells).
fn char_cell_width(c: char) -> usize {
    // Zero-width ranges from the TS implementation
    let cp = c as u32;
    if (0x0300..=0x036f).contains(&cp)   // combining diacriticals
        || (0x1ab0..=0x1aff).contains(&cp)
        || (0x1dc0..=0x1dff).contains(&cp)
        || (0x20d0..=0x20ff).contains(&cp)
        || (0xfe20..=0xfe2f).contains(&cp)
        || (0x200b..=0x200f).contains(&cp)  // ZWSP/ZWNJ/ZWJ/LRM/RLM
        || cp == 0x2060   // WORD JOINER
        || cp == 0xfeff   // BOM
        || (0xfe00..=0xfe0f).contains(&cp)  // variation selectors
        || (0xe0100..=0xe01ef).contains(&cp)
    {
        return 0;
    }
    c.width().unwrap_or(1)
}

fn take_suffix_by_cell_width(s: &str, max_cells: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut width = 0usize;
    let mut start = chars.len();
    for i in (0..chars.len()).rev() {
        let w = char_cell_width(chars[i]);
        if width + w > max_cells {
            break;
        }
        width += w;
        start = i;
    }
    // Skip leading zero-width chars
    while start < chars.len() && char_cell_width(chars[start]) == 0 {
        start += 1;
    }
    chars[start..].iter().collect()
}

fn take_prefix_by_cell_width(s: &str, max_cells: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut begin = 0;
    while begin < chars.len() && char_cell_width(chars[begin]) == 0 {
        begin += 1;
    }
    let mut width = 0usize;
    let mut end = begin;
    for (i, &c) in chars.iter().enumerate().skip(begin) {
        let w = char_cell_width(c);
        if width + w > max_cells {
            break;
        }
        width += w;
        end = i + 1;
    }
    chars[begin..end].iter().collect()
}

/// Normalize text for fuzzy matching: strip ANSI → NFC → remove all whitespace.
#[must_use]
pub fn normalize_for_match(s: &str) -> String {
    let no_ansi = strip_ansi(s);
    let nfc: String = no_ansi.nfc().collect();
    nfc.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Returns `true` when the pasted prompt has rendered on screen.
#[must_use]
pub fn paste_is_consumed(screen: &str, needles: &PasteNeedles) -> bool {
    let norm_suffix = normalize_for_match(&needles.suffix);
    let norm_prefix = normalize_for_match(&needles.prefix);
    // Empty-needle short-circuit: treat as already consumed
    if norm_suffix.is_empty() && norm_prefix.is_empty() {
        return true;
    }
    let norm_screen = normalize_for_match(screen);
    if !norm_suffix.is_empty() && norm_screen.contains(&norm_suffix) {
        return true;
    }
    if !norm_prefix.is_empty() && norm_screen.contains(&norm_prefix) {
        return true;
    }
    // Check collapsed paste citation patterns against both raw and normalized screen
    COLLAPSED_PASTE_PATTERNS
        .iter()
        .any(|re| re.is_match(screen) || re.is_match(&norm_screen))
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests may panic on unexpected fixtures")]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_ansi_and_whitespace() {
        let s = "\x1b[32mhello\x1b[0m world";
        assert_eq!(normalize_for_match(s), "helloworld");
    }

    #[test]
    fn empty_prompt_gives_empty_needles() {
        let n = build_needles("");
        assert!(n.prefix.is_empty());
        assert!(n.suffix.is_empty());
    }

    #[test]
    fn whitespace_only_prompt_gives_empty_needles() {
        let n = build_needles("   \n  ");
        assert!(n.prefix.is_empty());
        assert!(n.suffix.is_empty());
    }

    #[test]
    fn ascii_prompt_builds_needles() {
        let n = build_needles("What is Python?");
        assert!(!n.suffix.is_empty());
        assert!(!n.prefix.is_empty());
    }

    #[test]
    fn paste_is_consumed_empty_needles() {
        let n = PasteNeedles {
            prefix: String::new(),
            suffix: String::new(),
        };
        assert!(paste_is_consumed("anything", &n));
    }

    #[test]
    fn paste_is_consumed_suffix_match() {
        let n = PasteNeedles {
            prefix: String::new(),
            suffix: "Python".to_string(),
        };
        assert!(paste_is_consumed("What is Python?", &n));
    }

    #[test]
    fn paste_not_consumed_when_needle_absent() {
        let n = PasteNeedles {
            prefix: "hello".to_string(),
            suffix: "world".to_string(),
        };
        assert!(!paste_is_consumed("completely different text", &n));
    }

    #[test]
    fn detect_session_limit_absent() {
        assert!(detect_session_limit("Normal Claude prompt ❯").is_none());
    }

    #[test]
    fn detect_session_limit_present_with_reset() {
        let screen = "You've hit your session limit. This resets at 6:40pm (Asia/Tokyo)";
        let result = detect_session_limit(screen);
        assert!(result.is_some());
        let reset = result.unwrap();
        assert!(reset.is_some());
        assert!(reset.unwrap().contains("6:40pm"));
    }

    #[test]
    fn detect_session_limit_present_no_reset() {
        let screen = "you've hit your usage limit";
        let result = detect_session_limit(screen);
        assert!(result.is_some());
    }

    #[test]
    fn detect_session_limit_rejects_long_reset_info() {
        let long = "x".repeat(MAX_RESET_INFO_LENGTH + 1);
        let screen = format!("You've hit your session limit resets {long}");
        let result = detect_session_limit(&screen);
        assert!(result.is_some());
        assert!(
            result.unwrap().is_none(),
            "too-long reset info should be None"
        );
    }

    #[test]
    fn cjk_needle_stays_within_cell_limit() {
        // Each CJK char = 2 cells; 32 cells = max 16 CJK chars
        let prompt: String = "あ".repeat(20);
        let n = build_needles(&prompt);
        let suffix_chars: Vec<char> = n.suffix.chars().collect();
        let width: usize = suffix_chars.iter().map(|c| char_cell_width(*c)).sum();
        assert!(
            width <= MAX_NEEDLE_CELLS,
            "suffix width {width} > {MAX_NEEDLE_CELLS}"
        );
    }

    #[test]
    fn collapsed_paste_pattern_detected() {
        let screen = "You typed: [Pasted text #1 +5 lines]";
        let n = PasteNeedles {
            prefix: "not-on-screen".to_string(),
            suffix: "not-on-screen".to_string(),
        };
        assert!(paste_is_consumed(screen, &n));
    }
}
