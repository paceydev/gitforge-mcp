//! Fresh connection diagnostics and explicitly confirmed keyring setup.
use super::ui::confirm;
use crate::cache::Cache;
use crate::config::Config;
use crate::context::{self, Options, Report, Status};
use crate::{auth, secret::Secret};
use anyhow::{Context, Result, anyhow, ensure};
use std::time::Duration;

const CONNECTION_TIMEOUT: Duration = Duration::from_secs(30);

fn validate_config(options: &Options) -> Result<()> {
    options
        .config_path
        .as_ref()
        .map(|path| {
            Config::load(path).map_err(|_| {
                anyhow!(
                    "Cannot load gitforge configuration at {}; fix it before running init",
                    path.display()
                )
            })
        })
        .transpose()?;
    Ok(())
}

async fn probe(options: &Options) -> Result<Report> {
    validate_config(options)?;
    let (report, _) = tokio::time::timeout(
        CONNECTION_TIMEOUT,
        context::probe(options, &Cache::disabled()),
    )
    .await
    .context("Connection check timed out after 30 seconds; check your network and retry")?;
    Ok(report)
}

fn show(report: &Report) {
    if let Some(binding) = &report.binding {
        eprintln!(
            "Repository: {} on {}",
            binding.repo.full_name(),
            binding.repo.host
        );
    }
    if let Some(forge) = &report.forge {
        eprintln!(
            "Forge: {}{}",
            forge.kind,
            forge
                .version_raw
                .as_ref()
                .map(|version| format!(" {version}"))
                .unwrap_or_default()
        );
    }
    if let Some(credential) = &report.credential {
        eprintln!(
            "Credentials: {}",
            credential.source.as_deref().unwrap_or("none found")
        );
    }
    if let Some(user) = &report.user {
        eprintln!("Signed in as: {}", user.login);
    }
    match report.status {
        Status::Live => {
            eprintln!("Repository access verified with a fresh request.");
            if report
                .credential
                .as_ref()
                .is_some_and(|c| c.source.is_none())
            {
                eprintln!("Access is anonymous; authenticated operations need a credential.");
            } else if report.user.is_none() {
                eprintln!("Repository access works, but the signed-in user could not be verified.");
            }
        }
        Status::Unbound => eprintln!(
            "No usable checkout/remote found. Run init inside a checkout, or pass --dir and --remote."
        ),
        _ => eprintln!(
            "Repository access could not be verified. Check network, permissions and host configuration; run gitforge-mcp context for details."
        ),
    }
}

fn missing_credential_host(report: &Report) -> Option<String> {
    report
        .binding
        .as_ref()
        .filter(|_| {
            report
                .credential
                .as_ref()
                .is_some_and(|credential| credential.source.is_none())
        })
        .map(|binding| binding.repo.host.clone())
}

fn offer_token(host: &str, dry_run: bool) -> Result<bool> {
    eprintln!(
        "Use an existing GitHub CLI/HTTPS Git login, or gitforge-mcp auth set {host}. SSH push access alone is not API authentication."
    );
    if dry_run {
        return Ok(false);
    }
    confirm(&format!("Store a token for {host} in the OS keyring now?"))
}

async fn store_token(host: String) -> Result<()> {
    let token = Secret::new(rpassword::prompt_password("Token (hidden): ")?.trim());
    ensure!(!token.is_empty(), "No token given; nothing stored");
    tokio::task::spawn_blocking(move || auth::keyring_set(&host, &token)).await??;
    eprintln!("Token stored in the OS keyring. Retesting the connection...");
    Ok(())
}

pub async fn check(options: &Options, dry_run: bool) -> Result<Report> {
    let mut report = probe(options).await?;
    show(&report);
    if let Some(host) = missing_credential_host(&report)
        && offer_token(&host, dry_run)?
    {
        store_token(host).await?;
        report = probe(options).await?;
        show(&report);
    }
    Ok(report)
}
