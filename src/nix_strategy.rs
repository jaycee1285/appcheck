use crate::{
    ledger::{Ledger, expand_path},
    update::timestamp,
};
use anyhow::{Context, Result, ensure};
use std::{
    fs,
    io::Write,
    ops::Range,
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};
use toml_edit::{Item, Table, value};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemovalPlan {
    pub index: usize,
    pub name: String,
    identity: String,
    config_text: String,
    edited_text: String,
    config_file_text: String,
    config_file: PathBuf,
    expression: String,
    line: usize,
    installation_index: Option<usize>,
    migration: bool,
}

#[derive(Clone, Debug)]
struct Authority {
    installation_index: Option<usize>,
    installer: Option<String>,
    package: Option<String>,
    config_file: Option<String>,
    config_expression: Option<String>,
    installed_paths: Vec<String>,
    migration: bool,
}

fn authority(app: &crate::ledger::App) -> Option<Authority> {
    if app.provenance.source == "nix" {
        return Some(Authority {
            installation_index: None,
            installer: app.provenance.installer.clone(),
            package: app.provenance.package.clone(),
            config_file: app.provenance.config_file.clone(),
            config_expression: app.provenance.config_expression.clone(),
            installed_paths: app.installed_paths.clone(),
            migration: false,
        });
    }
    if app.tags.iter().any(|tag| tag == "multi-install") {
        if let Some((index, item)) = app
            .installations
            .iter()
            .enumerate()
            .find(|(_, item)| item.source == "nix")
        {
            return Some(Authority {
                installation_index: Some(index),
                installer: item.installer.clone(),
                package: item.package.clone(),
                config_file: item.config_file.clone(),
                config_expression: item.config_expression.clone(),
                installed_paths: item.installed_paths.clone(),
                migration: false,
            });
        }
    }
    let receipt = app.nix_migration.as_ref().filter(|receipt| receipt.active())?;
    Some(Authority {
        installation_index: None,
        installer: Some("home-manager".into()),
        package: Some(receipt.config_expression.clone()),
        config_file: Some(receipt.config_file.clone()),
        config_expression: Some(receipt.config_expression.clone()),
        installed_paths: receipt.realized_path.iter().cloned().collect(),
        migration: true,
    })
}

impl RemovalPlan {
    pub fn summary(&self) -> String {
        format!(
            "Remove exact Nix package declaration?\n\n{}\n{}:{}\n{}\n\nThis edits configuration only. AppTrack will not rebuild NixOS or Home Manager. The archive decision and reason are already saved; keeping the declaration is the default.",
            self.name,
            self.config_file.display(),
            self.line,
            self.expression,
        )
    }
}

fn smoke_config_path(path: &Path) -> bool {
    path.ancestors().any(|ancestor| {
        ancestor.file_name().is_some_and(|name| name == "nix-config")
            && ancestor.parent().and_then(Path::file_name).is_some_and(|name| {
                name.to_string_lossy().starts_with("apptrack-smoke-")
            })
    })
}

pub(crate) fn ensure_config_target(path: &Path) -> Result<()> {
    let live_root = fs::canonicalize(expand_path("~/repos/config/home"));
    ensure!(
        live_root.as_ref().is_ok_and(|root| path.starts_with(root)) || smoke_config_path(path),
        "Nix config target is outside ~/repos/config/home or the isolated smoke fixture"
    );
    Ok(())
}

pub(crate) fn config_path(raw: &str) -> Result<PathBuf> {
    let path = expand_path(raw);
    ensure!(
        path.is_absolute()
            && !path.components().any(|component| matches!(component, Component::ParentDir)),
        "Nix config path must be absolute without '..'"
    );
    let metadata = fs::symlink_metadata(&path)
        .with_context(|| format!("Cannot inspect Nix config file {}", path.display()))?;
    ensure!(metadata.file_type().is_file(), "Nix config target must be one regular file");
    let path = fs::canonicalize(path)?;
    ensure_config_target(&path)?;
    Ok(path)
}

