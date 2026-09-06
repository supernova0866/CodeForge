//! Commit message templating engine.
//!
//! Supports:
//!   [filename]                          -> relative path from project root
//!   [time=yy/mo/dd 24hh:mi:ss]          -> formatted timestamp
//!   \[ and \]                            -> escaped literal brackets
//!
//! Validation has two severities:
//!   Warning    -> unrecognized token, falls back to literal text, push allowed
//!   HardError  -> near-miss malformed token shape (e.g. "yyy"), push blocked
//!
//! `\x` for any x other than `[` or `]` is NOT a universal escape -- it is
//! just two literal characters, `\` and `x`.

use chrono::{DateTime, Local};

#[derive(Debug, Clone, PartialEq)]
pub enum Segment {
    Literal(String),
    Filename,
    Time(String), // raw format string, e.g. "yy/mo/dd 24hh:mi:ss"
}

#[derive(Debug, Clone, PartialEq)]
pub enum Severity {
    Warning,
    HardError,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ValidationIssue {
    pub severity: Severity,
    pub message: String,
    /// byte offset range in the original input this issue applies to, for
    /// highlighting in the UI.
    pub span: (usize, usize),
}

#[derive(Debug, Clone)]
pub struct ParseResult {
    pub segments: Vec<Segment>,
    pub issues: Vec<ValidationIssue>,
}

impl ParseResult {
    pub fn has_hard_error(&self) -> bool {
        self.issues.iter().any(|i| i.severity == Severity::HardError)
    }
}

/// Known year/date/time token shapes, used to distinguish "unrecognized
/// entirely" (Warning) from "clearly an attempted token but wrong shape"
/// (HardError). E.g. "yyy" is a near-miss of "yyyy"/"yy".
const VALID_TIME_TOKENS: &[&str] = &["yyyy", "yy", "mo", "dd", "mi", "ss", "24hh", "12hh"];

/// Tokens that are "close" to a valid token (same letter family) but wrong
/// length/shape -- these are treated as hard errors rather than silently
/// falling back to literal text, since the user clearly meant a variable.
fn looks_like_malformed_time_token(tok: &str) -> bool {
    let letter_families = ["y", "m", "d", "s", "h"];
    let stripped: String = tok.chars().filter(|c| c.is_alphabetic()).collect();
    if stripped.is_empty() {
        return false;
    }
    let first_char = stripped.chars().next().unwrap();
    let all_same = stripped.chars().all(|c| c == first_char);
    if all_same && letter_families.contains(&first_char.to_string().as_str()) {
        // e.g. "yyy", "mmm", "ddd", "sss", "hhh" -- same-letter runs that
        // aren't in the valid list are near-misses.
        return !VALID_TIME_TOKENS.contains(&tok);
    }
    false
}

pub fn parse_template(input: &str) -> ParseResult {
    let mut segments = Vec::new();
    let mut issues = Vec::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    let mut literal_buf = String::new();
    let mut byte_offset = 0usize;

    fn flush_literal(buf: &mut String, segments: &mut Vec<Segment>) {
        if !buf.is_empty() {
            segments.push(Segment::Literal(std::mem::take(buf)));
        }
    }

    while i < chars.len() {
        let c = chars[i];
        let char_len = c.len_utf8();

        if c == '\\' {
            // Only \[ and \] are real escapes.
            if i + 1 < chars.len() && (chars[i + 1] == '[' || chars[i + 1] == ']') {
                literal_buf.push(chars[i + 1]);
                byte_offset += char_len + chars[i + 1].len_utf8();
                i += 2;
                continue;
            } else if i + 1 >= chars.len() {
                // Trailing lone backslash -- warning, kept as literal.
                issues.push(ValidationIssue {
                    severity: Severity::Warning,
                    message: "Trailing '\\' with nothing to escape; kept as literal.".into(),
                    span: (byte_offset, byte_offset + char_len),
                });
                literal_buf.push('\\');
                byte_offset += char_len;
                i += 1;
                continue;
            } else {
                // \x for any other x -- not a universal escape, both chars literal.
                literal_buf.push('\\');
                literal_buf.push(chars[i + 1]);
                byte_offset += char_len + chars[i + 1].len_utf8();
                i += 2;
                continue;
            }
        }

        if c == '[' {
            // find matching ]
            let start = i;
            let mut j = i + 1;
            let mut inner = String::new();
            let mut found_close = false;
            while j < chars.len() {
                if chars[j] == ']' {
                    found_close = true;
                    break;
                }
                inner.push(chars[j]);
                j += 1;
            }

            if !found_close {
                // Unmatched '[' -- warning, rest is literal from here.
                let start_byte = byte_offset;
                let remaining: String = chars[i..].iter().collect();
                let rem_len = remaining.len();
                issues.push(ValidationIssue {
                    severity: Severity::Warning,
                    message: "Unmatched '[' with no closing ']'; treated as literal text.".into(),
                    span: (start_byte, start_byte + rem_len),
                });
                literal_buf.push_str(&remaining);
                byte_offset += rem_len;
                i = chars.len();
                continue;
            }

            let full_token_len: usize = chars[start..=j].iter().map(|c| c.len_utf8()).sum();
            let token_span = (byte_offset, byte_offset + full_token_len);

            if inner == "filename" {
                flush_literal(&mut literal_buf, &mut segments);
                segments.push(Segment::Filename);
            } else if let Some(fmt) = inner.strip_prefix("time=") {
                // Validate each token inside the time format.
                let mut cursor = 0usize;
                let fchars: Vec<char> = fmt.chars().collect();
                let mut bad_hard = false;
                while cursor < fchars.len() {
                    let rest: String = fchars[cursor..].iter().collect();
                    let mut matched = false;
                    // longest-match against known tokens
                    let mut sorted_tokens = VALID_TIME_TOKENS.to_vec();
                    sorted_tokens.sort_by_key(|t| std::cmp::Reverse(t.len()));
                    for tok in &sorted_tokens {
                        if rest.starts_with(tok) {
                            cursor += tok.chars().count();
                            matched = true;
                            break;
                        }
                    }
                    if !matched {
                        // check if it's a run of letters that looks malformed
                        let letters_run: String = fchars[cursor..]
                            .iter()
                            .take_while(|c| c.is_alphabetic())
                            .collect();
                        if !letters_run.is_empty() {
                            if looks_like_malformed_time_token(&letters_run) {
                                bad_hard = true;
                                issues.push(ValidationIssue {
                                    severity: Severity::HardError,
                                    message: format!(
                                        "'{}' is not a valid time token (did you mean 'yyyy'/'yy', 'mo', 'dd', 'mi', 'ss', '24hh', or '12hh'?).",
                                        letters_run
                                    ),
                                    span: token_span,
                                });
                            }
                            cursor += letters_run.chars().count();
                        } else {
                            // separator char like '/' or ':' or ' ' -- fine, skip
                            cursor += 1;
                        }
                    }
                }
                flush_literal(&mut literal_buf, &mut segments);
                if !bad_hard {
                    segments.push(Segment::Time(fmt.to_string()));
                }
                // if bad_hard, we deliberately omit adding a Time segment;
                // push is blocked anyway due to HardError.
            } else {
                // Unrecognized variable name entirely -- warning, literal fallback.
                issues.push(ValidationIssue {
                    severity: Severity::Warning,
                    message: format!(
                        "'[{}]' is not a recognized variable; treated as literal text.",
                        inner
                    ),
                    span: token_span,
                });
                literal_buf.push('[');
                literal_buf.push_str(&inner);
                literal_buf.push(']');
            }

            byte_offset += full_token_len;
            i = j + 1;
            continue;
        }

        literal_buf.push(c);
        byte_offset += char_len;
        i += 1;
    }

    flush_literal(&mut literal_buf, &mut segments);

    ParseResult { segments, issues }
}

/// Resolve a parsed template into the final commit message string.
/// `filename` should already be the correctly-scoped relative path (root
/// files = bare name, nested files = full relative path from project root),
/// resolved by the caller based on Bulk vs Multi mode rules.
pub fn render(
    parsed: &ParseResult,
    filename: Option<&str>,
    now: DateTime<Local>,
) -> String {
    let mut out = String::new();
    for seg in &parsed.segments {
        match seg {
            Segment::Literal(s) => out.push_str(s),
            Segment::Filename => {
                out.push_str(filename.unwrap_or("multiple files"));
            }
            Segment::Time(fmt) => {
                out.push_str(&render_time(fmt, now));
            }
        }
    }
    out
}

fn render_time(fmt: &str, now: DateTime<Local>) -> String {
    use chrono::Timelike;
    use chrono::Datelike;

    let mut out = String::new();
    let chars: Vec<char> = fmt.chars().collect();
    let mut i = 0;
    let mut sorted_tokens = VALID_TIME_TOKENS.to_vec();
    sorted_tokens.sort_by_key(|t| std::cmp::Reverse(t.len()));
    // 12hh appends its AM/PM marker at the very end of the whole rendered
    // string (not inline at the hour's position), since the format may
    // continue with minutes/seconds after the hour token.
    let mut trailing_meridiem: Option<&'static str> = None;

    while i < chars.len() {
        let rest: String = chars[i..].iter().collect();
        let mut matched_token: Option<&str> = None;
        for tok in &sorted_tokens {
            if rest.starts_with(tok) {
                matched_token = Some(tok);
                break;
            }
        }
        if let Some(tok) = matched_token {
            match tok {
                "yyyy" => out.push_str(&format!("{:04}", now.year())),
                "yy" => out.push_str(&format!("{:02}", now.year() % 100)),
                "mo" => out.push_str(&format!("{:02}", now.month())),
                "dd" => out.push_str(&format!("{:02}", now.day())),
                "mi" => out.push_str(&format!("{:02}", now.minute())),
                "ss" => out.push_str(&format!("{:02}", now.second())),
                "24hh" => out.push_str(&format!("{:02}", now.hour())),
                "12hh" => {
                    let h = now.hour12();
                    out.push_str(&format!("{:02}", h.1));
                    trailing_meridiem = Some(if h.0 { "PM" } else { "AM" });
                }
                _ => unreachable!(),
            }
            i += tok.chars().count();
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    if let Some(meridiem) = trailing_meridiem {
        out.push(' ');
        out.push_str(meridiem);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn fixed_time() -> DateTime<Local> {
        // 2026-08-25 14:32:07 local
        Local.with_ymd_and_hms(2026, 8, 25, 14, 32, 7).unwrap()
    }

    #[test]
    fn renders_filename_root_file() {
        let parsed = parse_template("update [filename]");
        assert!(!parsed.has_hard_error());
        let out = render(&parsed, Some("index.js"), fixed_time());
        assert_eq!(out, "update index.js");
    }

    #[test]
    fn renders_filename_nested() {
        let parsed = parse_template("update [filename]");
        let out = render(&parsed, Some("src/utils/helpers.ts"), fixed_time());
        assert_eq!(out, "update src/utils/helpers.ts");
    }

    #[test]
    fn escaped_brackets_render_literal() {
        let parsed = parse_template(r"\[Hi\]");
        assert!(!parsed.has_hard_error());
        assert!(parsed.issues.is_empty());
        let out = render(&parsed, None, fixed_time());
        assert_eq!(out, "[Hi]");
    }

    #[test]
    fn non_bracket_escape_is_literal_backslash_and_char() {
        let parsed = parse_template(r"\h test");
        let out = render(&parsed, None, fixed_time());
        assert_eq!(out, r"\h test");
        assert!(parsed.issues.is_empty());
    }

    #[test]
    fn timestamp_24h_format() {
        let parsed = parse_template("[time=yy/mo/dd 24hh:mi:ss]");
        assert!(!parsed.has_hard_error());
        let out = render(&parsed, None, fixed_time());
        assert_eq!(out, "26/08/25 14:32:07");
    }

    #[test]
    fn timestamp_12h_format_with_am_pm_space() {
        let parsed = parse_template("[time=12hh:mi:ss]");
        assert!(!parsed.has_hard_error());
        let out = render(&parsed, None, fixed_time());
        assert_eq!(out, "02:32:07 PM");
    }

    #[test]
    fn timestamp_full_year() {
        let parsed = parse_template("[time=yyyy-mo-dd]");
        let out = render(&parsed, None, fixed_time());
        assert_eq!(out, "2026-08-25");
    }

    #[test]
    fn unrecognized_variable_is_warning_and_literal() {
        let parsed = parse_template("[bogus]");
        assert!(!parsed.has_hard_error());
        assert_eq!(parsed.issues.len(), 1);
        assert_eq!(parsed.issues[0].severity, Severity::Warning);
        let out = render(&parsed, None, fixed_time());
        assert_eq!(out, "[bogus]");
    }

    #[test]
    fn malformed_time_token_triple_y_is_hard_error() {
        let parsed = parse_template("[time=yyy/mo/dd]");
        assert!(parsed.has_hard_error());
    }

    #[test]
    fn malformed_time_token_triple_m_is_hard_error() {
        let parsed = parse_template("[time=mmm]");
        assert!(parsed.has_hard_error());
    }

    #[test]
    fn unmatched_open_bracket_is_warning() {
        let parsed = parse_template("update [filename");
        assert!(!parsed.has_hard_error());
        assert_eq!(parsed.issues.len(), 1);
        assert_eq!(parsed.issues[0].severity, Severity::Warning);
    }

    #[test]
    fn trailing_backslash_is_warning() {
        let parsed = parse_template(r"update file\");
        assert!(!parsed.has_hard_error());
        assert_eq!(parsed.issues[0].severity, Severity::Warning);
    }

    #[test]
    fn mixed_literal_and_variables() {
        let parsed = parse_template("Auto: [filename] @ [time=24hh:mi:ss]");
        assert!(!parsed.has_hard_error());
        let out = render(&parsed, Some("app.rs"), fixed_time());
        assert_eq!(out, "Auto: app.rs @ 14:32:07");
    }
}
