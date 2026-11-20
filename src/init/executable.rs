//! Retain stable executable symlinks while matching the running binary.
use anyhow::{Context, Result, ensure};
use std::path::{Path, PathBuf};

/// Prefer the invoked symlink/PATH entry over current_exe's resolved Cellar
/// version, while ensuring we never register a different installed binary.
pub(super) fn resolve(invoked: &Path, current: &Path, search: &[PathBuf]) -> Result<PathBuf> {
    let current = std::fs::canonicalize(current)?;
    let mut candidates = Vec::new();
    if invoked.is_absolute() || invoked.components().count() > 1 {
        candidates.push(std::path::absolute(invoked)?);
    }
    candidates.extend(search.iter().map(|dir| {
        dir.join(if cfg!(windows) {
            "gitforge-mcp.exe"
        } else {
            "gitforge-mcp"
        })
    }));
    candidates
        .into_iter()
        .filter_map(|path| std::path::absolute(path).ok())
        .find(|path| std::fs::canonicalize(path).ok().as_ref() == Some(&current))
        .map(Ok)
        .unwrap_or(Ok(current))
}

pub fn installed() -> Result<PathBuf> {
    let search: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).collect())
        .unwrap_or_default();
    let invoked = std::env::args_os().next().context("No executable path")?;
    let exe = resolve(Path::new(&invoked), &std::env::current_exe()?, &search)?;
    ensure!(
        !exe.to_string_lossy().contains("${"),
        "The executable path contains client variable-expansion syntax; move it to a plain path first"
    );
    Ok(exe)
}
