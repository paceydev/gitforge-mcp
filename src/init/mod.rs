//! Interactive, user-scoped client setup and a fresh read-only connection check.
mod clients;
mod connection;
mod executable;
mod files;
mod ui;

use std::io::IsTerminal;
use std::process::ExitCode;

use anyhow::{Result, ensure};
use clap::ValueEnum;

use crate::context::Options;

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Client {
    Claude,
    Codex,
    Both,
    None,
}

impl Client {
    fn selected(self) -> &'static [Self] {
        match self {
            Self::Claude => &[Self::Claude],
            Self::Codex => &[Self::Codex],
            Self::Both => &[Self::Claude, Self::Codex],
            Self::None => &[],
        }
    }

    fn name(self) -> &'static str {
        [
            "Claude Code",
            "Codex",
            "both clients",
            "connection check only",
        ][self as usize]
    }
}

pub async fn run(
    options: Options,
    client: Option<Client>,
    dry_run: bool,
    include_config: bool,
) -> Result<ExitCode> {
    let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    ensure!(
        dry_run || interactive,
        "init needs an interactive terminal to confirm changes; use init --dry-run --client both to preview without writing"
    );
    let exe = executable::installed()?;
    let args = clients::arguments(&options, include_config)?;
    eprintln!("gitforge-mcp setup\nExecutable: {}", exe.display());
    eprintln!("Client entries follow each opened checkout; this repository is not pinned.");
    let report = connection::check(&options, dry_run).await?;
    let client = ui::selected_client(client, dry_run)?;
    let plans = client
        .selected()
        .iter()
        .map(|client| {
            clients::plan(*client, &clients::path(*client)?, &exe, &args)
                .map(|plan| (*client, plan))
        })
        .collect::<Result<Vec<_>>>()?;
    for (client, plan) in plans {
        ui::configure_client(client, &plan, dry_run)?;
    }
    Ok(ui::finish(report.status, dry_run))
}

#[cfg(test)]
mod tests;
