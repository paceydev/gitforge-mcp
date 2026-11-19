//! `gitforge-mcp config` as text for a person: a row per setting, each with
//! where it came from.

use super::{FileState, Host, Report};

const NOWHERE: &str = "nowhere: no cache directory";

/// The report as text.
pub fn render(report: &Report) -> String {
    let hosts: String = report
        .hosts
        .iter()
        .map(|h| host_block(h, "Forge", "asked when the server starts"))
        .collect();
    let ci: String = report
        .ci
        .iter()
        .map(|h| host_block(h, "CI", "found from commit statuses"))
        .collect();
    [
        "Settings\n".to_owned(),
        table(&settings_rows(report)),
        repository_block(report),
        hosts,
        ci,
        footer(&report.problems),
    ]
    .concat()
}

/// Rows of label, value and where it came from, aligned.
fn table(rows: &[(&str, String, String)]) -> String {
    rows.iter()
        .map(|(label, value, from)| format!("  {label:<12} {value}  ({from})\n"))
        .collect()
}

fn settings_rows(report: &Report) -> Vec<(&'static str, String, String)> {
    let file = &report.config_file;
    let state = match file.state {
        FileState::Loaded => "",
        FileState::Missing => ", not there: nothing configured",
        FileState::Invalid => ", not valid: ignored",
    };
    let path =
        |p: Option<&std::path::Path>| p.map_or(NOWHERE.to_owned(), |p| p.display().to_string());
    vec![
        (
            "config",
            file.path
                .as_ref()
                .map_or("none".to_owned(), |p| p.display().to_string()),
            format!("{}{state}", file.from),
        ),
        (
            "dir",
            report.dir.value.display().to_string(),
            report.dir.from.clone(),
        ),
        (
            "remote",
            report
                .remote
                .value
                .clone()
                .unwrap_or_else(|| "origin, or the only remote".to_owned()),
            report.remote.from.clone(),
        ),
        (
            "read-only",
            if report.read_only.value { "on" } else { "off" }.to_owned(),
            report.read_only.from.clone(),
        ),
        ("log", report.log.value.clone(), report.log.from.clone()),
        (
            "cache",
            path(report.cache.value.as_deref()),
            report.cache.from.clone(),
        ),
        (
            "history",
            path(report.history.value.as_deref()),
            report.history.from.clone(),
        ),
    ]
}

fn repository_block(report: &Report) -> String {
    report.repository.as_ref().map_or(String::new(), |repo| {
        format!(
            "\nRepository\n  {} on {}, through remote {} ({})\n  checkout     {}\n",
            repo.full_name,
            repo.host,
            repo.remote,
            repo.remote_url,
            repo.dir.display()
        )
    })
}

fn host_block(host: &Host, what: &str, unset_kind: &str) -> String {
    format!(
        "{}{}",
        host_title(host, what),
        table(&host_rows(host, unset_kind))
    )
}

fn host_title(host: &Host, what: &str) -> String {
    let whose = if host.bound {
        " (the repository's)"
    } else {
        ""
    };
    format!("\n{what} {}{whose}\n", host.host)
}

fn host_rows(host: &Host, unset_kind: &str) -> Vec<(&'static str, String, String)> {
    let vars: Vec<String> = host
        .token_vars
        .value
        .iter()
        .map(|v| format!("{} ({})", v.name, if v.set { "set" } else { "not set" }))
        .collect();
    let mut rows = vec![
        (
            "kind",
            host.kind
                .value
                .clone()
                .unwrap_or_else(|| unset_kind.to_owned()),
            host.kind.from.clone(),
        ),
        (
            "web address",
            host.base_url
                .value
                .clone()
                .unwrap_or_else(|| "https://<host>/, or what its links show".to_owned()),
            host.base_url.from.clone(),
        ),
        ("token from", vars.join(", "), host.token_vars.from.clone()),
    ];
    rows.extend(
        host.username
            .iter()
            .map(|user| ("username", user.clone(), "config file".to_owned())),
    );
    rows.push((
        "credentials",
        host.credentials.value.join(", "),
        host.credentials.from.clone(),
    ));
    rows
}

fn footer(problems: &[String]) -> String {
    if problems.is_empty() {
        return "\nRun `gitforge-mcp auth status` to see which credential answers, and \
                `gitforge-mcp context` to see what the forge says.\n"
            .to_owned();
    }
    let lines: String = problems.iter().map(|p| format!("  - {p}\n")).collect();
    format!("\nProblems\n{lines}")
}
