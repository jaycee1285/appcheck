use crate::{
    bun_strategy,
    dialog::Dialog,
    cargo_strategy,
    flatpak_strategy,
    ledger::{Disposition, Ledger, expand_path},
    nix_strategy,
    update,
};
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};
use toml_edit::{Array, Item, Table, value};

#[derive(Clone, Debug, PartialEq, Eq)]
struct DirectPlan {
    index: usize,
    name: String,
    identity: String,
    raw_path: String,
    path: PathBuf,
    sha256: String,
    installer: String,
}

enum Plan {
    Flatpak(flatpak_strategy::RemovalPlan),
    Cargo(cargo_strategy::RemovalPlan),
    Bun(bun_strategy::RemovalPlan),
    Nix(nix_strategy::RemovalPlan),
    Direct(DirectPlan),
}

impl Plan {
    fn summary(&self) -> String {
        match self {
            Self::Flatpak(plan) => plan.summary(),
            Self::Cargo(plan) => plan.summary(),
            Self::Bun(plan) => plan.summary(),
            Self::Nix(plan) => plan.summary(),
            Self::Direct(plan) => format!(
                "Remove managed {}?\n\n{}\n{}\nSHA-256 {}…\n\nThe archive decision and reason are already saved. Keeping it is the default.",
                match plan.installer.as_str() {
                    "appimage" => "AppImage",
                    "appimage-appdir" => "AppDir tree",
                    _ => "executable",
                },
                plan.name,
                plan.path.display(),
                &plan.sha256[..12],
            ),
        }
    }

    fn remove_label(&self) -> &'static str {
        match self {
            Self::Flatpak(_) => "Remove exact managed Flatpak application",
            Self::Cargo(_) => "Remove exact managed Cargo package",
            Self::Bun(_) => "Remove exact managed Bun package",
            Self::Nix(_) => "Remove exact Nix package declaration",
            Self::Direct(plan) if plan.installer == "appimage" => "Remove exact managed AppImage",
            Self::Direct(plan) if plan.installer == "appimage-appdir" => "Remove exact managed AppDir tree",
            Self::Direct(_) => "Remove exact managed executable",
        }
    }
}