pub(crate) fn expression_range(text: &str, expression: &str) -> Result<Option<(Range<usize>, usize)>> {
    ensure!(!expression.trim().is_empty() && expression.trim() == expression, "Nix config expression must be exact and trimmed");
    ensure!(!expression.contains(['\n', '\r']), "Nix config expression must fit on one line");
    let mut in_packages = false;
    let mut package_blocks = 0;
    let mut matches = Vec::new();
    let mut offset = 0;
    for (line_index, line) in text.split_inclusive('\n').enumerate() {
        let without_newline = line.strip_suffix('\n').unwrap_or(line);
        let trimmed = without_newline.trim();
        if trimmed == "home.packages = [" || trimmed == "home.packages = with pkgs; [" {
            ensure!(!in_packages, "Nested home.packages list is unsupported");
            in_packages = true;
            package_blocks += 1;
        } else if let Some(inner) = trimmed
            .strip_prefix("home.packages = [")
            .and_then(|value| value.strip_suffix("];"))
            .or_else(|| {
                trimmed
                    .strip_prefix("home.packages = with pkgs; [")
                    .and_then(|value| value.strip_suffix("];"))
            })
        {
            package_blocks += 1;
            if inner.trim() == expression {
                let start_in_line = without_newline.find(expression).unwrap();
                matches.push((
                    offset + start_in_line..offset + start_in_line + expression.len(),
                    line_index + 1,
                ));
            }
        } else if in_packages && trimmed == "];" {
            in_packages = false;
        } else if trimmed == expression {
            ensure!(in_packages, "Nix expression exists outside a supported home.packages list");
            matches.push((offset..offset + line.len(), line_index + 1));
        }
        offset += line.len();
    }
    ensure!(!in_packages, "Unclosed home.packages list is unsupported");
    ensure!(package_blocks > 0, "No supported home.packages list found");
    ensure!(matches.len() <= 1, "Nix expression is not unique in the config file");
    if matches.is_empty() {
        ensure!(
            !text.contains(expression),
            "Nix expression changed shape; exact standalone declaration no longer matches"
        );
    }
    Ok(matches.pop())
}

pub fn declaration_state(app: &crate::ledger::App) -> Result<Option<bool>> {
    let Some(authority) = authority(app) else {
        return Ok(None);
    };
    let Some(file) = authority.config_file.as_deref() else {
        return Ok(None);
    };
    let Some(expression) = authority.config_expression.as_deref() else {
        return Ok(None);
    };
    let file = config_path(file)?;
    let text = fs::read_to_string(&file)?;
    Ok(Some(expression_range(&text, expression)?.is_some()))
}

pub fn realization_state(app: &crate::ledger::App) -> Option<bool> {
    let authority = authority(app)?;
    (!authority.installed_paths.is_empty()).then(|| {
        authority
            .installed_paths
            .iter()
            .all(|path| fs::metadata(expand_path(path)).is_ok())
    })
}

pub fn removal_plan(ledger: &Ledger, index: usize) -> Result<Option<RemovalPlan>> {
    ledger.check_unchanged()?;
    let app = ledger.apps.get(index).context("Application no longer exists")?;
    let Some(authority) = authority(app) else {
        return Ok(None);
    };
    if authority.installer.as_deref() != Some("home-manager")
        || authority.config_file.is_none()
        || authority.config_expression.is_none() {
        return Ok(None);
    }
    let config_file_text = authority.config_file.as_deref().unwrap();
    let expression = authority.config_expression.as_deref().unwrap();
    ensure!(authority.package.as_deref() == Some(expression), "Nix package identity and config expression disagree");
    let config_file = config_path(config_file_text)?;
    let config_text = fs::read_to_string(&config_file)?;
    let (range, line) = expression_range(&config_text, expression)?
        .context("Exact Nix package declaration is no longer present")?;
    let mut edited_text = config_text.clone();
    edited_text.replace_range(range, "");
    ensure!(expression_range(&edited_text, expression)?.is_none(), "Nix package declaration remained after the proposed edit");
    Ok(Some(RemovalPlan {
        index,
        name: app.name.clone(),
        identity: app.identity.clone(),
        config_text,
        edited_text,
        config_file_text: config_file_text.into(),
        config_file,
        expression: expression.into(),
        line,
        installation_index: authority.installation_index,
        migration: authority.migration,
    }))
}

pub(crate) fn replace_file(path: &Path, expected: &str, replacement: &str) -> Result<()> {
    ensure!(fs::read_to_string(path)? == expected, "Nix config changed on disk; external changes were preserved");
    write_atomic(path, replacement)
}

