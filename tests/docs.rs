//! The documentation site (docs/) against the code, so the two can't drift
//! apart: every command and environment variable is documented (every tool,
//! parameter and action is checked in src/server.rs's tests), every page is in the table of contents, every link and
//! anchor resolves, the config examples are valid config, and every page
//! opens with the sentence llms.txt uses to describe it.
//!
//! Needs no mdbook: it reads the markdown. scripts/docs/build.sh builds the
//! site itself.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use gitforge_mcp::config::Config;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn src() -> PathBuf {
    root().join("docs").join("src")
}

/// A page's markdown, with `{{#include path}}` expanded as mdBook does.
fn page(path: &Path) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let mut out = String::new();
    for line in text.lines() {
        match line
            .trim()
            .strip_prefix("{{#include ")
            .and_then(|l| l.strip_suffix("}}"))
        {
            Some(included) => {
                out.push_str(&page(&path.parent().unwrap().join(included.trim())));
            }
            None => {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    out
}

/// The pages SUMMARY.md links to, in order, relative to docs/src.
fn summary() -> Vec<String> {
    let text = std::fs::read_to_string(src().join("SUMMARY.md")).unwrap();
    text.lines()
        .filter_map(|line| {
            let start = line.find("](")? + 2;
            let end = start + line[start..].find(')')?;
            Some(line[start..end].to_owned())
        })
        .collect()
}

fn all_pages() -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "md") {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(&src(), &mut out);
    out.sort();
    out
}

fn relative(path: &Path) -> String {
    path.strip_prefix(src())
        .unwrap()
        .to_string_lossy()
        .replace('\\', "/")
}

/// Lines outside fenced code blocks.
fn prose(text: &str) -> Vec<&str> {
    let mut fenced = false;
    text.lines()
        .filter(|line| {
            if line.trim_start().starts_with("```") {
                fenced = !fenced;
                return false;
            }
            !fenced
        })
        .collect()
}

/// A heading's id, as mdBook makes it: the text without markup, lower
/// case, spaces as hyphens, and anything else but letters, digits, `-` and
/// `_` dropped. A repeated id gets `-1`, `-2` and so on.
fn heading_ids(text: &str) -> BTreeSet<String> {
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    let mut ids = BTreeSet::new();
    for line in prose(text) {
        let Some(heading) = line.strip_prefix('#') else {
            continue;
        };
        let heading = heading.trim_start_matches('#');
        if !heading.starts_with(' ') {
            continue;
        }
        let id: String = strip_links(heading.trim())
            .chars()
            .filter_map(|c| {
                if c.is_alphanumeric() || c == '_' || c == '-' {
                    Some(c.to_ascii_lowercase())
                } else if c.is_whitespace() {
                    Some('-')
                } else {
                    None
                }
            })
            .collect();
        let n = seen.entry(id.clone()).or_insert(0);
        ids.insert(if *n == 0 {
            id.clone()
        } else {
            format!("{id}-{n}")
        });
        *n += 1;
    }
    ids
}

/// `[text](url)` → `text`.
fn strip_links(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after.find("](") {
            Some(close) if after[close + 2..].contains(')') => {
                out.push_str(&after[..close]);
                let tail = &after[close + 2..];
                rest = &tail[tail.find(')').unwrap() + 1..];
            }
            _ => {
                out.push('[');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Every `](target)` in the page's prose.
fn links(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in prose(text) {
        let mut rest = line;
        while let Some(at) = rest.find("](") {
            let tail = &rest[at + 2..];
            let Some(end) = tail.find(')') else { break };
            out.push(tail[..end].to_owned());
            rest = &tail[end..];
        }
    }
    out
}

#[test]
fn every_page_is_in_the_table_of_contents_and_the_contents_exist() {
    let listed: BTreeSet<String> = summary().into_iter().collect();
    for path in &listed {
        assert!(
            src().join(path).is_file(),
            "SUMMARY.md lists {path}, which isn't there"
        );
    }
    for page in all_pages() {
        let path = relative(&page);
        if path == "SUMMARY.md" {
            continue;
        }
        assert!(listed.contains(&path), "{path} isn't in SUMMARY.md");
    }
}

#[test]
fn every_page_opens_with_a_title_and_a_sentence_for_llms_txt() {
    for path in summary() {
        let text = page(&src().join(&path));
        let mut lines = text.lines().filter(|l| !l.trim().is_empty());
        let title = lines.next().unwrap_or_default();
        assert!(
            title.starts_with("# "),
            "{path} doesn't open with a # title"
        );
        let first = lines.next().unwrap_or_default();
        let structural = ["#", "|", "<", "```", ">", "- ", "{{"];
        assert!(
            !structural.iter().any(|s| first.starts_with(s)),
            "{path}: after the title comes {first:?}, but llms.txt needs a sentence there"
        );
    }
}

/// What's wrong with `link` on the page at `from`, if anything.
fn broken(from: &Path, link: &str) -> Option<String> {
    // Other sites, and llms.txt and llms-full.txt, which exist only in the
    // built site.
    let external = link.contains("://") || link.starts_with("mailto:");
    let (file, anchor) = link.split_once('#').unwrap_or((link, ""));
    if external || file.ends_with(".txt") {
        return None;
    }
    let target = match file {
        "" => from.to_owned(),
        file => from.parent().unwrap().join(file),
    };
    if !target.is_file() {
        return Some(format!("{}: {link}: no such file", relative(from)));
    }
    let missing = !anchor.is_empty() && !heading_ids(&page(&target)).contains(anchor);
    missing.then(|| format!("{}: {link}: no heading with that id", relative(from)))
}

#[test]
fn every_link_and_anchor_resolves() {
    let problems: Vec<String> = all_pages()
        .iter()
        .flat_map(|path| {
            links(&page(path))
                .into_iter()
                .filter_map(move |link| broken(path, &link))
        })
        .collect();
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn every_command_is_documented() {
    let out = Command::new(env!("CARGO_BIN_EXE_gitforge-mcp"))
        .arg("--help")
        .output()
        .unwrap();
    let help = String::from_utf8_lossy(&out.stdout);
    let commands: Vec<&str> = help
        .lines()
        .skip_while(|l| !l.starts_with("Commands:"))
        .skip(1)
        .take_while(|l| l.starts_with("  "))
        .filter_map(|l| l.split_whitespace().next())
        .filter(|c| *c != "help")
        .collect();
    assert!(commands.contains(&"config"), "{help}");
    let text = page(&src().join("guide").join("command-line.md"));
    for command in commands {
        let documented = text.contains(&format!("`gitforge-mcp {command}`"))
            || text.contains(&format!("`{command}`"));
        assert!(
            documented,
            "`gitforge-mcp {command}` isn't in guide/command-line.md"
        );
    }
}

#[test]
fn every_environment_variable_is_documented() {
    let mut vars = BTreeSet::new();
    let mut stack = vec![root().join("src")];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            for (at, _) in text.match_indices("\"GITFORGE_MCP_") {
                let name: String = text[at + 1..]
                    .chars()
                    .take_while(|c| c.is_ascii_uppercase() || *c == '_')
                    .collect();
                vars.insert(name);
            }
        }
    }
    assert!(vars.contains("GITFORGE_MCP_DIR"), "{vars:?}");
    let text = page(&src().join("guide").join("configuration.md"));
    for var in vars {
        assert!(
            text.contains(&format!("`{var}`")),
            "{var} isn't in guide/configuration.md"
        );
    }
}

/// The contents of every ```toml block in `text`.
fn toml_blocks(text: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut open: Option<String> = None;
    for line in text.lines() {
        match (open.as_mut(), line.trim_start()) {
            (None, "```toml") => open = Some(String::new()),
            (Some(_), "```") => blocks.extend(open.take()),
            (Some(block), _) => {
                block.push_str(line);
                block.push('\n');
            }
            (None, _) => {}
        }
    }
    blocks
}

#[test]
fn config_examples_are_valid_config() {
    let mut checked = 0;
    for path in all_pages() {
        // Client configs (Codex's) are TOML too; only ours have [hosts.
        // or [ci. sections.
        let ours = toml_blocks(&page(&path)).into_iter().filter(|t| {
            t.lines()
                .any(|l| l.starts_with("[hosts.") || l.starts_with("[ci."))
        });
        for toml in ours {
            let parsed = Config::parse(&toml);
            assert!(
                parsed.is_ok(),
                "{}: a config example isn't valid: {:?}\n{toml}",
                relative(&path),
                parsed.err()
            );
            checked += 1;
        }
    }
    assert!(checked >= 2, "found {checked} config examples");
}
