use crate::ledger::{App, expand_path};
use std::{fs, os::unix::fs::PermissionsExt};

fn numeric_version(value: &str) -> Option<Vec<u64>> {
    let value = value.strip_prefix('v').unwrap_or(value);
    (!value.is_empty())
        .then(|| value.split('.').map(str::parse).collect::<Result<Vec<_>, _>>().ok())
        .flatten()
}

pub fn effective_version(app: &App) -> &str {
    let mut versions: Vec<_> = app
        .version
        .iter()
        .chain(app.installations.iter().filter_map(|item| item.version.as_ref()))
        .map(String::as_str)
        .collect();
    if !app.tags.iter().any(|tag| tag == "multi-install") {
        return app.version.as_deref().unwrap_or("unknown");
    }
    versions.sort_by(|left, right| match (numeric_version(left), numeric_version(right)) {
        (Some(left), Some(right)) => left.cmp(&right),
        _ => std::cmp::Ordering::Equal,
    });
    versions.last().copied().unwrap_or("unknown")
}

pub fn observations(app: &App) -> Vec<String> {
    app.installed_paths
        .iter()
        .map(|raw| {
            let path = expand_path(raw);
            match fs::metadata(&path) {
                Ok(meta) if !meta.is_file() => format!("! not a file: {raw}"),
                Ok(meta) if meta.permissions().mode() & 0o111 == 0 => {
                    format!("! not executable: {raw}")
                }
                Ok(_) => format!("present: {raw}"),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => format!("! missing: {raw}"),
                Err(e) => format!("! unreadable: {raw} ({e})"),
            }
        })
        .collect()
}

pub fn health(app: &App) -> &'static str {
    if app.installed_paths.is_empty() {
        if app.installed == Some(false) {
            "not installed"
        } else {
            "unverified"
        }
    } else {
        let obs = observations(app);
        if obs.iter().any(|s| s.starts_with('!')) {
            "! check paths"
        } else if app.installed == Some(false) {
            "! present, marked absent"
        } else {
            "paths present"
        }
    }
}

/// A cheap map-level observation, not a repair or discovery pass. Recorded
/// paths must all exist. Pathless Flatpaks can only be affirmed by an exact
/// AppTrack receipt; `r`/a new run refreshes that state after external changes.
pub fn exists_on_disk(app: &App) -> bool {
    if app.installed == Some(false) {
        return false;
    }
    if !app.installed_paths.is_empty() {
        return app
            .installed_paths
            .iter()
            .all(|raw| fs::metadata(expand_path(raw)).is_ok());
    }
    app.installed == Some(true)
        && app.provenance.managed_by_apptrack
        && app.provenance.source == "flatpak"
}

pub fn presence_marker(app: &App) -> &'static str {
    let realized = exists_on_disk(app)
        || crate::nix_strategy::realization_state(app) == Some(true);
    if matches!(
        (crate::nix_strategy::declaration_state(app), crate::nix_strategy::realization_state(app)),
        (Ok(Some(declared)), Some(nix_realized)) if declared != nix_realized
    ) {
        "iP"
    } else if realized {
        "\\/"
    } else {
        "[]"
    }
}

