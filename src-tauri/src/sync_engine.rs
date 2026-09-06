//! Sync comparison engine.
//!
//! Determines, for every file in a project, whether it's in sync between
//! disk and GitHub, and if not, WHY -- real content change vs. cosmetic-only
//! (whitespace and/or comments). This is what powers the sync status view.
//!
//! Pipeline per file:
//!   1. Fast path: compare git blob SHA (computed locally via
//!      `github_api::compute_git_blob_sha`) against the remote tree entry's
//!      sha. Match -> InSync, done, no further work.
//!   2. Slow path (hashes differ): if "ignore cosmetic differences" is off,
//!      -> ContentChanged, stop.
//!      If on, normalize both versions two independent ways (strip comments
//!      only, strip whitespace only) and hash each. Compare against raw and
//!      against each other to bucket into WhitespaceOnly / CommentOnly /
//!      CosmeticMixed / ContentChanged.

use crate::github_api::compute_git_blob_sha;
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffCategory {
    InSync,
    ContentChanged,
    WhitespaceOnly,
    CommentOnly,
    CosmeticMixed,
    LocalOnly,
    RemoteOnly,
}

#[derive(Debug, Clone)]
pub struct FileSyncStatus {
    pub relative_path: String,
    pub category: DiffCategory,
}

#[derive(Debug, Clone, Copy)]
pub struct CosmeticToggle {
    pub ignore_whitespace_and_comments: bool,
}

/// Which comment-stripping ruleset to use, inferred from file extension.
/// This is intentionally a small, explicit set rather than a generic regex
/// -- language-specific string-literal awareness matters (a `//` inside a
/// URL string or a `#` inside a Python f-string must not be treated as the
/// start of a comment).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommentStyle {
    CLike,     // //, /* */   (js, ts, jsx, tsx, rs, c, cpp, java, go, css uses only block)
    CssBlock,  // /* */ only
    Hash,      // #            (python, sh, ruby, yaml)
    HtmlBlock, // <!-- -->
    None,      // unknown extension -- skip comment stripping, whitespace-only still applies
}

fn comment_style_for(path: &str) -> CommentStyle {
    let ext = path.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "js" | "jsx" | "ts" | "tsx" | "rs" | "c" | "h" | "cpp" | "hpp" | "java" | "go"
        | "kt" | "swift" | "cs" => CommentStyle::CLike,
        "css" | "scss" | "less" => CommentStyle::CssBlock,
        "py" | "sh" | "bash" | "rb" | "yaml" | "yml" | "toml" => CommentStyle::Hash,
        "html" | "htm" | "xml" | "svg" => CommentStyle::HtmlBlock,
        _ => CommentStyle::None,
    }
}

/// Strip whitespace-only differences: collapse all runs of whitespace
/// (including newlines) to a single space, and trim. This intentionally
/// does NOT touch comments -- whitespace-only and comment-only are separate
/// buckets, so each normalization pass must be independent of the other.
fn strip_whitespace(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut last_was_space = false;
    for c in content.chars() {
        if c.is_whitespace() {
            if !last_was_space {
                out.push(' ');
                last_was_space = true;
            }
        } else {
            out.push(c);
            last_was_space = false;
        }
    }
    out.trim().to_string()
}

/// Strip comments only, respecting string literals so `//` or `#` inside a
/// string isn't mistaken for a comment start. This does NOT normalize
/// whitespace -- whitespace-only and comment-only are independent passes.
///
/// This is a hand-rolled scanner, not a full tokenizer/AST -- it handles the
/// common cases (single/double/backtick-quoted strings with backslash
/// escapes) but is not a substitute for a real per-language parser. Good
/// enough for a v1 cosmetic-diff heuristic; Tree-sitter would be the
/// long-term upgrade path mentioned in the original design discussion.
fn strip_comments(content: &str, style: CommentStyle) -> String {
    match style {
        CommentStyle::None => content.to_string(),
        CommentStyle::HtmlBlock => strip_block_only(content, "<!--", "-->"),
        CommentStyle::CssBlock => strip_block_only(content, "/*", "*/"),
        CommentStyle::CLike => strip_c_like(content),
        CommentStyle::Hash => strip_hash_line_comments(content),
    }
}