fn sha256(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn direct_plan(ledger: &Ledger, index: usize) -> Result<Option<DirectPlan>> {
    ledger.check_unchanged()?;
    let app = ledger.apps.get(index).context("Application no longer exists")?;
    if !app.provenance.managed_by_apptrack
        || !matches!(
            app.provenance.source.as_str(),
            "github" | "gitlab" | "codeberg"
        )
        || !app
            .provenance
            .installer
            .as_deref()
            .is_some_and(update::supported_installer)
    {
        return Ok(None);
    }
    ensure!(app.installed == Some(true), "Managed executable receipt does not say the application is installed");
    ensure!(app.provenance.installed_paths.len() == 1, "Managed forge provenance must authorize exactly one installed path");
    let raw_path = &app.provenance.installed_paths[0];
    let path = expand_path(raw_path);
    ensure!(path.is_absolute(), "Managed executable path is not absolute");
    ensure!(
        app.installed_paths.iter().any(|recorded| expand_path(recorded) == path),
        "Managed path is not present in the application's recorded installed paths"
    );
    let metadata = fs::symlink_metadata(&path)
        .with_context(|| format!("Managed executable is missing: {}", path.display()))?;
    let installer = app.provenance.installer.clone().context("Managed artifact receipt has no installer")?;
    if installer == "appimage-appdir" {
        ensure!(metadata.file_type().is_dir(), "Refusing to remove a symlink or non-directory AppDir: {}", path.display());
    } else {
        ensure!(metadata.file_type().is_file(), "Refusing to remove a symlink or non-file: {}", path.display());
    }
    let expected = if installer == "appimage-appdir" {
        app.provenance.appdir_sha256.as_deref().context("Managed AppDir receipt has no tree SHA-256")?
    } else {
        app.provenance.sha256.as_deref().context("Managed executable receipt has no installed SHA-256")?
    };
    ensure!(expected.len() == 64 && expected.bytes().all(|byte| byte.is_ascii_hexdigit()), "Managed executable receipt has an invalid SHA-256");
    let observed = if installer == "appimage-appdir" {
        crate::appimage_tree::hash(&path)?
    } else {
        sha256(&path)?
    };
    ensure!(observed == expected, "Managed executable changed since the AppTrack receipt; refusing removal");
    if installer == "appimage" {
        ensure!(app.provenance.appimage_type == Some(2), "Managed AppImage receipt has no Type-2 authority");
        let recipe = app.recipe.as_ref().context("Managed AppImage receipt has no recipe")?;
        let check_plan = update::Plan {
            index,
            name: app.name.clone(),
            identity: app.identity.clone(),
            recipe: recipe.clone(),
            member: None,
            release: app.provenance.release.clone().unwrap_or_default(),
            asset: update::Asset {
                name: app.provenance.asset.clone().unwrap_or_default(),
                api_url: String::new(),
                size: metadata.len(),
                digest: None,
            },
            destination: path.clone(),
            previous_hash: Some(observed),
            old_version: app.version.clone(),
            up_to_date: true,
            renamed_from: None,
        };
        update::verify_appimage(&path, &check_plan)?;
    } else if installer == "appimage-appdir" {
        ensure!(app.provenance.appimage_type == Some(2), "Managed AppDir receipt has no Type-2 authority");
        let recipe = app.recipe.as_ref().context("Managed AppDir receipt has no recipe")?;
        ensure!(recipe.installer == "appimage-appdir", "Managed AppDir recipe changed installer");
        ensure!(app.provenance.disabled_libs == recipe.disable_libs, "Managed AppDir library-family receipt changed");
    }
    Ok(Some(DirectPlan {
        index,
        name: app.name.clone(),
        identity: app.identity.clone(),
        raw_path: raw_path.clone(),
        path,
        sha256: expected.into(),
        installer,
    }))
}

fn plan(ledger: &Ledger, index: usize) -> Result<Option<Plan>> {
    if let Some(plan) = flatpak_strategy::removal_plan(ledger, index)? {
        return Ok(Some(Plan::Flatpak(plan)));
    }
    if let Some(plan) = cargo_strategy::removal_plan(ledger, index)? {
        return Ok(Some(Plan::Cargo(plan)));
    }
    if let Some(plan) = bun_strategy::removal_plan(ledger, index)? {
        return Ok(Some(Plan::Bun(plan)));
    }
    if let Some(plan) = nix_strategy::removal_plan(ledger, index)? {
        return Ok(Some(Plan::Nix(plan)));
    }
    Ok(direct_plan(ledger, index)?.map(Plan::Direct))
}

/// Archive first: the decision and reason survive prompt cancellation or a
/// failed uninstall. Removal is a separate, explicitly confirmed consequence.
pub fn archive(
    ledger: &mut Ledger,
    index: usize,
    reason: &str,
    dialog: &dyn Dialog,
) -> Result<String> {
    ledger.decide(index, Disposition::Archived, Some(reason))?;
    let planned = match plan(ledger, index) {
        Ok(planned) => planned,
        Err(error) => {
            let detail = format!("{error:#}");
            ledger.record_failure(index, "remove", &detail)?;
            return Err(error.context("Archived with reason, but removal authority could not be verified"));
        }
    };
    let Some(plan) = planned else {
        return Ok("Archived with reason. Installation retained; no supported AppTrack-managed receipt grants removal authority.".into());
    };
    dialog.message(plan.summary());
    let choices = ["Keep installed".to_string(), plan.remove_label().to_string()];
    if dialog.choose("Archived · installation", &choices, Some(0))? != Some(1) {
        return Ok("Archived with reason. Managed installation retained.".into());
    }
    let result = match &plan {
        Plan::Flatpak(plan) => flatpak_strategy::remove(
            ledger,
            plan,
            dialog.cancel_flag(),
            |phase| dialog.message(phase.into()),
        ),
        Plan::Cargo(plan) => cargo_strategy::remove(
            ledger,
            plan,
            dialog.cancel_flag(),
            |phase| dialog.message(phase.into()),
        ),
        Plan::Bun(plan) => bun_strategy::remove(
            ledger,
            plan,
            dialog.cancel_flag(),
            |phase| dialog.message(phase.into()),
        ),
        Plan::Nix(plan) => nix_strategy::remove(
            ledger,
            plan,
            dialog.cancel_flag(),
            |phase| dialog.message(phase.into()),
        ),
        Plan::Direct(plan) => remove_direct(
            ledger,
            plan,
            dialog.cancel_flag(),
            |phase| dialog.message(phase.into()),
        ),
    };
    match result {
        Ok(()) => Ok(match plan {
            Plan::Nix(_) => "Archived with reason · Nix declaration removed; rebuild remains yours.".into(),
            _ => "Archived with reason · managed installation removed.".into(),
        }),
        Err(error) => {
            let detail = format!("{error:#}");
            ledger.record_failure(index, "remove", &detail)?;
            Err(error.context("Archived with reason, but removal failed"))
        }
    }
}

fn removal_receipt_document(ledger: &Ledger, plan: &DirectPlan) -> Result<toml_edit::DocumentMut> {
    let mut doc = ledger.document();
    let app = doc["apps"]
        .as_array_of_tables_mut()
        .context("apps must be an array of tables")?
        .get_mut(plan.index)
        .context("Application disappeared while saving removal receipt")?;
    let remaining: Vec<_> = ledger.apps[plan.index]
        .installed_paths
        .iter()
        .filter(|recorded| expand_path(recorded) != plan.path)
        .cloned()
        .collect();
    let another_artifact_exists = remaining
        .iter()
        .any(|recorded| fs::metadata(expand_path(recorded)).is_ok());
    app["installed"] = value(another_artifact_exists);
    let mut paths = Array::new();
    for path in remaining {
        paths.push(path);
    }
    app["installed_paths"] = value(paths);
    let provenance = app["provenance"]
        .as_table_mut()
        .context("Managed provenance disappeared while saving removal receipt")?;
    provenance["managed_by_apptrack"] = value(false);
    provenance["removed_at_unix"] = value(update::timestamp());
    let mut action = Table::new();
    action["action"] = value("remove");
    action["result"] = value("success");
    action["at_unix"] = value(update::timestamp());
    action["path"] = value(&plan.raw_path);
    action["sha256"] = value(&plan.sha256);
    app["last_action"] = Item::Table(action);
    Ok(doc)
}

fn remove_direct(
    ledger: &mut Ledger,
    plan: &DirectPlan,
    cancel: &AtomicBool,
    progress: impl Fn(&str),
) -> Result<()> {
    ledger.check_unchanged()?;
    ensure!(direct_plan(ledger, plan.index)?.as_ref() == Some(plan), "Executable removal authority changed; review the archived record again");
    ensure!(!cancel.load(Ordering::Acquire), "Operation cancelled");
    let _lock = ledger.write_lock()?;
    ensure!(
        ledger.apps.get(plan.index).is_some_and(|app| app.identity == plan.identity),
        "Application changed before removal"
    );
    let observed = if plan.installer == "appimage-appdir" {
        crate::appimage_tree::hash(&plan.path)?
    } else {
        sha256(&plan.path)?
    };
    ensure!(observed == plan.sha256, "Managed artifact changed before removal");
    let parent = plan.path.parent().context("Managed executable has no parent directory")?;
    let staging = tempfile::Builder::new()
        .prefix(".apptrack-remove-")
        .tempdir_in(parent)
        .context("Cannot create recoverable removal staging beside the executable")?;
    let staged = staging.path().join(
        plan.path
            .file_name()
            .context("Managed executable has no file name")?,
    );
    progress("Staging the exact managed executable for removal…");
    fs::rename(&plan.path, &staged)?;
    let saved = (|| -> Result<()> {
        ensure!(!plan.path.exists(), "Managed executable still exists after staging");
        progress("Saving the removal receipt…");
        ledger.save_locked(removal_receipt_document(ledger, plan)?)?;
        Ok(())
    })();
    if let Err(error) = saved {
        if let Err(rollback) = fs::rename(&staged, &plan.path) {
            return Err(error.context(format!("Removal receipt failed and executable rollback also failed: {rollback}")));
        }
        return Err(error.context("Removal receipt failed; executable restored"));
    }
    drop(staging);
    progress("Removed; absence and AppTrack receipt verified.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Choice(usize);
    impl Dialog for Choice {
        fn message(&self, _: String) {}
        fn prompt(&self, _: &str, _: &str) -> Result<Option<String>> { Ok(None) }
        fn choose(&self, _: &str, _: &[String], _: Option<usize>) -> Result<Option<usize>> {
            Ok(Some(self.0))
        }
        fn cancel_flag(&self) -> &AtomicBool {
            static CANCEL: AtomicBool = AtomicBool::new(false);
            &CANCEL
        }
    }

    #[test]
    fn archive_offers_nix_removal_for_an_active_migration_receipt() -> Result<()> {
        let dir = tempfile::Builder::new()
            .prefix("apptrack-smoke-")
            .tempdir_in("/tmp")?;
        let config_dir = dir.path().join("nix-config/home");
        fs::create_dir_all(&config_dir)?;
        let config = config_dir.join("tracked.nix");
        fs::write(
            &config,
            "{ pkgs, ... }:\n# AppTrack-managed Home Manager packages. AppTrack is the only editor of this\n# file; every entry was individually confirmed and collision-checked.\n{\n  home.packages = with pkgs; [\n    way-displays\n  ];\n}\n",
        )?;
        let path = dir.path().join("ledger.toml");
        fs::write(
            &path,
            format!(r#"schema_version = 1
[[apps]]
identity = "https://github.com/alex-courtis/way-displays"
name = "way-displays"
category = "Desktop"
description = "fucks up scaling for no reason"
tags = []
disposition = "using"
outcome = "unknown"
review = "fucks up scaling for no reason"

[apps.provenance]
source = "unknown"
managed_by_apptrack = false

[apps.evidence]
imported_from = "~/syncthing/Master-Apps-List.md"
observed_on = "2026-09-09"

[apps.nix_migration]
package = "way-displays"
version = "1.15.0"
homepage = "https://github.com/alex-courtis/way-displays"
drv_path = "/nix/store/way-displays.drv"
config_file = {config:?}
config_expression = "way-displays"
migrated_at_unix = 1789857483
realized_path = "/etc/profiles/per-user/john/bin/way-displays"

[apps.nix_check]
result = "matched"
checked_at_unix = 1789857636
"#, config = config.to_string_lossy()),
        )?;
        let mut ledger = Ledger::open(&path)?;
        let result = archive(&mut ledger, 0, "still unhappy with scaling", &Choice(1))?;
        assert!(result.contains("removed"), "{result}");
        assert!(!fs::read_to_string(&config)?.contains("way-displays"));
        let reopened = Ledger::open(&path)?;
        assert_eq!(reopened.apps[0].disposition, Disposition::Archived);
        assert!(reopened.record(0).contains("removed_at_unix"), "{}", reopened.record(0));
        Ok(())
    }

    #[test]
    fn archive_survives_without_removal_authority() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("ledger.toml");
        fs::write(&path, r#"schema_version = 1
[[apps]]
identity = "name:tool"
name = "tool"
category = "Tools"
disposition = "considering"
[apps.provenance]
source = "flatpak"
managed_by_apptrack = false
"#)?;
        let mut ledger = Ledger::open(&path)?;
        let result = archive(&mut ledger, 0, "not for me", &Choice(0))?;
        assert!(result.contains("no supported AppTrack-managed"));
        let reopened = Ledger::open(&path)?;
        assert_eq!(reopened.apps[0].disposition, Disposition::Archived);
        assert_eq!(reopened.apps[0].archived_because, "not for me");
        Ok(())
    }

    #[test]
    fn exact_managed_executable_is_staged_then_removed_with_receipt() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let target = dir.path().join("tool");
        fs::write(&target, "managed executable")?;
        let hash = sha256(&target)?;
        let path = dir.path().join("ledger.toml");
        fs::write(
            &path,
            format!(r#"schema_version = 1
[[apps]]
identity = "github:tool"
name = "tool"
category = "Tools"
disposition = "considering"
installed = true
installed_paths = [{target:?}]
[apps.provenance]
source = "github"
installer = "binary-copy"
managed_by_apptrack = true
sha256 = "{hash}"
installed_paths = [{target:?}]
"#,
                target = target.to_string_lossy(),
            ),
        )?;
        let mut ledger = Ledger::open(&path)?;
        let result = archive(&mut ledger, 0, "not useful", &Choice(1))?;
        assert!(result.contains("installation removed"));
        assert!(!target.exists());
        let reopened = Ledger::open(&path)?;
        let app = &reopened.apps[0];
        assert_eq!(app.disposition, Disposition::Archived);
        assert_eq!(app.installed, Some(false));
        assert!(app.installed_paths.is_empty());
        assert!(!app.provenance.managed_by_apptrack);
        let record = reopened.record(0);
        assert!(record.contains("action = \"remove\""));
        assert!(record.contains(&hash));
        Ok(())
    }

    #[test]
    fn exact_managed_type_two_appimage_is_removed_with_receipt() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let target = dir.path().join("tool");
        let mut image = vec![0; 64];
        image[..6].copy_from_slice(b"\x7fELF\x02\x01");
        image[8..12].copy_from_slice(b"AI\x02\0");
        image[16] = 2;
        image[18] = if std::env::consts::ARCH == "x86_64" { 62 } else { 183 };
        fs::write(&target, &image)?;
        let hash = sha256(&target)?;
        let path = dir.path().join("ledger.toml");
        fs::write(
            &path,
            format!(r#"schema_version = 1
[[apps]]
identity = "https://github.com/example/tool"
name = "tool"
category = "Tools"
disposition = "considering"
installed = true
installed_paths = [{target:?}]
[apps.provenance]
source = "github"
installer = "appimage"
release = "v1"
asset = "tool.AppImage"
appimage_type = 2
managed_by_apptrack = true
sha256 = "{hash}"
installed_paths = [{target:?}]
[apps.recipe]
source = "github"
repo = "example/tool"
asset = "tool.AppImage"
installer = "appimage"
destination = {target:?}
os = "linux"
arch = {arch:?}
"#,
                target = target.to_string_lossy(),
                arch = std::env::consts::ARCH,
            ),
        )?;
        let mut ledger = Ledger::open(&path)?;
        let result = archive(&mut ledger, 0, "not useful", &Choice(1))?;
        assert!(result.contains("installation removed"));
        assert!(!target.exists());
        let reopened = Ledger::open(&path)?;
        assert_eq!(reopened.apps[0].installed, Some(false));
        assert!(reopened.record(0).contains("action = \"remove\""));
        Ok(())
    }

    #[test]
    fn exact_managed_appdir_tree_is_removed_with_receipt() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let target = dir.path().join("tool-AppDir");
        fs::create_dir(&target)?;
        fs::write(target.join("AppRun"), "runner")?;
        fs::create_dir_all(target.join("usr/lib"))?;
        fs::write(target.join("usr/lib/library.so"), "library")?;
        let hash = crate::appimage_tree::hash(&target)?;
        let path = dir.path().join("ledger.toml");
        fs::write(
            &path,
            format!(r#"schema_version = 1
[[apps]]
identity = "https://gitlab.com/example/tool"
name = "tool"
category = "Tools"
disposition = "considering"
installed = true
installed_paths = [{target:?}]
[apps.provenance]
source = "gitlab"
installer = "appimage-appdir"
release = "v1"
asset = "tool.AppImage"
appimage_type = 2
appdir_sha256 = "{hash}"
disabled_libs = []
managed_by_apptrack = true
installed_paths = [{target:?}]
[apps.recipe]
source = "gitlab"
repo = "example/tool"
asset = "tool.AppImage"
installer = "appimage-appdir"
destination = {target:?}
os = "linux"
arch = {arch:?}
"#,
                target = target.to_string_lossy(),
                arch = std::env::consts::ARCH,
            ),
        )?;
        let mut ledger = Ledger::open(&path)?;
        let result = archive(&mut ledger, 0, "not useful", &Choice(1))?;
        assert!(result.contains("installation removed"));
        assert!(!target.exists());
        let reopened = Ledger::open(&path)?;
        assert_eq!(reopened.apps[0].installed, Some(false));
        assert!(reopened.record(0).contains("appdir_sha256"));
        Ok(())
    }

    #[test]
    fn changed_executable_is_retained_and_archive_and_failure_are_recorded() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let target = dir.path().join("tool");
        fs::write(&target, "changed executable")?;
        let path = dir.path().join("ledger.toml");
        fs::write(
            &path,
            format!(r#"schema_version = 1
[[apps]]
identity = "github:tool"
name = "tool"
category = "Tools"
disposition = "considering"
installed = true
installed_paths = [{target:?}]
[apps.provenance]
source = "github"
installer = "binary-copy"
managed_by_apptrack = true
sha256 = "{}"
installed_paths = [{target:?}]
"#,
                "0".repeat(64),
                target = target.to_string_lossy(),
            ),
        )?;
        let mut ledger = Ledger::open(&path)?;
        let error = archive(&mut ledger, 0, "changed my mind", &Choice(1)).unwrap_err();
        assert!(format!("{error:#}").contains("changed since the AppTrack receipt"));
        assert!(target.exists());
        let reopened = Ledger::open(&path)?;
        assert_eq!(reopened.apps[0].disposition, Disposition::Archived);
        let record = reopened.record(0);
        assert!(record.contains("action = \"remove\""));
        assert!(record.contains("result = \"failed\""));
        Ok(())
    }
}