pub fn report(app: &App, raw_record: &str) -> String {
    let observations = observations(app);
    let removed_by_apptrack = app.installed == Some(false)
        && app.provenance.removed_at_unix.is_some();
    let nix_config_removed = app.provenance.source == "nix"
        && app.provenance.config_removed_at_unix.is_some()
        || app.installations.iter().any(|item| {
            item.source == "nix" && item.config_removed_at_unix.is_some()
        });
    let multi_install = if app.tags.iter().any(|tag| tag == "multi-install") {
        let sources = app
            .installations
            .iter()
            .map(|item| {
                let location = item.config_file.as_deref().map(|file| {
                    format!(
                        " @ {file}{}",
                        item.config_line.map(|line| format!(":{line}")).unwrap_or_default()
                    )
                }).unwrap_or_default();
                format!(
                    "{}{} {}{}{}",
                    item.source,
                    item.package.as_deref().map(|package| format!(":{package}")).unwrap_or_default(),
                    item.version.as_deref().unwrap_or("unknown"),
                    if item.preferred { " (selected)" } else { "" },
                    location,
                )
            })
            .collect::<Vec<_>>()
            .join(" · ");
        format!("\nmulti-install: allowed · {sources}")
    } else {
        String::new()
    };
    let nix_realized = crate::nix_strategy::realization_state(app);
    let nix_declared = crate::nix_strategy::declaration_state(app);
    let nix_state = match (&nix_declared, nix_realized) {
        (Ok(Some(declared)), Some(realized)) => format!(
            "\nNix declaration: {} · realization: {}",
            if *declared { "present" } else { "absent" },
            if realized { "present" } else { "absent" },
        ),
        (Err(error), _) if app.provenance.source == "nix" || app.tags.iter().any(|tag| tag == "multi-install") => format!("\nNix declaration: unverified ({error})"),
        _ => String::new(),
    };
    format!(
        "app: {}\nstatus: {}\ndisposition: {}\nledger installed: {}\neffective version: {}{}\noutcome: {}\nreview: {}\narchived because: {}\n\nfilesystem observations (existence and mode only):\n{}\n\nsource: {}\nrepository: {}\npackage: {}\ninstaller: {}\nconfig: {}{}{}\nAppTrack removal authority: {}\n\nsuggested action: {}\n\ncomplete ledger record (including evidence/history):\n{}",
        app.name,
        health(app),
        app.disposition.label(),
        app.installed
            .map(|v| v.to_string())
            .unwrap_or_else(|| "unknown".into()),
        effective_version(app),
        multi_install,
        app.outcome,
        if app.review.is_empty() {
            "not recorded"
        } else {
            &app.review
        },
        if !app.archived_because.is_empty() {
            &app.archived_because
        } else if app.disposition == crate::ledger::Disposition::Archived && !app.review.is_empty() {
            &app.review
        } else {
            "not recorded"
        },
        if observations.is_empty() && removed_by_apptrack {
            "no installed paths recorded; AppTrack removal receipt says absent".into()
        } else if observations.is_empty() && app.installed == Some(false) {
            "no installed paths recorded; ledger says not installed".into()
        } else if observations.is_empty() {
            "no installed paths recorded; installation not verified".into()
        } else {
            observations.join("\n")
        },
        app.provenance.source,
        app.provenance.repo.as_deref().unwrap_or("unknown"),
        app.provenance.package.as_deref().unwrap_or("unknown"),
        app.provenance.installer.as_deref().unwrap_or("unknown"),
        app.provenance
            .config_file
            .as_deref()
            .unwrap_or("not recorded"),
        app.provenance
            .config_line
            .map(|n| format!(":{n}"))
            .unwrap_or_default(),
        nix_state,
        if removed_by_apptrack {
            "none (AppTrack-managed installation was removed)"
        } else if nix_config_removed {
            "none (exact Nix declaration was removed; realization remains human-controlled)"
        } else if matches!(nix_declared, Ok(Some(true))) {
            "exact Home Manager declaration is separately editable after confirmation"
        } else if app.provenance.managed_by_apptrack {
            "claimed in ledger; installer receipt must be verified before removal"
        } else {
            "none (imported/unmanaged)"
        },
        if removed_by_apptrack {
            "none; archived managed installation was removed successfully"
        } else if nix_config_removed && nix_realized == Some(true) {
            "run your normal rebuild when ready; AppTrack will only observe whether realization converges"
        } else if nix_config_removed {
            "none; Nix declaration and recorded realization are absent"
        } else if app.tags.iter().any(|tag| tag == "multi-install") {
            "none; recovery-floor and current-overlay installations are expected"
        } else if health(app).starts_with('!') {
            "inspect the recorded paths and provenance; reinstall through the recorded source if needed"
        } else {
            "review unknown evidence; no automatic changes"
        },
        raw_record
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::Ledger;

    #[test]
    fn disk_presence_requires_observed_paths_or_a_managed_flatpak_receipt() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let present = dir.path().join("present");
        fs::write(&present, "ok")?;
        let missing = dir.path().join("missing");
        let ledger_path = dir.path().join("ledger.toml");
        fs::write(
            &ledger_path,
            format!(
                r#"schema_version = 1
[[apps]]
identity = "name:present"
name = "present"
category = "Tools"
disposition = "using"
installed_paths = [{present:?}]
[[apps]]
identity = "name:missing"
name = "missing"
category = "Tools"
disposition = "using"
installed_paths = [{missing:?}]
[[apps]]
identity = "flatpak:managed"
name = "managed"
category = "Tools"
disposition = "using"
installed = true
[apps.provenance]
source = "flatpak"
managed_by_apptrack = true
"#,
                present = present.to_string_lossy(),
                missing = missing.to_string_lossy(),
            ),
        )?;
        let ledger = Ledger::open(&ledger_path)?;
        assert!(exists_on_disk(&ledger.apps[0]));
        assert!(!exists_on_disk(&ledger.apps[1]));
        assert!(exists_on_disk(&ledger.apps[2]));
        Ok(())
    }

    #[test]
    fn completed_removal_is_not_reported_as_imported_or_unverified() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let ledger_path = dir.path().join("ledger.toml");
        fs::write(
            &ledger_path,
            r#"schema_version = 1
[[apps]]
identity = "cargo:tool"
name = "tool"
category = "Tools"
disposition = "archived"
installed = false
installed_paths = []
archived_because = "done"
[apps.provenance]
source = "cargo"
installer = "cargo-install"
package = "tool"
managed_by_apptrack = false
removed_at_unix = 123
"#,
        )?;
        let ledger = Ledger::open(&ledger_path)?;
        let report = report(&ledger.apps[0], &ledger.record(0));
        assert!(report.contains("AppTrack removal receipt says absent"));
        assert!(report.contains("none (AppTrack-managed installation was removed)"));
        assert!(report.contains("removed successfully"));
        assert!(!report.contains("imported/unmanaged"));
        Ok(())
    }

    #[test]
    fn multi_install_uses_highest_numeric_version_without_flagging_older_sources() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let ledger_path = dir.path().join("ledger.toml");
        fs::write(
            &ledger_path,
            r#"schema_version = 1
[[apps]]
identity = "name:agent"
name = "agent"
category = "Tools"
tags = ["multi-install"]
disposition = "using"
version = "2.1.266"
[apps.provenance]
source = "nix"
[[apps.installations]]
source = "nix"
version = "2.1.266"
[[apps.installations]]
source = "bun"
version = "2.1.273"
preferred = true
"#,
        )?;
        let ledger = Ledger::open(&ledger_path)?;
        assert_eq!(effective_version(&ledger.apps[0]), "2.1.273");
        let report = report(&ledger.apps[0], &ledger.record(0));
        assert!(report.contains("multi-install: allowed"));
        assert!(report.contains("effective version: 2.1.273"));
        Ok(())
    }
}