pub(crate) fn create_file(path: &Path, content: &str) -> Result<()> {
    ensure!(!path.exists(), "Nix config target appeared on disk; external changes were preserved");
    write_atomic(path, content)
}

fn write_atomic(path: &Path, content: &str) -> Result<()> {
    let mut temp = tempfile::NamedTempFile::new_in(path.parent().context("Nix config has no parent directory")?)?;
    if let Ok(metadata) = fs::metadata(path) {
        temp.as_file().set_permissions(metadata.permissions())?;
    }
    temp.write_all(content.as_bytes())?;
    temp.as_file().sync_all()?;
    temp.persist(path).context("Could not atomically replace Nix config")?;
    Ok(())
}

fn receipt_document(ledger: &Ledger, plan: &RemovalPlan) -> Result<toml_edit::DocumentMut> {
    let mut doc = ledger.document();
    let app = doc["apps"]
        .as_array_of_tables_mut()
        .context("apps must be an array of tables")?
        .get_mut(plan.index)
        .context("Application disappeared while saving Nix config receipt")?;
    if plan.migration {
        let receipt = app["nix_migration"]
            .as_table_mut()
            .context("Nix migration receipt disappeared")?;
        receipt["removed_at_unix"] = value(timestamp());
    } else if let Some(index) = plan.installation_index {
        let installation = app["installations"]
            .as_array_of_tables_mut()
            .context("Multi-install evidence must use [[apps.installations]] tables")?
            .get_mut(index)
            .context("Nix installation evidence disappeared")?;
        installation["config_removed_at_unix"] = value(timestamp());
    } else {
        let provenance = app["provenance"].as_table_mut().context("Nix provenance disappeared")?;
        provenance["config_removed_at_unix"] = value(timestamp());
    }
    let mut action = Table::new();
    action["action"] = value("remove-config");
    action["result"] = value("success");
    action["at_unix"] = value(timestamp());
    action["config_file"] = value(&plan.config_file_text);
    action["config_expression"] = value(&plan.expression);
    app["last_action"] = Item::Table(action);
    Ok(doc)
}