fn strip_block_only(content: &str, open: &str, close: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut rest = content;
    loop {
        match rest.find(open) {
            Some(start) => {
                out.push_str(&rest[..start]);
                // Trim trailing spaces/tabs on the current line before the
                // comment start, so a comment that sits alone on its own
                // line (possibly indented) doesn't leave residual
                // indentation behind as a false "whitespace difference".
                while out.ends_with(' ') || out.ends_with('\t') {
                    out.pop();
                }
                let after_open = &rest[start + open.len()..];
                match after_open.find(close) {
                    Some(end) => {
                        let mut remainder = &after_open[end + close.len()..];
                        // If the comment is immediately followed by a
                        // newline, swallow that newline too -- this merges
                        // what would otherwise be a lone blank line left
                        // behind by removing a comment that occupied its
                        // own full line.
                        if remainder.starts_with('\n') {
                            remainder = &remainder[1..];
                        } else if remainder.starts_with("\r\n") {
                            remainder = &remainder[2..];
                        }
                        rest = remainder;
                    }
                    None => {
                        // Unterminated comment to end of file; drop the rest.
                        rest = "";
                    }
                }
            }
            None => {
                out.push_str(rest);
                break;
            }
        }
    }
    out
}

/// Handles both `//` line comments and `/* */` block comments, respecting
/// single/double/backtick string literals with backslash-escape awareness.
fn strip_c_like(content: &str) -> String {
    let chars: Vec<char> = content.chars().collect();
    let mut out = String::with_capacity(content.len());
    let mut i = 0;
    let mut in_string: Option<char> = None; // active quote char, if any

    while i < chars.len() {
        let c = chars[i];

        if let Some(quote) = in_string {
            out.push(c);
            if c == '\\' && i + 1 < chars.len() {
                // escaped char inside string -- consume both, don't let the
                // escaped char itself close or confuse the string state
                out.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if c == quote {
                in_string = None;
            }
            i += 1;
            continue;
        }

        // Not in a string.
        if c == '"' || c == '\'' || c == '`' {
            in_string = Some(c);
            out.push(c);
            i += 1;
            continue;
        }

        if c == '/' && i + 1 < chars.len() && chars[i + 1] == '/' {
            // Trim trailing spaces/tabs already pushed on this line before
            // the comment marker, so removing the comment doesn't leave a
            // false residual-whitespace difference behind.
            while out.ends_with(' ') || out.ends_with('\t') {
                out.pop();
            }
            // line comment -- skip to end of line, keep the newline itself
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }

        if c == '/' && i + 1 < chars.len() && chars[i + 1] == '*' {
            while out.ends_with(' ') || out.ends_with('\t') {
                out.pop();
            }
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            i = (i + 2).min(chars.len());
            // If the block comment sat alone on its own line, swallow the
            // following newline too so no blank line is left behind.
            if i < chars.len() && chars[i] == '\n' {
                i += 1;
            }
            continue;
        }

        out.push(c);
        i += 1;
    }

    out
}

/// `#` line comments, respecting single/double-quoted strings (covers
/// Python/shell/YAML common cases; doesn't handle Python triple-quoted
/// strings specially, which is a known v1 limitation worth flagging).
fn strip_hash_line_comments(content: &str) -> String {
    let chars: Vec<char> = content.chars().collect();
    let mut out = String::with_capacity(content.len());
    let mut i = 0;
    let mut in_string: Option<char> = None;

    while i < chars.len() {
        let c = chars[i];

        if let Some(quote) = in_string {
            out.push(c);
            if c == '\\' && i + 1 < chars.len() {
                out.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if c == quote {
                in_string = None;
            }
            i += 1;
            continue;
        }

        if c == '"' || c == '\'' {
            in_string = Some(c);
            out.push(c);
            i += 1;
            continue;
        }

        if c == '#' {
            while out.ends_with(' ') || out.ends_with('\t') {
                out.pop();
            }
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }

        out.push(c);
        i += 1;
    }

    out
}

/// Full cosmetic-aware classification -- requires both local and remote
/// raw content (remote content must be fetched by the caller via
/// `GitHubClient::fetch_blob_content`, since this module stays network-free
/// and pure so it can be unit tested without hitting the API).
pub fn classify_file_with_remote_content(
    relative_path: &str,
    local_content: &[u8],
    remote_content: &[u8],
    toggle: CosmeticToggle,
) -> DiffCategory {
    let local_sha = compute_git_blob_sha(local_content);
    let remote_sha = compute_git_blob_sha(remote_content);
    if local_sha == remote_sha {
        return DiffCategory::InSync;
    }

    if !toggle.ignore_whitespace_and_comments {
        return DiffCategory::ContentChanged;
    }

    let local_str = String::from_utf8_lossy(local_content);
    let remote_str = String::from_utf8_lossy(remote_content);
    let style = comment_style_for(relative_path);

    let local_no_ws = strip_whitespace(&local_str);
    let remote_no_ws = strip_whitespace(&remote_str);
    let ws_equal = local_no_ws == remote_no_ws;

    let local_no_comments = strip_comments(&local_str, style);
    let remote_no_comments = strip_comments(&remote_str, style);
    let comments_equal = local_no_comments == remote_no_comments;

    let local_fully_normalized = strip_whitespace(&strip_comments(&local_str, style));
    let remote_fully_normalized = strip_whitespace(&strip_comments(&remote_str, style));
    let fully_equal = local_fully_normalized == remote_fully_normalized;

    if !fully_equal {
        return DiffCategory::ContentChanged;
    }

    // fully_equal is true here, so the only remaining question is which
    // cosmetic dimension(s) actually needed stripping to reach equality.
    // ws_equal true means whitespace-normalization ALONE already bridges
    // the gap -> the difference was purely whitespace. comments_equal true
    // means comment-stripping ALONE already bridges the gap -> purely
    // comments. If neither alone suffices, both were needed -> mixed.
    match (ws_equal, comments_equal) {
        (true, true) => DiffCategory::InSync, // shouldn't normally hit (raw shas already differed) but harmless
        (true, false) => DiffCategory::WhitespaceOnly,
        (false, true) => DiffCategory::CommentOnly,
        (false, false) => DiffCategory::CosmeticMixed,
    }
}

/// Top-level project sync report: walks a merged set of local paths and
/// remote tree paths, classifying each. Callers assemble the two input maps
/// (local: relative_path -> content, remote: relative_path -> blob sha)
/// themselves from `fs_sandbox` and `github_api` respectively.
pub fn compare_project(
    local_files: &HashMap<String, Vec<u8>>,
    remote_files: &HashMap<String, String>, // relative_path -> blob sha
    fetch_remote_content: impl Fn(&str) -> Option<Vec<u8>>,
    toggle: CosmeticToggle,
) -> Vec<FileSyncStatus> {
    let mut results = Vec::new();
    let mut seen_paths: std::collections::HashSet<&str> = std::collections::HashSet::new();

    for (path, local_content) in local_files {
        seen_paths.insert(path.as_str());
        match remote_files.get(path) {
            None => results.push(FileSyncStatus {
                relative_path: path.clone(),
                category: DiffCategory::LocalOnly,
            }),
            Some(remote_sha) => {
                let local_sha = compute_git_blob_sha(local_content);
                let category = if &local_sha == remote_sha {
                    DiffCategory::InSync
                } else if !toggle.ignore_whitespace_and_comments {
                    DiffCategory::ContentChanged
                } else if let Some(remote_content) = fetch_remote_content(path) {
                    classify_file_with_remote_content(path, local_content, &remote_content, toggle)
                } else {
                    // Could not fetch remote content for comparison; fall
                    // back to a plain content-changed classification rather
                    // than silently guessing.
                    DiffCategory::ContentChanged
                };
                results.push(FileSyncStatus {
                    relative_path: path.clone(),
                    category,
                });
            }
        }
    }

    for path in remote_files.keys() {
        if !seen_paths.contains(path.as_str()) {
            results.push(FileSyncStatus {
                relative_path: path.clone(),
                category: DiffCategory::RemoteOnly,
            });
        }
    }

    results
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toggle_on() -> CosmeticToggle {
        CosmeticToggle { ignore_whitespace_and_comments: true }
    }
    fn toggle_off() -> CosmeticToggle {
        CosmeticToggle { ignore_whitespace_and_comments: false }
    }

    #[test]
    fn identical_content_is_in_sync() {
        let a = b"const x = 1;\n";
        let cat = classify_file_with_remote_content("a.js", a, a, toggle_on());
        assert_eq!(cat, DiffCategory::InSync);
    }

    #[test]
    fn toggle_off_never_produces_cosmetic_buckets() {
        let local = b"const x=1;\n// comment\n";
        let remote = b"const x = 1;\n";
        let cat = classify_file_with_remote_content("a.js", local, remote, toggle_off());
        assert_eq!(cat, DiffCategory::ContentChanged);
    }

    #[test]
    fn whitespace_only_difference_js() {
        let local = b"const x = 1;\nconst y = 2;\n";
        let remote = b"const x = 1;\n\n\nconst    y = 2;\n";
        let cat = classify_file_with_remote_content("a.js", local, remote, toggle_on());
        assert_eq!(cat, DiffCategory::WhitespaceOnly);
    }

    #[test]
    fn comment_only_difference_js() {
        let local = b"const x = 1; // set x\n";
        let remote = b"const x = 1;\n";
        let cat = classify_file_with_remote_content("a.js", local, remote, toggle_on());
        assert_eq!(cat, DiffCategory::CommentOnly);
    }

    #[test]
    fn comment_only_difference_python_hash() {
        let local = b"x = 1  # set x\n";
        let remote = b"x = 1\n";
        let cat = classify_file_with_remote_content("a.py", local, remote, toggle_on());
        assert_eq!(cat, DiffCategory::CommentOnly);
    }

    #[test]
    fn cosmetic_mixed_whitespace_and_comments() {
        let local = b"const x = 1;   // note\n\n\nconst y = 2;\n";
        let remote = b"const x = 1;\nconst y = 2;\n";
        let cat = classify_file_with_remote_content("a.js", local, remote, toggle_on());
        assert_eq!(cat, DiffCategory::CosmeticMixed);
    }

    #[test]
    fn real_content_change_is_not_cosmetic() {
        let local = b"function add(a, b) { return a + b; }\n";
        let remote = b"function add(a, b) { return a - b; }\n";
        let cat = classify_file_with_remote_content("a.js", local, remote, toggle_on());
        assert_eq!(cat, DiffCategory::ContentChanged);
    }

    #[test]
    fn comment_stripper_does_not_touch_url_in_string() {
        // The classic false-positive case: `//` inside a string literal
        // must NOT be treated as a comment start.
        let local = b"const url = \"https://example.com\";\n";
        let remote = b"const url = \"https://example.com\"; // the site\n";
        let cat = classify_file_with_remote_content("a.js", local, remote, toggle_on());
        // The only real difference is the trailing comment -> CommentOnly,
        // proving the URL's `//` wasn't misinterpreted as a comment itself
        // (if it had been, stripping would have mangled the string and this
        // would likely misclassify as ContentChanged).
        assert_eq!(cat, DiffCategory::CommentOnly);
    }

    #[test]
    fn hash_stripper_does_not_touch_hash_in_string() {
        let local = b"color = \"#ffffff\"\n";
        let remote = b"color = \"#ffffff\"  # white\n";
        let cat = classify_file_with_remote_content("a.py", local, remote, toggle_on());
        assert_eq!(cat, DiffCategory::CommentOnly);
    }

    #[test]
    fn css_block_comment_only() {
        let local = b"body { color: red; }\n";
        let remote = b"/* header */\nbody { color: red; }\n";
        let cat = classify_file_with_remote_content("a.css", local, remote, toggle_on());
        assert_eq!(cat, DiffCategory::CommentOnly);
    }

    #[test]
    fn compare_project_buckets_local_only_and_remote_only() {
        let mut local = HashMap::new();
        local.insert("proj/only_local.js".to_string(), b"a".to_vec());
        let mut remote = HashMap::new();
        remote.insert("proj/only_remote.js".to_string(), "somesha".to_string());

        let results = compare_project(&local, &remote, |_| None, toggle_on());
        let local_only = results.iter().find(|r| r.relative_path == "proj/only_local.js").unwrap();
        let remote_only = results.iter().find(|r| r.relative_path == "proj/only_remote.js").unwrap();
        assert_eq!(local_only.category, DiffCategory::LocalOnly);
        assert_eq!(remote_only.category, DiffCategory::RemoteOnly);
    }

    #[test]
    fn compare_project_in_sync_when_shas_match() {
        let content = b"hello world\n".to_vec();
        let sha = compute_git_blob_sha(&content);
        let mut local = HashMap::new();
        local.insert("proj/a.txt".to_string(), content);
        let mut remote = HashMap::new();
        remote.insert("proj/a.txt".to_string(), sha);

        let results = compare_project(&local, &remote, |_| None, toggle_on());
        assert_eq!(results[0].category, DiffCategory::InSync);
    }
}
