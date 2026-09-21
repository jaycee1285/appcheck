use crate::{
    ledger::{App, Ledger, expand_path},
    update::{Recipe, timestamp},
};
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
    process::Command,
    sync::atomic::AtomicBool,
};
use toml_edit::{Array, Item, Table, value};

#[derive(Clone, Debug)]
pub struct Plan {
    pub index: usize,
    pub name: String,
    pub identity: String,
    pub package: String,
    pub registry: String,
    pub root_text: String,
    pub root: PathBuf,
    pub bins: Vec<String>,
    pub targets: Vec<PathBuf>,
    pub previous_hashes: Vec<Option<String>>,
    pub old_version: Option<String>,
    pub installed_version: Option<String>,
    pub release: String,
    pub up_to_date: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemovalPlan {
    pub index: usize,
    pub name: String,
    identity: String,
    package: String,
    registry: String,
    root_text: String,
    root: PathBuf,
    bins: Vec<String>,
    targets: Vec<PathBuf>,
    hashes: BTreeMap<String, String>,
    version: String,
}

impl RemovalPlan {
    pub fn summary(&self) -> String {
        format!(
            "Remove managed Bun global package?\n\n{} {}\nregistry {} · package {}\nroot {}\n{}\n\nInstalled package metadata, contained command symlinks, and every hash must still match. The archive decision and reason are already saved; keeping the package is the default.",
            self.name,
            self.version,
            self.registry,
            self.package,
            self.root.display(),
            self.targets.iter().map(|path| path.display().to_string()).collect::<Vec<_>>().join("\n"),
        )
    }
}

#[derive(Debug)]
struct Installed {
    version: String,
    bins: Vec<String>,
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

fn safe_component(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn package_path(package: &str) -> Result<PathBuf> {
    let parts: Vec<_> = package.split('/').collect();
    let valid = match parts.as_slice() {
        [name] => safe_component(name),
        [scope, name] => scope.starts_with('@') && safe_component(&scope[1..]) && safe_component(name),
        _ => false,
    };
    ensure!(valid, "Bun package must be one exact npm package name");
    Ok(parts.iter().collect())
}

fn recipe(app: &App) -> Result<(&Recipe, &str, &str, &str, &[String])> {
    let recipe = app.recipe.as_ref().context("No update recipe recorded for this app")?;
    ensure!(
        recipe.source == "bun" && recipe.installer == "bun-global",
        "Expected a bun / bun-global recipe"
    );
    let package = recipe.package.as_deref().context("Bun recipe requires package")?;
    package_path(package)?;
    let registry = recipe.registry.as_deref().context("Bun recipe requires registry")?;
    ensure!(
        registry == "https://registry.npmjs.org",
        "Only the exact https://registry.npmjs.org Bun registry identity is supported"
    );
    let root = recipe.root.as_deref().context("Bun recipe requires root")?;
    ensure!(!recipe.bins.is_empty(), "Bun recipe requires at least one exact bin");
    let mut unique = HashSet::new();
    for bin in &recipe.bins {
        ensure!(safe_component(bin), "Bun bin must be one exact command name");
        ensure!(unique.insert(bin), "Bun recipe repeats bin {bin}");
    }
    Ok((recipe, package, registry, root, &recipe.bins))
}

fn root_path(raw: &str) -> Result<PathBuf> {
    let root = expand_path(raw);
    ensure!(
        root.is_absolute()
            && !root.components().any(|component| matches!(component, Component::ParentDir)),
        "Bun root must be an absolute path without '..'"
    );
    ensure!(root.is_dir(), "Bun root must already exist: {}", root.display());
    ensure!(!root.starts_with("/nix/store"), "Bun root cannot be in the Nix store");
    Ok(root)
}

pub(crate) fn parse_latest(output: &str) -> Result<String> {
    let value: serde_json::Value =
        serde_json::from_str(output.trim()).context("Invalid bun info JSON")?;
    let version = value
        .as_str()
        .or_else(|| value.get("version").and_then(serde_json::Value::as_str))
        .context("bun info did not return one version")?;
    ensure!(
        version.starts_with(|character: char| character.is_ascii_digit())
            && version.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+')
            }),
        "bun info returned an invalid version"
    );
    Ok(version.into())
}

fn latest(package: &str, registry: &str, cancel: &AtomicBool) -> Result<String> {
    let workdir = bun_workdir()?;
    let output = crate::work::output(
        Command::new("bun").args([
            "info",
            package,
            "version",
            "--json",
            &format!("--registry={registry}"),
        ])
        .current_dir(workdir.path()),
        true,
        Some(cancel),
    )
    .context("Cannot run bun info")?;
    ensure!(
        output.status.success(),
        "bun info failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    parse_latest(&String::from_utf8(output.stdout).context("bun info returned non-UTF-8 output")?)
}

fn bun_workdir() -> Result<tempfile::TempDir> {
    let workdir = tempfile::Builder::new()
        .prefix("apptrack-bun-")
        .tempdir_in("/tmp")?;
    fs::write(
        workdir.path().join("package.json"),
        r#"{"name":"apptrack-bun-operation","version":"0.0.0","private":true}"#,
    )?;
    Ok(workdir)
}

fn bun_command(root: &Path) -> Command {
    let mut command = Command::new("bun");
    command
        .env("BUN_INSTALL", root)
        .env("BUN_INSTALL_BIN", root.join("bin"));
    command
}

fn installed(root: &Path, package: &str) -> Result<Option<Installed>> {
    let package_dir = root
        .join("install/global/node_modules")
        .join(package_path(package)?);
    let manifest = package_dir.join("package.json");
    let contents = match fs::read_to_string(&manifest) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("Cannot read Bun package.json"),
    };
    let value: serde_json::Value =
        serde_json::from_str(&contents).context("Invalid installed Bun package.json")?;
    ensure!(
        value.get("name").and_then(serde_json::Value::as_str) == Some(package),
        "Installed Bun package identity does not match the recipe"
    );
    let version = value
        .get("version")
        .and_then(serde_json::Value::as_str)
        .context("Installed Bun package has no version")?
        .to_string();
    let bins = match value.get("bin") {
        Some(serde_json::Value::Object(entries)) => entries.keys().cloned().collect(),
        Some(serde_json::Value::String(_)) => vec![package.rsplit('/').next().unwrap().into()],
        _ => anyhow::bail!("Installed Bun package has no bin mapping"),
    };
    Ok(Some(Installed { version, bins }))
}

fn current_hash(root: &Path, package: &str, bin: &str) -> Result<Option<String>> {
    let link = root.join("bin").join(bin);
    let metadata = match fs::symlink_metadata(&link) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    ensure!(metadata.file_type().is_symlink(), "Bun command is not a symlink: {}", link.display());
    let resolved = fs::canonicalize(&link).context("Bun command symlink is broken")?;
    let package_dir = fs::canonicalize(
        root.join("install/global/node_modules")
            .join(package_path(package)?),
    )?;
    ensure!(
        resolved.starts_with(&package_dir),
        "Bun command symlink escapes its exact package: {}",
        link.display()
    );
    Ok(Some(sha256(&resolved)?))
}

pub fn removal_plan(ledger: &Ledger, index: usize) -> Result<Option<RemovalPlan>> {
    ledger.check_unchanged()?;
    let app = ledger.apps.get(index).context("Application no longer exists")?;
    if !app.provenance.managed_by_apptrack || app.provenance.source != "bun" {
        return Ok(None);
    }
    ensure!(app.provenance.installer.as_deref() == Some("bun-global"), "Managed Bun receipt has the wrong installer");
    ensure!(app.installed == Some(true), "Managed Bun receipt does not say the package is installed");
    let package = app.provenance.package.as_deref().context("Managed Bun receipt has no package")?;
    let registry = app.provenance.registry.as_deref().context("Managed Bun receipt has no registry")?;
    let root_text = app.provenance.root.as_deref().context("Managed Bun receipt has no root")?;
    let version = app.provenance.release.as_deref().context("Managed Bun receipt has no version")?;
    package_path(package)?;
    ensure!(registry == "https://registry.npmjs.org", "Only the exact npm registry receipt grants Bun removal authority");
    let root = root_path(root_text)?;
    ensure!(!app.provenance.bin_sha256.is_empty(), "Managed Bun receipt has no command hashes");
    let bins: Vec<_> = app.provenance.bin_sha256.keys().cloned().collect();
    for bin in &bins {
        ensure!(safe_component(bin), "Managed Bun receipt has an invalid command name");
    }
    let targets: Vec<_> = bins.iter().map(|bin| root.join("bin").join(bin)).collect();
    let receipt_paths: HashSet<_> = app.provenance.installed_paths.iter().map(|path| expand_path(path)).collect();
    let target_paths: HashSet<_> = targets.iter().cloned().collect();
    ensure!(receipt_paths == target_paths, "Bun provenance paths do not match its exact command set");
    ensure!(targets.iter().all(|target| app.installed_paths.iter().any(|path| expand_path(path) == *target)), "A managed Bun command is absent from the application's installed paths");
    let observed = installed(&root, package)?.context("Bun package metadata no longer contains the managed package")?;
    ensure!(observed.version == version, "Bun's installed version changed since the AppTrack receipt");
    let mut installed_bins = observed.bins;
    installed_bins.sort();
    ensure!(installed_bins == bins, "Bun's installed command set changed since the AppTrack receipt");
    for (bin, target) in bins.iter().zip(&targets) {
        ensure!(
            current_hash(&root, package, bin)?.as_deref() == app.provenance.bin_sha256.get(bin).map(String::as_str),
            "Managed Bun command changed since the AppTrack receipt: {}",
            target.display()
        );
    }
    Ok(Some(RemovalPlan {
        index,
        name: app.name.clone(),
        identity: app.identity.clone(),
        package: package.into(),
        registry: registry.into(),
        root_text: root_text.into(),
        root,
        bins,
        targets,
        hashes: app.provenance.bin_sha256.clone(),
        version: version.into(),
    }))
}

fn removal_receipt_document(ledger: &Ledger, plan: &RemovalPlan) -> Result<toml_edit::DocumentMut> {
    let mut doc = ledger.document();
    let app = doc["apps"]
        .as_array_of_tables_mut()
        .context("apps must be an array of tables")?
        .get_mut(plan.index)
        .context("Application disappeared while saving removal receipt")?;
    let remaining: Vec<_> = ledger.apps[plan.index]
        .installed_paths
        .iter()
        .filter(|recorded| !plan.targets.iter().any(|target| expand_path(recorded) == *target))
        .cloned()
        .collect();
    let another_artifact_exists = remaining.iter().any(|recorded| fs::metadata(expand_path(recorded)).is_ok());
    app["installed"] = value(another_artifact_exists);
    let mut paths = Array::new();
    for path in remaining {
        paths.push(path);
    }
    app["installed_paths"] = value(paths);
    let provenance = app["provenance"].as_table_mut().context("Managed Bun provenance disappeared")?;
    provenance["managed_by_apptrack"] = value(false);
    provenance["removed_at_unix"] = value(timestamp());
    let mut action = Table::new();
    action["action"] = value("remove");
    action["result"] = value("success");
    action["at_unix"] = value(timestamp());
    action["package"] = value(&plan.package);
    action["registry"] = value(&plan.registry);
    action["version"] = value(&plan.version);
    app["last_action"] = Item::Table(action);
    Ok(doc)
}

fn exact_removal_state(plan: &RemovalPlan) -> Result<bool> {
    let Some(observed) = installed(&plan.root, &plan.package)? else {
        return Ok(false);
    };
    let mut bins = observed.bins;
    bins.sort();
    if observed.version != plan.version || bins != plan.bins {
        return Ok(false);
    }
    for bin in &plan.bins {
        if current_hash(&plan.root, &plan.package, bin)?.as_deref()
            != plan.hashes.get(bin).map(String::as_str)
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn restore_removed(plan: &RemovalPlan) -> Result<()> {
    let spec = format!("{}@{}", plan.package, plan.version);
    let workdir = bun_workdir()?;
    let output = crate::work::output_long_running(
        bun_command(&plan.root)
            .args([
                "add",
                "--global",
                "--exact",
                &spec,
                &format!("--registry={}", plan.registry),
                "--no-progress",
            ])
            .current_dir(workdir.path()),
        true,
        None,
    )
    .context("Cannot restore removed Bun package")?;
    ensure!(output.status.success(), "Bun removal rollback failed: {}", String::from_utf8_lossy(&output.stderr).trim());
    ensure!(exact_removal_state(plan)?, "Bun removal rollback did not restore the exact package and command hashes");
    Ok(())
}

pub fn remove(
    ledger: &mut Ledger,
    plan: &RemovalPlan,
    cancel: &AtomicBool,
    progress: impl Fn(&str),
) -> Result<()> {
    ensure!(removal_plan(ledger, plan.index)?.as_ref() == Some(plan), "Bun removal authority changed; review the archived record again");
    let _lock = ledger.write_lock()?;
    ensure!(
        ledger.apps.get(plan.index).is_some_and(|app| app.identity == plan.identity),
        "Application changed before Bun removal"
    );
    let result = (|| -> Result<()> {
        progress("Removing the exact Bun global package…");
        let workdir = bun_workdir()?;
        let output = crate::work::output_long_running(
            bun_command(&plan.root)
                .args([
                    "remove",
                    "--global",
                    &plan.package,
                    &format!("--registry={}", plan.registry),
                    "--no-progress",
                ])
                .current_dir(workdir.path()),
            true,
            Some(cancel),
        )
        .context("Cannot run bun remove --global")?;
        ensure!(output.status.success(), "bun remove --global failed: {}", String::from_utf8_lossy(&output.stderr).trim());
        progress("Verifying Bun package metadata and managed commands are absent…");
        ensure!(installed(&plan.root, &plan.package)?.is_none(), "Bun package metadata still contains the removed package");
        for (bin, target) in plan.bins.iter().zip(&plan.targets) {
            ensure!(current_hash(&plan.root, &plan.package, bin)?.is_none(), "Bun command remains after removal: {}", target.display());
        }
        progress("Saving the Bun removal receipt…");
        ledger.save_locked(removal_receipt_document(ledger, plan)?)?;
        Ok(())
    })();
    if let Err(error) = result {
        if exact_removal_state(plan).unwrap_or(false) {
            return Err(error.context("Bun removal failed; original installation remains exact"));
        }
        if let Err(rollback) = restore_removed(plan) {
            return Err(error.context(format!("Bun removal failed after mutation and rollback also failed: {rollback:#}")));
        }
        return Err(error.context("Bun removal failed after mutation; exact package restored"));
    }
    progress("Removed; Bun absence and AppTrack receipt verified.");
    Ok(())
}

pub fn check(ledger: &Ledger, index: usize, cancel: &AtomicBool) -> Result<Plan> {
    ledger.check_unchanged()?;
    let app = &ledger.apps[index];
    ensure!(
        app.disposition != crate::ledger::Disposition::Archived,
        "Archived applications are excluded from updates; move it to Considering first"
    );
    let (_recipe, package, registry, root_text, bins) = recipe(app)?;
    let root = root_path(root_text)?;
    let release = latest(package, registry, cancel)?;
    let observed = installed(&root, package)?;
    let installed_version = observed.as_ref().map(|installed| installed.version.clone());
    if let Some(installed) = &observed {
        for bin in bins {
            ensure!(
                installed.bins.iter().any(|found| found == bin),
                "Installed Bun package does not expose configured bin {bin}"
            );
        }
    }
    let targets: Vec<_> = bins.iter().map(|bin| root.join("bin").join(bin)).collect();
    let previous_hashes = bins
        .iter()
        .map(|bin| current_hash(&root, package, bin))
        .collect::<Result<Vec<_>>>()?;
    let hashes_match = bins.iter().zip(&previous_hashes).all(|(bin, observed)| {
        observed.as_ref().is_some_and(|hash| app.provenance.bin_sha256.get(bin) == Some(hash))
    });
    let up_to_date = app.provenance.managed_by_apptrack
        && app.provenance.source == "bun"
        && app.provenance.installer.as_deref() == Some("bun-global")
        && app.provenance.package.as_deref() == Some(package)
        && app.provenance.registry.as_deref() == Some(registry)
        && app.provenance.root.as_deref() == Some(root_text)
        && app.provenance.release.as_deref() == Some(&release)
        && installed_version.as_deref() == Some(&release)
        && hashes_match;
    ledger.check_unchanged()?;
    Ok(Plan {
        index,
        name: app.name.clone(),
        identity: app.identity.clone(),
        package: package.into(),
        registry: registry.into(),
        root_text: root_text.into(),
        root,
        bins: bins.into(),
        targets,
        previous_hashes,
        old_version: app.version.clone(),
        installed_version,
        release,
        up_to_date,
    })
}

impl Plan {
    pub fn summary(&self) -> String {
        format!(
            "{}\n{} → {}\n\nSource\nBun registry {} · package {}\n\nInstall\n{}\n\nVerification\nInstalled package identity/version, exact bin set, package-contained symlinks, and SHA-256 for each command.\n\n{}",
            self.name,
            self.old_version.as_deref().or(self.installed_version.as_deref()).unwrap_or("unknown"),
            self.release,
            self.registry,
            self.package,
            self.targets.iter().map(|path| path.display().to_string()).collect::<Vec<_>>().join("\n"),
            if self.up_to_date {
                "Installed package and commands match the current AppTrack receipt."
            } else if self.previous_hashes.iter().any(Option::is_some) {
                "Update the selected Bun global package. Record a new installation receipt."
            } else {
                "Install the selected Bun global package. Record a new installation receipt."
            }
        )
    }
}

pub fn apply(
    ledger: &mut Ledger,
    plan: &Plan,
    cancel: &AtomicBool,
    progress: impl Fn(&str),
) -> Result<()> {
    ensure!(!plan.up_to_date, "Already up to date");
    ledger.check_unchanged()?;
    ensure!(
        ledger.apps.get(plan.index).is_some_and(|app| app.identity == plan.identity),
        "Application changed since planning"
    );
    let (_recipe, package, registry, root_text, bins) = recipe(&ledger.apps[plan.index])?;
    ensure!(
        package == plan.package
            && registry == plan.registry
            && root_text == plan.root_text
            && bins == plan.bins,
        "Bun recipe changed since planning"
    );
    ensure!(root_path(root_text)? == plan.root, "Bun root changed since planning");
    let _lock = ledger.write_lock()?;
    for ((bin, expected), target) in plan
        .bins
        .iter()
        .zip(&plan.previous_hashes)
        .zip(&plan.targets)
    {
        ensure!(
            current_hash(&plan.root, &plan.package, bin)? == *expected,
            "Bun command changed since planning: {}",
            target.display()
        );
    }
    ensure!(
        installed(&plan.root, &plan.package)?.as_ref().map(|found| &found.version)
            == plan.installed_version.as_ref(),
        "Bun package changed since planning; check again"
    );
    progress("Installing the exact Bun global package version…");
    let spec = format!("{}@{}", plan.package, plan.release);
    let workdir = bun_workdir()?;
    let output = crate::work::output_long_running(
        bun_command(&plan.root)
            .args([
                "add",
                "--global",
                "--exact",
                &spec,
                &format!("--registry={}", plan.registry),
                "--no-progress",
            ])
            .current_dir(workdir.path()),
        true,
        Some(cancel),
    )
    .context("Cannot run bun add --global")?;
    ensure!(
        output.status.success(),
        "bun add --global failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    progress("Verifying Bun package metadata and command symlinks…");
    let observed = installed(&plan.root, &plan.package)?.context("Bun did not install the package")?;
    ensure!(observed.version == plan.release, "Bun installed {}, expected {}", observed.version, plan.release);
    let mut hashes = BTreeMap::new();
    for bin in &plan.bins {
        ensure!(observed.bins.iter().any(|found| found == bin), "Installed Bun package omitted bin {bin}");
        hashes.insert(
            bin.clone(),
            current_hash(&plan.root, &plan.package, bin)?
                .context("Bun reported success but its command symlink is missing")?,
        );
    }
    let receipt = receipt_document(ledger, plan, &hashes)?;
    progress("Saving the Bun installation receipt…");
    ledger.save_locked(receipt)?;
    progress("Installed; Bun metadata and AppTrack receipt verified.");
    Ok(())
}

fn receipt_document(
    ledger: &Ledger,
    plan: &Plan,
    hashes: &BTreeMap<String, String>,
) -> Result<toml_edit::DocumentMut> {
    let mut doc = ledger.document();
    let app = doc["apps"]
        .as_array_of_tables_mut()
        .unwrap()
        .get_mut(plan.index)
        .unwrap();
    if let Some(old) = app.get("provenance").and_then(Item::as_table) {
        let mut old = old.clone();
        old["ended_at_unix"] = value(timestamp());
        if let Some(version) = &plan.old_version {
            old["version"] = value(version);
        }
        if app.get("provenance_history").is_none() {
            app["provenance_history"] = Item::ArrayOfTables(toml_edit::ArrayOfTables::new());
        }
        app["provenance_history"]
            .as_array_of_tables_mut()
            .context("provenance_history must use [[apps.provenance_history]] tables")?
            .push(old);
    }
    let mut provenance = Table::new();
    provenance["source"] = value("bun");
    provenance["installer"] = value("bun-global");
    provenance["package"] = value(&plan.package);
    provenance["registry"] = value(&plan.registry);
    provenance["root"] = value(&plan.root_text);
    provenance["release"] = value(&plan.release);
    provenance["managed_by_apptrack"] = value(true);
    provenance["installed_at_unix"] = value(timestamp());
    let mut paths = Array::new();
    for target in &plan.targets {
        paths.push(target.to_string_lossy().as_ref());
    }
    provenance["installed_paths"] = value(paths);
    let mut hash_table = Table::new();
    for (bin, hash) in hashes {
        hash_table[bin] = value(hash);
    }
    provenance["bin_sha256"] = Item::Table(hash_table);
    app["provenance"] = Item::Table(provenance);
    app["version"] = value(&plan.release);
    app["installed"] = value(true);
    let old_paths = &ledger.apps[plan.index].installed_paths;
    let mut installed_paths = Array::new();
    for target in &plan.targets {
        installed_paths.push(target.to_string_lossy().as_ref());
    }
    app["installed_paths"] = value(installed_paths);
    if ledger.apps[plan.index].tags.iter().any(|tag| tag == "multi-install") {
        if let Some(installations) = app.get_mut("installations").and_then(Item::as_array_of_tables_mut) {
            for installation in installations.iter_mut() {
                let matches = installation.get("source").and_then(Item::as_str) == Some("bun")
                    && installation.get("package").and_then(Item::as_str) == Some(plan.package.as_str());
                if matches {
                    installation["version"] = value(&plan.release);
                    installation["preferred"] = value(true);
                    let mut paths = Array::new();
                    for target in &plan.targets {
                        paths.push(target.to_string_lossy().as_ref());
                    }
                    installation["installed_paths"] = value(paths);
                }
            }
        }
    }
    if let Some(launch) = &ledger.apps[plan.index].launch {
        if old_paths.iter().any(|path| expand_path(path) == expand_path(&launch.program)) {
            if let Some(target) = plan.targets.iter().find(|target| {
                target.file_name() == expand_path(&launch.program).file_name()
            }) {
                app["launch"]["program"] = value(target.to_string_lossy().as_ref());
            }
        }
    }
    let mut action = Table::new();
    action["action"] = value(if plan.previous_hashes.iter().any(Option::is_some) {
        "update"
    } else {
        "install"
    });
    action["result"] = value("success");
    action["at_unix"] = value(timestamp());
    action["release"] = value(&plan.release);
    app["last_action"] = Item::Table(action);
    Ok(doc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bun_commands_pin_package_and_bin_roots_together() -> Result<()> {
        let root = Path::new("/tmp/apptrack-bun-root");
        let command = bun_command(root);
        let environments: BTreeMap<_, _> = command
            .get_envs()
            .filter_map(|(key, value)| value.map(|value| (key.to_owned(), value.to_owned())))
            .collect();
        assert_eq!(environments.get(std::ffi::OsStr::new("BUN_INSTALL")), Some(&root.as_os_str().to_owned()));
        assert_eq!(
            environments.get(std::ffi::OsStr::new("BUN_INSTALL_BIN")),
            Some(&root.join("bin").into_os_string())
        );
        Ok(())
    }

    #[test]
    fn latest_version_json_is_exact() -> Result<()> {
        assert_eq!(parse_latest(r#""1.2.3""#)?, "1.2.3");
        assert_eq!(parse_latest(r#"{"version":"2.0.0-beta.1"}"#)?, "2.0.0-beta.1");
        assert!(parse_latest(r#"{"version":"latest"}"#).is_err());
        Ok(())
    }

    #[test]
    fn installed_state_requires_exact_package_metadata_and_contained_bin_links() -> Result<()> {
        let root = tempfile::tempdir()?;
        let package = root
            .path()
            .join("install/global/node_modules/@scope/tool");
        fs::create_dir_all(package.join("dist"))?;
        fs::create_dir(root.path().join("bin"))?;
        fs::write(
            package.join("package.json"),
            r#"{"name":"@scope/tool","version":"1.2.3","bin":{"tool":"dist/cli.js"}}"#,
        )?;
        fs::write(package.join("dist/cli.js"), "#!/usr/bin/env bun\n")?;
        std::os::unix::fs::symlink(
            "../install/global/node_modules/@scope/tool/dist/cli.js",
            root.path().join("bin/tool"),
        )?;
        let found = installed(root.path(), "@scope/tool")?.unwrap();
        assert_eq!(found.version, "1.2.3");
        assert_eq!(found.bins, ["tool"]);
        assert!(current_hash(root.path(), "@scope/tool", "tool")?.is_some());
        Ok(())
    }

    #[test]
    fn removal_authority_requires_exact_metadata_paths_and_command_hashes() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("bun");
        let package = root.join("install/global/node_modules/@scope/tool");
        let target = root.join("bin/tool");
        fs::create_dir_all(package.join("dist"))?;
        fs::create_dir_all(target.parent().unwrap())?;
        fs::write(
            package.join("package.json"),
            r#"{"name":"@scope/tool","version":"1.2.3","bin":{"tool":"dist/cli.js"}}"#,
        )?;
        let command = package.join("dist/cli.js");
        fs::write(&command, "#!/usr/bin/env bun\n")?;
        std::os::unix::fs::symlink(
            "../install/global/node_modules/@scope/tool/dist/cli.js",
            &target,
        )?;
        let hash = current_hash(&root, "@scope/tool", "tool")?.unwrap();
        let ledger_path = dir.path().join("ledger.toml");
        fs::write(
            &ledger_path,
            format!(
                r#"schema_version = 1
[[apps]]
identity = "npm:@scope/tool"
name = "tool"
category = "Tools"
disposition = "considering"
installed = true
version = "1.2.3"
installed_paths = [{target:?}]
[apps.provenance]
source = "bun"
installer = "bun-global"
package = "@scope/tool"
registry = "https://registry.npmjs.org"
root = {root:?}
release = "1.2.3"
managed_by_apptrack = true
installed_paths = [{target:?}]
[apps.provenance.bin_sha256]
tool = "{hash}"
"#,
                root = root.to_string_lossy(),
                target = target.to_string_lossy(),
            ),
        )?;
        let ledger = Ledger::open(&ledger_path)?;
        assert!(removal_plan(&ledger, 0)?.is_some());
        fs::write(&command, "changed\n")?;
        assert!(removal_plan(&ledger, 0).unwrap_err().to_string().contains("changed since"));
        Ok(())
    }

    #[test]
    fn bun_receipt_records_exact_authority_and_switches_an_imported_launch_path() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("bun");
        fs::create_dir_all(root.join("bin"))?;
        let ledger_path = dir.path().join("ledger.toml");
        let old = dir.path().join("old-bin/tool");
        fs::create_dir_all(old.parent().unwrap())?;
        fs::write(&old, "old")?;
        fs::write(
            &ledger_path,
            format!(
                r#"schema_version = 1
[[apps]]
identity = "npm:@scope/tool"
name = "tool"
category = "Tools"
tags = ["multi-install"]
disposition = "using"
installed = true
version = "1.0.0"
installed_paths = [{old:?}]
[apps.provenance]
source = "bun"
package = "@scope/tool"
installer = "bun-global"
managed_by_apptrack = false
[apps.launch]
program = {old:?}
args = []
gui = false
[apps.recipe]
source = "bun"
installer = "bun-global"
package = "@scope/tool"
registry = "https://registry.npmjs.org"
root = {root:?}
bins = ["tool"]
[[apps.installations]]
source = "bun"
installer = "bun-global"
package = "@scope/tool"
version = "1.0.0"
installed_paths = [{old:?}]
preferred = true
"#,
                old = old.to_string_lossy(),
                root = root.to_string_lossy(),
            ),
        )?;
        let mut ledger = Ledger::open(&ledger_path)?;
        let target = root.join("bin/tool");
        let plan = Plan {
            index: 0,
            name: "tool".into(),
            identity: "npm:@scope/tool".into(),
            package: "@scope/tool".into(),
            registry: "https://registry.npmjs.org".into(),
            root_text: root.to_string_lossy().into(),
            root,
            bins: vec!["tool".into()],
            targets: vec![target.clone()],
            previous_hashes: vec![None],
            old_version: Some("1.0.0".into()),
            installed_version: Some("1.0.0".into()),
            release: "2.0.0".into(),
            up_to_date: false,
        };
        let hashes = BTreeMap::from([("tool".into(), "abc123".into())]);
        let receipt = receipt_document(&ledger, &plan, &hashes)?;
        let _lock = ledger.write_lock()?;
        ledger.save_locked(receipt)?;
        let reopened = Ledger::open(&ledger.path)?;
        assert_eq!(reopened.apps[0].version.as_deref(), Some("2.0.0"));
        assert_eq!(reopened.apps[0].provenance.package.as_deref(), Some("@scope/tool"));
        assert_eq!(reopened.apps[0].provenance.bin_sha256.get("tool").map(String::as_str), Some("abc123"));
        assert_eq!(reopened.apps[0].launch.as_ref().unwrap().program, target.to_string_lossy());
        assert_eq!(reopened.apps[0].installations[0].version.as_deref(), Some("2.0.0"));
        assert_eq!(reopened.apps[0].installations[0].installed_paths, [target.to_string_lossy()]);
        assert!(reopened.record(0).contains("[[apps.provenance_history]]"));
        Ok(())
    }
}
