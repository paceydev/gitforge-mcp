//! Terminal prompts, previews and explicit confirmations.
use super::{Client, clients, files};
use crate::context::Status;
use anyhow::{Result, ensure};
use std::io::Write;
use std::process::ExitCode;

fn prompt(question: &str) -> Result<String> {
    eprint!("{question} ");
    std::io::stderr().flush()?;
    let mut input = String::new();
    ensure!(
        std::io::stdin().read_line(&mut input)? > 0,
        "Input ended; no further changes made"
    );
    Ok(input.trim().to_owned())
}

pub(super) fn confirm(question: &str) -> Result<bool> {
    loop {
        match prompt(&format!("{question} [y/N]"))?
            .to_ascii_lowercase()
            .as_str()
        {
            "y" | "yes" => return Ok(true),
            "" | "n" | "no" => return Ok(false),
            _ => eprintln!("Enter yes or no."),
        }
    }
}

fn choose_client() -> Result<Client> {
    eprintln!("\nConfigure user-level MCP access for:");
    eprintln!("  1. Claude Code\n  2. Codex\n  3. Both\n  4. Connection check only");
    loop {
        match prompt("Choose [1-4]:")?.as_str() {
            "1" => return Ok(Client::Claude),
            "2" => return Ok(Client::Codex),
            "3" => return Ok(Client::Both),
            "4" => return Ok(Client::None),
            _ => eprintln!("Enter 1, 2, 3 or 4."),
        }
    }
}

pub fn selected_client(client: Option<Client>, dry_run: bool) -> Result<Client> {
    match client {
        Some(client) => Ok(client),
        None if dry_run => Ok(Client::Both),
        None => choose_client(),
    }
}

fn approve_plan(plan: &clients::Plan, dry_run: bool) -> Result<bool> {
    eprintln!(
        "Proposed gitforge entry (command and arguments replace any previous values):\n{}",
        plan.snippet
    );
    eprintln!(
        "Other servers and settings, including per-server environment and enablement, are kept."
    );
    if dry_run {
        eprintln!("Preview only; no file written.");
        return Ok(false);
    }
    confirm("Write this client configuration?")
}

fn save_plan(client: Client, plan: &clients::Plan) -> Result<()> {
    if let Some(backup) = files::apply(plan)? {
        files::describe_backup(&backup);
    }
    eprintln!("Configuration saved. Restart {} to load it.", client.name());
    Ok(())
}

pub fn configure_client(client: Client, plan: &clients::Plan, dry_run: bool) -> Result<()> {
    eprintln!("\n{}: {}", client.name(), plan.path.display());
    if !plan.changed() {
        eprintln!("Already configured; unchanged.");
        return Ok(());
    }
    if approve_plan(plan, dry_run)? {
        save_plan(client, plan)?;
    } else {
        eprintln!("Skipped; no file written.");
    }
    Ok(())
}

pub fn finish(status: Status, dry_run: bool) -> ExitCode {
    eprintln!(
        "\nOpen a checkout in your client and ask it to call forge_context. Project or managed settings can override user settings; desktop credentials/environment can differ from this terminal."
    );
    if dry_run {
        eprintln!("Dry run complete; client configuration and keyring were not changed.");
    }
    if status == Status::Live {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
