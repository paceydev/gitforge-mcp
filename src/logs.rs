//! Cutting a CI log down to what an agent needs: the end, or the lines
//! matching a pattern, without the timestamp on every line.

use regex::Regex;
use serde::Serialize;

/// Never return more than this many characters of log per job, however the
/// caller asks: a whole log can be megabytes.
pub const MAX_CHARS: usize = 60_000;

#[derive(Debug, Clone)]
pub struct Cut {
    /// Lines from the end; `None` keeps them all (still capped).
    pub tail: Option<usize>,
    /// Keep only lines matching this, applied before `tail`.
    pub grep: Option<Regex>,
    /// Keep the runner's timestamp at the start of each line.
    pub timestamps: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Excerpt {
    pub text: String,
    /// Lines in what was fetched (the whole log, or the end of it).
    pub lines_fetched: usize,
    pub lines_returned: usize,
    /// True when `text` is not everything that matched.
    pub truncated: bool,
    /// True when only the end of the log was fetched.
    pub partial_fetch: bool,
}

/// A runner timestamp at the start of a line: `2026-11-03T09:03:35.3861147Z `.
fn timestamp_prefix() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:\d{2}) ?")
            .expect("static regex")
    })
}

pub fn cut(raw: &str, how: &Cut, partial_fetch: bool) -> Excerpt {
    let mut lines: Vec<&str> = raw.lines().collect();
    // A ranged fetch starts mid-line; drop the fragment.
    if partial_fetch && !lines.is_empty() {
        lines.remove(0);
    }
    let lines_fetched = lines.len();
    let stripped: Vec<String> = lines
        .iter()
        .map(|l| {
            let l = l.trim_end_matches('\r');
            if how.timestamps {
                l.to_owned()
            } else {
                timestamp_prefix().replace(l, "").into_owned()
            }
        })
        .collect();
    let mut kept: Vec<&String> = match &how.grep {
        Some(re) => stripped.iter().filter(|l| re.is_match(l)).collect(),
        None => stripped.iter().collect(),
    };
    let mut truncated = false;
    if let Some(n) = how.tail
        && kept.len() > n
    {
        kept.drain(..kept.len() - n);
        truncated = true;
    }
    // Keep the end when capping: the failure is almost always there.
    let mut total = 0;
    let mut start = kept.len();
    for (i, line) in kept.iter().enumerate().rev() {
        if total + line.len() + 1 > MAX_CHARS {
            truncated = true;
            break;
        }
        total += line.len() + 1;
        start = i;
    }
    let kept = &kept[start..];
    Excerpt {
        text: kept
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        lines_fetched,
        lines_returned: kept.len(),
        truncated,
        partial_fetch,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOG: &str = "2026-11-03T09:00:23.5944483Z first\n\
                       2026-11-03T09:00:24.0000000Z error: boom\n\
                       2026-11-03T09:00:25.0000000Z last\n";

    fn how(tail: Option<usize>, grep: Option<&str>, timestamps: bool) -> Cut {
        Cut {
            tail,
            grep: grep.map(|g| Regex::new(g).unwrap()),
            timestamps,
        }
    }

    #[test]
    fn strips_timestamps_and_tails() {
        let e = cut(LOG, &how(Some(2), None, false), false);
        assert_eq!(e.text, "error: boom\nlast");
        assert!(e.truncated);
        assert_eq!(e.lines_fetched, 3);
    }

    #[test]
    fn keeps_timestamps_on_request() {
        let e = cut(LOG, &how(Some(1), None, true), false);
        assert_eq!(e.text, "2026-11-03T09:00:25.0000000Z last");
    }

    #[test]
    fn greps_before_tailing() {
        let e = cut(LOG, &how(Some(5), Some("(?i)error"), false), false);
        assert_eq!(e.text, "error: boom");
        assert!(!e.truncated);
    }

    #[test]
    fn drops_the_partial_first_line_of_a_ranged_fetch() {
        let e = cut(
            "ment of a line\nwhole line\n",
            &how(None, None, false),
            true,
        );
        assert_eq!(e.text, "whole line");
    }

    #[test]
    fn caps_size_keeping_the_end() {
        let big: String = (0..20_000).map(|i| format!("line {i}\n")).collect();
        let e = cut(&big, &how(None, None, false), false);
        assert!(e.text.len() <= MAX_CHARS);
        assert!(e.text.ends_with("line 19999"));
        assert!(e.truncated);
    }
}
