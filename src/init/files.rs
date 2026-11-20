//! Confirmed writes with a retained private backup and atomic replacement.
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};

use super::clients::Plan;

fn unchanged(plan: &Plan) -> Result<()> {
    if let Ok(metadata) = std::fs::symlink_metadata(&plan.path) {
        ensure!(
            metadata.is_file(),
            "Configuration is no longer a regular file; rerun init: {}",
            plan.path.display()
        );
    }
    let now = match std::fs::read(&plan.path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    ensure!(
        now == plan.before,
        "Configuration changed since the preview; rerun init: {}",
        plan.path.display()
    );
    Ok(())
}

pub fn apply(plan: &Plan) -> Result<Option<PathBuf>> {
    if !plan.changed() {
        return Ok(None);
    }
    unchanged(plan)?;
    let parent = plan
        .path
        .parent()
        .context("Configuration has no parent directory")?;
    std::fs::create_dir_all(parent)?;
    let mut replacement = tempfile::NamedTempFile::new_in(parent)?;
    replacement.write_all(&plan.after)?;
    if let Ok(metadata) = std::fs::metadata(&plan.path) {
        replacement
            .as_file()
            .set_permissions(metadata.permissions())?;
    }
    replacement.as_file().sync_all()?;
    let backup = if let Some(before) = &plan.before {
        let mut file = tempfile::Builder::new()
            .prefix(".gitforge-mcp-backup-")
            .tempfile_in(parent)?;
        file.write_all(before)?;
        file.as_file().sync_all()?;
        Some(file.keep()?.1)
    } else {
        None
    };
    unchanged(plan)?;
    replacement
        .persist(&plan.path)
        .map_err(|error| error.error)?;
    Ok(backup)
}

pub fn describe_backup(backup: &Path) {
    eprintln!("Backup: {}", backup.display());
}