pub fn remove(
    ledger: &mut Ledger,
    plan: &RemovalPlan,
    cancel: &AtomicBool,
    progress: impl Fn(&str),
) -> Result<()> {
    ensure!(removal_plan(ledger, plan.index)?.as_ref() == Some(plan), "Nix config removal authority changed; review the archived record again");
    ensure!(!cancel.load(Ordering::Acquire), "Operation cancelled");
    let _lock = ledger.write_lock()?;
    ensure!(
        ledger.apps.get(plan.index).is_some_and(|app| app.identity == plan.identity),
        "Application changed before Nix config edit"
    );
    progress("Removing the exact declaration from the copied Nix package list…");
    replace_file(&plan.config_file, &plan.config_text, &plan.edited_text)?;
    progress("Saving the Nix configuration-edit receipt…");
    if let Err(error) = ledger.save_locked(receipt_document(ledger, plan)?) {
        if let Err(rollback) = replace_file(&plan.config_file, &plan.edited_text, &plan.config_text) {
            return Err(error.context(format!("Nix receipt failed and config rollback also failed: {rollback:#}")));
        }
        return Err(error.context("Nix receipt failed; original configuration restored"));
    }
    progress("Configuration declaration removed; rebuild remains a human action.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn locator_accepts_only_one_exact_standalone_home_package() -> Result<()> {
        let text = "{ pkgs, ... }:\n{\n  home.packages = [\n    pkgs.one\n    pkgs.unstable.target\n  ];\n}\n";
        let (range, line) = expression_range(text, "pkgs.unstable.target")?.unwrap();
        assert_eq!(line, 5);
        let mut edited = text.to_string();
        edited.replace_range(range, "");
        assert!(!edited.contains("pkgs.unstable.target"));
        assert!(expression_range(&edited, "pkgs.unstable.target")?.is_none());
        assert!(expression_range("let selected = pkgs.unstable.target;\n", "pkgs.unstable.target").is_err());
        Ok(())
    }

    #[test]
    fn locator_covers_current_with_pkgs_and_single_item_forms() -> Result<()> {
        let with_pkgs = "{ pkgs, ... }:\n{\n  home.packages = with pkgs; [\n    codex\n  ];\n}\n";
        assert_eq!(expression_range(with_pkgs, "codex")?.unwrap().1, 4);
        let inline = "{\n  home.packages = [ apps.ferritebar ];\n}\n";
        let (range, line) = expression_range(inline, "apps.ferritebar")?.unwrap();
        assert_eq!(line, 2);
        let mut edited = inline.to_string();
        edited.replace_range(range, "");
        assert_eq!(edited, "{\n  home.packages = [  ];\n}\n");
        assert!(expression_range(&edited, "apps.ferritebar")?.is_none());
        Ok(())
    }

    #[test]
    fn every_reviewed_bootstrap_nix_expression_matches_live_home_config() -> Result<()> {
        let ledger = Ledger::open(&Path::new(env!("CARGO_MANIFEST_DIR")).join("apptrack.toml"))?;
        let reviewed: Vec<_> = ledger
            .apps
            .iter()
            .filter(|app| app.provenance.source == "nix" && app.provenance.config_expression.is_some())
            .collect();
        assert_eq!(reviewed.len(), 11);
        for app in reviewed {
            assert_eq!(declaration_state(app)?, Some(true), "{}", app.name);
        }
        let multi_nix: Vec<_> = ledger
            .apps
            .iter()
            .filter(|app| app.installations.iter().any(|item| item.source == "nix"))
            .collect();
        assert_eq!(multi_nix.len(), 2);
        for app in multi_nix {
            assert_eq!(declaration_state(app)?, Some(true), "{}", app.name);
            assert_eq!(realization_state(app), Some(true), "{}", app.name);
            let index = ledger.find(&app.identity)?;
            assert!(removal_plan(&ledger, index)?.is_some(), "{}", app.name);
        }
        Ok(())
    }

    #[test]
    fn unreviewed_nix_provenance_grants_no_edit_authority() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let ledger_path = dir.path().join("apptrack.toml");
        fs::write(
            &ledger_path,
            r#"schema_version = 1
[[apps]]
identity = "nix:tool"
name = "tool"
category = "Tools"
disposition = "archived"
[apps.provenance]
source = "nix"
installer = "home-manager"
package = "apps.tool"
config_file = "~/repos/config/definitions.nix"
managed_by_apptrack = false
"#,
        )?;
        let ledger = Ledger::open(&ledger_path)?;
        assert!(removal_plan(&ledger, 0)?.is_none());
        Ok(())
    }

    #[test]
    fn active_migration_receipt_grants_ordinary_removal_authority() -> Result<()> {
        let dir = tempfile::Builder::new()
            .prefix("apptrack-smoke-")
            .tempdir_in("/tmp")?;
        let config_dir = dir.path().join("nix-config/home");
        fs::create_dir_all(&config_dir)?;
        let config = config_dir.join("tracked.nix");
        let config_text = "{ pkgs, ... }:\n{\n  home.packages = with pkgs; [\n    way-displays\n  ];\n}\n";
        fs::write(&config, config_text)?;
        let ledger_path = dir.path().join("apptrack.toml");
        fs::write(
            &ledger_path,
            format!(
                r#"schema_version = 1
[[apps]]
identity = "https://github.com/alex-courtis/way-displays"
name = "way-displays"
category = "Desktop"
disposition = "archived"
archived_because = "done"
[apps.provenance]
source = "unknown"
managed_by_apptrack = false
[apps.nix_migration]
package = "way-displays"
version = "1.15.0"
homepage = "https://github.com/alex-courtis/way-displays"
drv_path = "/nix/store/way-displays.drv"
config_file = {config:?}
config_expression = "way-displays"
migrated_at_unix = 1789831809
realized_path = {realized:?}
"#,
                config = config.to_string_lossy(),
                realized = dir.path().join("bin/way-displays").to_string_lossy(),
            ),
        )?;
        let mut ledger = Ledger::open(&ledger_path)?;
        let app = &ledger.apps[0];
        assert_eq!(declaration_state(app)?, Some(true));
        assert_eq!(realization_state(app), Some(false));
        assert_eq!(crate::doctor::presence_marker(app), "iP");
        let plan = removal_plan(&ledger, 0)?.context("migration receipt must grant removal authority")?;
        remove(&mut ledger, &plan, &AtomicBool::new(false), |_| {})?;
        assert_eq!(fs::read_to_string(&config)?, "{ pkgs, ... }:\n{\n  home.packages = with pkgs; [\n  ];\n}\n");
        let text = fs::read_to_string(&ledger.path)?;
        assert!(text.contains("removed_at_unix"), "{text}");
        assert!(!text.contains("config_removed_at_unix"), "{text}");
        assert!(removal_plan(&ledger, 0)?.is_none(), "a removed receipt grants no further authority");
        Ok(())
    }

    #[test]
    fn realized_migration_reads_as_present() -> Result<()> {
        let dir = tempfile::Builder::new()
            .prefix("apptrack-smoke-")
            .tempdir_in("/tmp")?;
        let config_dir = dir.path().join("nix-config/home");
        fs::create_dir_all(&config_dir)?;
        let config = config_dir.join("tracked.nix");
        fs::write(&config, "{ pkgs, ... }:\n{\n  home.packages = with pkgs; [\n    way-displays\n  ];\n}\n")?;
        let realized = dir.path().join("bin/way-displays");
        fs::create_dir_all(realized.parent().unwrap())?;
        fs::write(&realized, "realized")?;
        let ledger_path = dir.path().join("apptrack.toml");
        fs::write(
            &ledger_path,
            format!(
                r#"schema_version = 1
[[apps]]
identity = "https://github.com/alex-courtis/way-displays"
name = "way-displays"
category = "Desktop"
disposition = "using"
[apps.provenance]
source = "unknown"
managed_by_apptrack = false
[apps.nix_migration]
package = "way-displays"
config_file = {config:?}
config_expression = "way-displays"
drv_path = "/nix/store/way-displays.drv"
migrated_at_unix = 1789831809
realized_path = {realized:?}
"#,
                config = config.to_string_lossy(),
                realized = realized.to_string_lossy(),
            ),
        )?;
        let ledger = Ledger::open(&ledger_path)?;
        let app = &ledger.apps[0];
        assert_eq!(declaration_state(app)?, Some(true));
        assert_eq!(realization_state(app), Some(true));
        assert_eq!(crate::doctor::presence_marker(app), "\\/");
        Ok(())
    }

    #[test]
    fn exact_fixture_declaration_is_removed_without_claiming_realization() -> Result<()> {
        let dir = tempfile::Builder::new()
            .prefix("apptrack-smoke-")
            .tempdir_in("/tmp")?;
        let config_dir = dir.path().join("nix-config/home");
        fs::create_dir_all(&config_dir)?;
        let config = config_dir.join("gui-apps.nix");
        fs::write(
            &config,
            "{ pkgs, ... }:\n{\n  home.packages = [\n    pkgs.unstable.project-graph\n  ];\n}\n",
        )?;
        let realized = dir.path().join("profile/bin/project-graph");
        fs::create_dir_all(realized.parent().unwrap())?;
        fs::write(&realized, "realized")?;
        let ledger_path = dir.path().join("apptrack.toml");
        fs::write(
            &ledger_path,
            format!(
                r#"schema_version = 1
[[apps]]
identity = "https://github.com/graphif/project-graph"
name = "project-graph"
category = "PKM"
disposition = "archived"
installed = true
installed_paths = [{realized:?}]
archived_because = "done"
[apps.provenance]
source = "nix"
installer = "home-manager"
package = "pkgs.unstable.project-graph"
config_file = {config:?}
config_expression = "pkgs.unstable.project-graph"
managed_by_apptrack = false
"#,
                realized = realized.to_string_lossy(),
                config = config.to_string_lossy(),
            ),
        )?;
        let mut ledger = Ledger::open(&ledger_path)?;
        let plan = removal_plan(&ledger, 0)?.unwrap();
        remove(&mut ledger, &plan, &AtomicBool::new(false), |_| {})?;
        assert!(!fs::read_to_string(&config)?.contains("pkgs.unstable.project-graph"));
        assert_eq!(ledger.apps[0].installed, Some(true));
        assert!(ledger.apps[0].provenance.config_removed_at_unix.is_some());
        assert_eq!(crate::doctor::presence_marker(&ledger.apps[0]), "iP");
        Ok(())
    }
}
