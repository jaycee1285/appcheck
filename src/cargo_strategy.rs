use crate::{
    ledger::{App, Ledger, expand_path},
    update::{Recipe, timestamp},
};
use anyhow::{Context, Result, bail, ensure};
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
    pub repo: String,
    pub local_path: Option<String>,
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
            "Remove managed Cargo package?\n\n{} {}\nsource {} · package {}\nroot {}\n{}\n\nCargo's package receipt and every managed command hash must still match. The archive decision and reason are already saved; keeping the package is the default.",
            self.name,
            self.version,
            self.registry,
            self.package,
            self.root.display(),
            self.targets.iter().map(|path| path.display().to_string()).collect::<Vec<_>>().join("\n"),
        )
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

fn current_hash(path: &Path) -> Result<Option<String>> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            ensure!(
                meta.file_type().is_file(),
                "Refusing Cargo binary symlink or non-file: {}",
                path.display()
            );
            Ok(Some(sha256(path)?))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn exact_name(value: &str, label: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.bytes().all(|b| {
                b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')
            }),
        "Cargo {label} must be one exact name"
    );
    Ok(())
}

fn recipe(app: &App) -> Result<(&Recipe, &str, &str, &str, &[String])> {
    let recipe = app.recipe.as_ref().context("No update recipe recorded for this app")?;
    ensure!(
        recipe.source == "cargo" && recipe.installer == "cargo-install",
        "Expected a cargo / cargo-install recipe"
    );
    let package = recipe.package.as_deref().context("Cargo recipe requires package")?;
    let registry = recipe.registry.as_deref().context("Cargo recipe requires registry")?;
    let root = recipe.root.as_deref().context("Cargo recipe requires root")?;
    exact_name(package, "package")?;
    exact_name(registry, "registry")?;
    ensure!(matches!(registry, "crates-io" | "git" | "path"), "Unsupported Cargo install source");
    if registry == "git" {
        ensure!(app.identity == format!("https://github.com/{}", recipe.repo), "Cargo Git source must match the app identity");
        ensure!(recipe.repo.split('/').count() == 2 && recipe.repo.split('/').all(crate::update::safe_name), "Cargo Git repository is not exact");
    }
    if registry == "path" {
        let path = recipe.remote.as_deref().context("Local Cargo recipe requires checkout path")?;
        let checkout = expand_path(path);
        ensure!(checkout.is_absolute() && checkout.starts_with(expand_path("~/repos/dz")) && checkout.join("Cargo.toml").is_file(), "Local Cargo checkout must be under ~/repos/dz and have Cargo.toml");
        ensure!(recipe.repo.split('/').count() == 2 && recipe.repo.split('/').all(crate::update::safe_name), "Local Cargo recipe needs the exact GitHub repository");
    }
    ensure!(!recipe.bins.is_empty(), "Cargo recipe requires at least one exact bin");
    let mut unique = HashSet::new();
    for bin in &recipe.bins {
        exact_name(bin, "bin")?;
        ensure!(unique.insert(bin), "Cargo recipe repeats bin {bin}");
    }
    Ok((recipe, package, registry, root, &recipe.bins))
}

fn root_path(raw: &str) -> Result<PathBuf> {
    let root = expand_path(raw);
    ensure!(
        root.is_absolute()
            && !root.components().any(|component| matches!(component, Component::ParentDir)),
        "Cargo root must be an absolute path without '..'"
    );
    ensure!(root.is_dir(), "Cargo root must already exist: {}", root.display());
    ensure!(!root.starts_with("/nix/store"), "Cargo root cannot be in the Nix store");
    Ok(root)
}

pub(crate) fn parse_info(output: &str) -> Result<String> {
    let versions: Vec<_> = output
        .lines()
        .filter_map(|line| line.strip_prefix("version:").map(str::trim))
        .filter_map(|version| version.split_whitespace().next())
        .collect();
    ensure!(versions.len() == 1, "Cargo info did not report exactly one version");
    ensure!(
        versions[0].starts_with(|character: char| character.is_ascii_digit())
            && versions[0].bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+')
            }),
        "Cargo info reported an invalid version"
    );
    Ok(versions[0].to_string())
}

pub(crate) fn parse_receipt(
    contents: &str,
    package: &str,
) -> Result<Option<(String, Vec<String>)>> {
    let receipt: serde_json::Value =
        serde_json::from_str(contents).context("Invalid Cargo .crates2.json receipt")?;
    let installs = receipt
        .get("installs")
        .and_then(serde_json::Value::as_object)
        .context("Cargo .crates2.json has no installs object")?;
    let prefix = format!("{package} ");
    let suffix = " (registry+https://github.com/rust-lang/crates.io-index)";
    let matches: Vec<_> = installs
        .iter()
        .filter_map(|(identity, entry)| {
            let version = identity.strip_prefix(&prefix)?.strip_suffix(suffix)?;
            Some((version, entry))
        })
        .collect();
    ensure!(
        matches.len() <= 1,
        "Cargo receipt contains multiple crates.io identities for {package}"
    );
    let Some((version, entry)) = matches.into_iter().next() else {
        return Ok(None);
    };
    let bins = entry
        .get("bins")
        .and_then(serde_json::Value::as_array)
        .context("Cargo receipt has no bin list")?
        .iter()
        .map(|bin| {
            bin.as_str()
                .map(str::to_string)
                .context("Cargo receipt contains a non-string bin")
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Some((version.to_string(), bins)))
}

fn output(command: &mut Command, cancel: &AtomicBool, operation: &str) -> Result<String> {
    let result = crate::work::output(command, true, Some(cancel))
        .with_context(|| format!("Cannot run {operation}"))?;
    ensure!(
        result.status.success(),
        "{operation} failed: {}",
        String::from_utf8_lossy(&result.stderr).trim()
    );
    String::from_utf8(result.stdout).with_context(|| format!("{operation} returned non-UTF-8 output"))
}

fn latest(package: &str, registry: &str, cancel: &AtomicBool) -> Result<String> {
    parse_info(&output(
        Command::new("cargo").args([
            "info", package, "--registry", registry, "--color", "never",
        ]),
        cancel,
        "cargo info",
    )?)
}

fn installed(root: &Path, package: &str) -> Result<Option<(String, Vec<String>)>> {
    match fs::read_to_string(root.join(".crates2.json")) {
        Ok(contents) => parse_receipt(&contents, package),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context("Cannot read Cargo .crates2.json receipt"),
    }
}

fn source_revision(recipe: &Recipe, cancel: &AtomicBool) -> Result<String> {
    let source = recipe.registry.as_deref().context("Missing Cargo source")?;
    let revision = if source == "git" {
        output(Command::new("gh").args(["api", &format!("repos/{}/commits/HEAD", recipe.repo), "--jq", ".sha"])
            .env("GH_PROMPT_DISABLED", "1"), cancel, "GitHub commit lookup")?
    } else {
        let path = recipe.remote.as_deref().context("Missing local checkout")?;
        let root = output(Command::new("git").args(["-C", path, "rev-parse", "--show-toplevel"]), cancel, "checkout root")?;
        let root = root.trim();
        ensure!(Path::new(root).starts_with(expand_path("~/repos/dz")), "Local Cargo checkout moved outside ~/repos/dz");
        let origin = output(Command::new("git").args(["-C", root, "remote", "get-url", "origin"]), cancel, "checkout origin")?;
        let origin = origin.trim().replace("git@github.com:", "https://github.com/");
        ensure!(crate::ledger::normalize_identity(&origin) == crate::ledger::normalize_identity(&format!("https://github.com/{}", recipe.repo)),
            "Local checkout origin changed since Cargo review");
        let dirty = output(Command::new("git").args(["-C", root, "status", "--porcelain"]), cancel, "checkout status")?;
        ensure!(dirty.trim().is_empty(), "Local checkout has uncommitted changes; commit before an exact Cargo install");
        output(Command::new("git").args(["-C", path, "rev-parse", "HEAD"]), cancel, "checkout revision")?
    };
    let revision = revision.trim().to_ascii_lowercase();
    ensure!(revision.len() == 40 && revision.bytes().all(|b| b.is_ascii_hexdigit()), "Cargo source revision is not a full Git SHA");
    Ok(revision)
}

fn source_receipt(root: &Path, recipe: &Recipe, release: &str) -> Result<Option<(String, Vec<String>)>> {
    let contents = match fs::read_to_string(root.join(".crates2.json")) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("Cannot read Cargo receipt"),
    };
    let data: serde_json::Value = serde_json::from_str(&contents)?;
    let installs = data.get("installs").and_then(serde_json::Value::as_object).context("Cargo receipt has no installs")?;
    let package = recipe.package.as_deref().context("Missing Cargo package")?;
    let prefix = format!("{package} ");
    let route = recipe.registry.as_deref().unwrap_or_default();
    let matches: Vec<_> = installs.iter().filter(|(key, _)| key.starts_with(&prefix)).filter(|(key, _)| {
        if route == "git" { key.contains(&format!("(git+https://github.com/{}", recipe.repo)) }
        else { recipe.remote.as_ref().is_some_and(|path| key.contains(&format!("(path+file://{}", expand_path(path).display()))) }
    }).collect();
    ensure!(matches.len() <= 1, "Cargo receipt has multiple identities for {package}");
    let Some((identity, entry)) = matches.first() else { return Ok(None); };
    let version = if route == "git" {
        let hash = identity.rsplit_once('#').and_then(|(_, rest)| rest.strip_suffix(')')).context("Cargo Git receipt has no commit")?;
        if release.starts_with(hash) { release.to_string() } else { hash.to_string() }
    } else { release.to_string() };
    let bins = entry.get("bins").and_then(serde_json::Value::as_array).context("Cargo receipt has no bin list")?
        .iter().map(|bin| bin.as_str().map(str::to_owned).context("Cargo receipt contains a non-string bin"))
        .collect::<Result<Vec<_>>>()?;
    Ok(Some((version, bins)))
}

fn package_receipt_count(root: &Path, package: &str) -> Result<usize> {
    let contents = match fs::read_to_string(root.join(".crates2.json")) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error).context("Cannot read Cargo receipt"),
    };
    let data: serde_json::Value = serde_json::from_str(&contents)?;
    let installs = data.get("installs").and_then(serde_json::Value::as_object).context("Cargo receipt has no installs")?;
    Ok(installs.keys().filter(|identity| identity.starts_with(&format!("{package} "))).count())
}

pub fn check(ledger: &Ledger, index: usize, cancel: &AtomicBool) -> Result<Plan> {
    ledger.check_unchanged()?;
    let app = &ledger.apps[index];
    ensure!(
        app.disposition != crate::ledger::Disposition::Archived,
        "Archived applications are excluded from updates; move it to Considering first"
    );
    let (source, package, registry, root_text, _bins) = recipe(app)?;
    let root = root_path(root_text)?;
    let release = if registry == "crates-io" { latest(package, registry, cancel)? }
        else if app.provenance.managed_by_apptrack && app.provenance.source == "cargo"
            && app.provenance.registry.as_deref() == Some(registry) { source_revision(source, cancel)? }
        else { match &source.branch { Some(pinned) => pinned.clone(), None => source_revision(source, cancel)? } };
    let installed = if registry == "crates-io" { installed(&root, package)? }
        else { source_receipt(&root, source, &release)? };
    let plan = make_plan(app, index, release, installed)?;
    ledger.check_unchanged()?;
    Ok(plan)
}

pub(crate) fn make_plan(
    app: &App,
    index: usize,
    release: String,
    installed: Option<(String, Vec<String>)>,
) -> Result<Plan> {
    let (source, package, registry, root_text, bins) = recipe(app)?;
    let root = root_path(root_text)?;
    let targets: Vec<_> = bins.iter().map(|bin| root.join("bin").join(bin)).collect();
    let previous_hashes = targets
        .iter()
        .map(|target| current_hash(target))
        .collect::<Result<Vec<_>>>()?;
    let installed_version = installed.as_ref().map(|(version, _)| version.clone());
    if let Some((_, installed_bins)) = &installed {
        for bin in bins {
            ensure!(
                installed_bins.iter().any(|installed| installed == bin),
                "Cargo receipt for {package} does not contain configured bin {bin}"
            );
        }
    }
    let hashes_match = bins.iter().zip(&previous_hashes).all(|(bin, observed)| {
        observed.as_ref().is_some_and(|hash| app.provenance.bin_sha256.get(bin) == Some(hash))
    });
    let up_to_date = app.provenance.managed_by_apptrack
        && app.provenance.source == "cargo"
        && app.provenance.installer.as_deref() == Some("cargo-install")
        && app.provenance.package.as_deref() == Some(package)
        && app.provenance.registry.as_deref() == Some(registry)
        && app.provenance.root.as_deref() == Some(root_text)
        && app.provenance.release.as_deref() == Some(&release)
        && installed_version.as_deref() == Some(&release)
        && hashes_match;
    Ok(Plan {
        index,
        name: app.name.clone(),
        identity: app.identity.clone(),
        package: package.into(),
        registry: registry.into(),
        repo: source.repo.clone(),
        local_path: source.remote.clone(),
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
            "{}\n{} → {}\n\nSource\nCargo {} · package {}\n\nInstall\n{}\n\nVerification\nCargo's installed-package receipt, exact bin set, and SHA-256 for each managed binary.\n\n{}",
            self.name,
            self.old_version.as_deref().or(self.installed_version.as_deref()).unwrap_or("unknown"),
            self.release,
            self.registry,
            self.package,
            self.targets.iter().map(|path| path.display().to_string()).collect::<Vec<_>>().join("\n"),
            if self.up_to_date {
                "Installed package and binaries match the current AppTrack receipt."
            } else if self.previous_hashes.iter().any(Option::is_some) {
                "Replace the selected Cargo package binaries. Record a new installation receipt."
            } else {
                "Install the selected Cargo package binaries. Record a new installation receipt."
            }
        )
    }
}

#[derive(Clone)]
struct Backup {
    path: PathBuf,
    contents: Option<(Vec<u8>, fs::Permissions)>,
}

fn backup(path: PathBuf) -> Result<Backup> {
    let contents = match fs::symlink_metadata(&path) {
        Ok(meta) => {
            ensure!(meta.file_type().is_file(), "Refusing to back up non-file: {}", path.display());
            Some((fs::read(&path)?, meta.permissions()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    Ok(Backup { path, contents })
}

fn restore(backups: &[Backup]) -> Result<()> {
    for backup in backups {
        match &backup.contents {
            Some((contents, permissions)) => {
                if let Some(parent) = backup.path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&backup.path, contents)?;
                fs::set_permissions(&backup.path, permissions.clone())?;
            }
            None => match fs::remove_file(&backup.path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            },
        }
    }
    Ok(())
}

pub fn removal_plan(ledger: &Ledger, index: usize) -> Result<Option<RemovalPlan>> {
    ledger.check_unchanged()?;
    let app = ledger.apps.get(index).context("Application no longer exists")?;
    if !app.provenance.managed_by_apptrack || app.provenance.source != "cargo" {
        return Ok(None);
    }
    ensure!(app.provenance.installer.as_deref() == Some("cargo-install"), "Managed Cargo receipt has the wrong installer");
    ensure!(app.installed == Some(true), "Managed Cargo receipt does not say the package is installed");
    let package = app.provenance.package.as_deref().context("Managed Cargo receipt has no package")?;
    let registry = app.provenance.registry.as_deref().context("Managed Cargo receipt has no registry")?;
    let root_text = app.provenance.root.as_deref().context("Managed Cargo receipt has no root")?;
    let version = app.provenance.release.as_deref().context("Managed Cargo receipt has no version")?;
    exact_name(package, "package")?;
    exact_name(registry, "registry")?;
    ensure!(matches!(registry, "crates-io" | "git" | "path"), "Unsupported Cargo removal source");
    let root = root_path(root_text)?;
    ensure!(!app.provenance.bin_sha256.is_empty(), "Managed Cargo receipt has no command hashes");
    let bins: Vec<_> = app.provenance.bin_sha256.keys().cloned().collect();
    for bin in &bins {
        exact_name(bin, "bin")?;
    }
    let targets: Vec<_> = bins.iter().map(|bin| root.join("bin").join(bin)).collect();
    let receipt_paths: HashSet<_> = app.provenance.installed_paths.iter().map(|path| expand_path(path)).collect();
    let target_paths: HashSet<_> = targets.iter().cloned().collect();
    ensure!(receipt_paths == target_paths, "Cargo provenance paths do not match its exact command set");
    ensure!(targets.iter().all(|target| app.installed_paths.iter().any(|path| expand_path(path) == *target)), "A managed Cargo command is absent from the application's installed paths");
    ensure!(package_receipt_count(&root, package)? == 1, "Cargo has multiple or no receipts for this package; removal needs one exact identity");
    let source = Recipe { source: "cargo".into(), installer: "cargo-install".into(),
        package: Some(package.into()), registry: Some(registry.into()),
        repo: app.provenance.repo.clone().unwrap_or_default(), remote: app.provenance.remote.clone(),
        ..Recipe::default() };
    let receipt = if registry == "crates-io" { installed(&root, package)? }
        else { source_receipt(&root, &source, version)? };
    let (installed_version, mut installed_bins) = receipt
        .context("Cargo's package receipt no longer contains the managed package")?;
    ensure!(installed_version == version, "Cargo's installed version changed since the AppTrack receipt");
    installed_bins.sort();
    ensure!(installed_bins == bins, "Cargo's installed command set changed since the AppTrack receipt");
    for (bin, target) in bins.iter().zip(&targets) {
        ensure!(
            current_hash(target)?.as_deref() == app.provenance.bin_sha256.get(bin).map(String::as_str),
            "Managed Cargo command changed since the AppTrack receipt: {}",
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
    let app = doc["apps"].as_array_of_tables_mut().context("apps must be an array of tables")?
        .get_mut(plan.index).context("Application disappeared while saving removal receipt")?;
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
    let provenance = app["provenance"].as_table_mut().context("Managed Cargo provenance disappeared")?;
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

pub fn remove(
    ledger: &mut Ledger,
    plan: &RemovalPlan,
    cancel: &AtomicBool,
    progress: impl Fn(&str),
) -> Result<()> {
    ensure!(removal_plan(ledger, plan.index)?.as_ref() == Some(plan), "Cargo removal authority changed; review the archived record again");
    let _lock = ledger.write_lock()?;
    let mut backup_paths = plan.targets.clone();
    backup_paths.push(plan.root.join(".crates.toml"));
    backup_paths.push(plan.root.join(".crates2.json"));
    let backups = backup_paths.into_iter().map(backup).collect::<Result<Vec<_>>>()?;
    let result = (|| -> Result<()> {
        progress("Uninstalling the exact Cargo package…");
        let output = crate::work::output_long_running(
            Command::new("cargo")
                .args(["uninstall", &plan.package, "--root"])
                .arg(&plan.root)
                .args(["--color", "never"]),
            true,
            Some(cancel),
        )
        .context("Cannot run cargo uninstall")?;
        ensure!(output.status.success(), "cargo uninstall failed: {}", String::from_utf8_lossy(&output.stderr).trim());
        progress("Verifying Cargo's receipt and managed commands are absent…");
        ensure!(package_receipt_count(&plan.root, &plan.package)? == 0, "Cargo receipt still contains the removed package");
        for target in &plan.targets {
            ensure!(current_hash(target)?.is_none(), "Cargo command remains after uninstall: {}", target.display());
        }
        progress("Saving the Cargo removal receipt…");
        ledger.save_locked(removal_receipt_document(ledger, plan)?)?;
        Ok(())
    })();
    if let Err(error) = result {
        if let Err(rollback) = restore(&backups) {
            return Err(error.context(format!("Cargo removal failed and rollback also failed: {rollback:#}")));
        }
        return Err(error.context("Cargo removal failed; commands and Cargo receipts restored"));
    }
    progress("Removed; Cargo absence and AppTrack receipt verified.");
    Ok(())
}

pub fn apply(
    ledger: &mut Ledger,
    plan: &Plan,
    cancel: &AtomicBool,
    progress: impl Fn(&str),
) -> Result<()> {
    let report = &progress;
    apply_with(
        ledger,
        plan,
        cancel,
        report,
        |plan, cancel| install_exact(plan, cancel, report),
        |plan, _| {
            if plan.registry == "crates-io" { installed(&plan.root, &plan.package) }
            else {
                let recipe = Recipe { source: "cargo".into(), installer: "cargo-install".into(),
                    package: Some(plan.package.clone()), registry: Some(plan.registry.clone()),
                    repo: plan.repo.clone(), remote: plan.local_path.clone(), ..Recipe::default() };
                source_receipt(&plan.root, &recipe, &plan.release)
            }
        },
    )
}

fn install_exact(plan: &Plan, cancel: &AtomicBool, progress: &dyn Fn(&str)) -> Result<()> {
    if plan.registry == "path" {
        let source = Recipe { registry: Some("path".into()), repo: plan.repo.clone(),
            remote: plan.local_path.clone(), ..Recipe::default() };
        ensure!(source_revision(&source, cancel)? == plan.release, "Local checkout changed since planning");
    }
    let version = format!("={}", plan.release);
    let mut command = Command::new("cargo");
    command.arg("install");
    match plan.registry.as_str() {
        "crates-io" => { command.args([&plan.package, "--version", &version, "--registry", &plan.registry]); }
        "git" => { command.args(["--git", &format!("https://github.com/{}", plan.repo), "--rev", &plan.release, &plan.package]); }
        "path" => { command.args(["--path", plan.local_path.as_deref().context("Missing checkout path")?]); }
        _ => bail!("Unsupported Cargo source"),
    }
    command.arg("--root").arg(&plan.root).args(["--force", "--color", "never"]);
    for bin in &plan.bins {
        command.args(["--bin", bin]);
    }
    let result = crate::work::output_long_running_progress(&mut command, true, Some(cancel), &|line| {
        let line = line.trim();
        if ["Updating ", "Downloading ", "Downloaded ", "Compiling ", "Installing ", "Installed ", "Finished ", "error:"]
            .iter().any(|prefix| line.starts_with(prefix)) {
            let short: String = line.chars().take(140).collect();
            progress(&format!("Cargo · {short}"));
        }
    })
        .context("Cannot run cargo install")?;
    ensure!(
        result.status.success(),
        "cargo install failed: {}",
        String::from_utf8_lossy(&result.stderr).trim()
    );
    if plan.registry == "path" {
        let source = Recipe { registry: Some("path".into()), repo: plan.repo.clone(),
            remote: plan.local_path.clone(), ..Recipe::default() };
        ensure!(source_revision(&source, cancel)? == plan.release, "Local checkout changed during Cargo install");
    }
    Ok(())
}

fn apply_with(
    ledger: &mut Ledger,
    plan: &Plan,
    cancel: &AtomicBool,
    progress: impl Fn(&str),
    install_package: impl FnOnce(&Plan, &AtomicBool) -> Result<()>,
    inspect_installed: impl Fn(&Plan, &AtomicBool) -> Result<Option<(String, Vec<String>)>>,
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
            && bins == plan.bins
            && _recipe.repo == plan.repo
            && _recipe.remote == plan.local_path,
        "Cargo recipe changed since planning"
    );
    ensure!(root_path(root_text)? == plan.root, "Cargo root changed since planning");
    let _lock = ledger.write_lock()?;
    for (target, expected) in plan.targets.iter().zip(&plan.previous_hashes) {
        ensure!(current_hash(target)? == *expected, "Cargo binary changed since planning: {}", target.display());
    }
    let before_installed = inspect_installed(plan, cancel)?;
    ensure!(
        before_installed.as_ref().map(|(version, _)| version) == plan.installed_version.as_ref(),
        "Cargo package receipt changed since planning; check again"
    );
    let mut backup_paths = plan.targets.clone();
    backup_paths.push(plan.root.join(".crates.toml"));
    backup_paths.push(plan.root.join(".crates2.json"));
    let backups = backup_paths.into_iter().map(backup).collect::<Result<Vec<_>>>()?;
    let result = (|| {
        progress("Installing the exact Cargo package version…");
        install_package(plan, cancel)?;
        progress("Verifying Cargo's receipt and installed binaries…");
        let Some((version, installed_bins)) = inspect_installed(plan, cancel)? else {
            bail!("Cargo did not record the installed package")
        };
        ensure!(version == plan.release, "Cargo recorded version {version}, expected {}", plan.release);
        let mut hashes = BTreeMap::new();
        for (bin, target) in plan.bins.iter().zip(&plan.targets) {
            ensure!(installed_bins.iter().any(|found| found == bin), "Cargo receipt omitted bin {bin}");
            hashes.insert(bin.clone(), current_hash(target)?.context("Cargo reported success but its binary is missing")?);
        }
        let receipt = receipt_document(ledger, plan, &hashes)?;
        progress("Saving the Cargo installation receipt…");
        ledger.save_locked(receipt)?;
        Ok(())
    })();
    if let Err(error) = result {
        if let Err(rollback) = restore(&backups) {
            return Err(error.context(format!("Cargo installation failed and rollback also failed: {rollback:#}")));
        }
        return Err(error.context("Cargo installation failed; previous binaries and Cargo receipt restored"));
    }
    progress("Installed; Cargo and AppTrack receipts verified.");
    Ok(())
}

fn receipt_document(ledger: &Ledger, plan: &Plan, hashes: &BTreeMap<String, String>) -> Result<toml_edit::DocumentMut> {
    let mut doc = ledger.document();
    let app = doc["apps"].as_array_of_tables_mut().unwrap().get_mut(plan.index).unwrap();
    let previous = &ledger.apps[plan.index];
    if let Some(launch) = &previous.launch {
        let launch_is_managed_command = previous.installed_paths.iter().any(|path| expand_path(path) == expand_path(&launch.program))
            || previous.recipe.as_ref().is_some_and(|recipe| !recipe.destination.is_empty()
                && expand_path(&recipe.destination) == expand_path(&launch.program))
            || previous.recipe.as_ref().and_then(|recipe| recipe.launch_from.as_ref())
                .is_some_and(|from| expand_path(from) == expand_path(&launch.program));
        if launch_is_managed_command
            && plan.targets.len() == 1
            && expand_path(&launch.program) != plan.targets[0]
        {
            app["launch"]["program"] = value(plan.targets[0].to_string_lossy().as_ref());
        }
    }
    if let Some(recipe) = app.get_mut("recipe").and_then(Item::as_table_mut) {
        recipe.remove("launch_from");
    }
    if let Some(old) = app.get("provenance").and_then(Item::as_table) {
        let mut old = old.clone();
        old["ended_at_unix"] = value(timestamp());
        if let Some(version) = &plan.old_version {
            old["version"] = value(version);
        }
        if app.get("provenance_history").is_none() {
            app["provenance_history"] = Item::ArrayOfTables(toml_edit::ArrayOfTables::new());
        }
        app["provenance_history"].as_array_of_tables_mut()
            .context("provenance_history must use [[apps.provenance_history]] tables")?
            .push(old);
    }
    let mut provenance = Table::new();
    provenance["source"] = value("cargo");
    provenance["installer"] = value("cargo-install");
    provenance["package"] = value(&plan.package);
    provenance["registry"] = value(&plan.registry);
    if plan.registry == "git" || plan.registry == "path" {
        provenance["repo"] = value(&plan.repo);
        provenance["commit"] = value(&plan.release);
        if let Some(path) = &plan.local_path { provenance["remote"] = value(path); }
        app["recipe"]["branch"] = value(&plan.release);
    }
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
    let mut installed_paths = Array::new();
    for target in &plan.targets {
        installed_paths.push(target.to_string_lossy().as_ref());
    }
    app["installed_paths"] = value(installed_paths);
    let mut action = Table::new();
    action["action"] = value(if plan.previous_hashes.iter().any(Option::is_some) { "update" } else { "install" });
    action["result"] = value("success");
    action["at_unix"] = value(timestamp());
    action["release"] = value(&plan.release);
    app["last_action"] = Item::Table(action);
    Ok(doc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn git_and_checkout_receipts_bind_to_the_selected_source() -> Result<()> {
        let root = tempfile::tempdir()?;
        let sha = "0123456789abcdef0123456789abcdef01234567";
        fs::write(root.path().join(".crates2.json"), format!(r#"{{"installs":{{"tool 1.0.0 (git+https://github.com/owner/tool?rev={sha}#{sha})":{{"bins":["tool"]}},"tool 1.0.0 (path+file:///tmp/other)":{{"bins":["other"]}}}}}}"#))?;
        let git = Recipe { package: Some("tool".into()), registry: Some("git".into()), repo: "owner/tool".into(), ..Recipe::default() };
        assert_eq!(source_receipt(root.path(), &git, sha)?, Some((sha.into(), vec!["tool".into()])));
        let wrong = Recipe { repo: "elsewhere/tool".into(), ..git };
        assert_eq!(source_receipt(root.path(), &wrong, sha)?, None);
        let local = Recipe { package: Some("tool".into()), registry: Some("path".into()),
            remote: Some("/tmp/other".into()), ..Recipe::default() };
        assert_eq!(source_receipt(root.path(), &local, sha)?, Some((sha.into(), vec!["other".into()])));
        assert_eq!(package_receipt_count(root.path(), "tool")?, 2);
        Ok(())
    }

    #[test]
    fn cargo_output_parsers_require_exact_package_registry_and_version() -> Result<()> {
        assert_eq!(
            parse_info("crate\nversion: 1.2.3 (latest 2.0.0)\n")?,
            "1.2.3"
        );
        assert!(parse_info("version: 1\nversion: 2\n").is_err());
        let receipt = r#"{"installs":{"other 9.0.0 (registry+https://github.com/rust-lang/crates.io-index)":{"bins":["other"]},"tool 1.2.3 (registry+https://github.com/rust-lang/crates.io-index)":{"bins":["tool","tool-ui"]},"tool 8.0.0 (git+https://example.invalid/tool)":{"bins":["wrong-source"]}}}"#;
        assert_eq!(
            parse_receipt(receipt, "tool")?,
            Some(("1.2.3".into(), vec!["tool".into(), "tool-ui".into()]))
        );
        assert_eq!(parse_receipt(receipt, "missing")?, None);
        Ok(())
    }

    #[test]
    fn inspecting_an_empty_cargo_root_is_read_only() -> Result<()> {
        let root = tempfile::tempdir()?;
        assert_eq!(installed(root.path(), "tool")?, None);
        assert!(fs::read_dir(root.path())?.next().is_none());
        Ok(())
    }

    #[test]
    fn cargo_removal_authority_requires_receipt_version_bins_paths_and_hashes() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("cargo");
        fs::create_dir_all(root.join("bin"))?;
        let target = root.join("bin/tool");
        fs::write(&target, "managed cargo command")?;
        let hash = sha256(&target)?;
        fs::write(
            root.join(".crates2.json"),
            r#"{"installs":{"tool-package 1.2.3 (registry+https://github.com/rust-lang/crates.io-index)":{"bins":["tool"]}}}"#,
        )?;
        let ledger_path = dir.path().join("ledger.toml");
        fs::write(
            &ledger_path,
            format!(r#"schema_version = 1
[[apps]]
identity = "cargo:tool-package"
name = "tool"
category = "Tools"
disposition = "archived"
installed = true
installed_paths = [{target:?}]
[apps.provenance]
source = "cargo"
installer = "cargo-install"
package = "tool-package"
registry = "crates-io"
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
        let plan = removal_plan(&ledger, 0)?.context("expected removal authority")?;
        assert!(plan.summary().contains("tool-package"));
        fs::write(&target, "externally changed")?;
        assert!(removal_plan(&ledger, 0).is_err());
        Ok(())
    }

    fn fixture() -> Result<(tempfile::TempDir, Ledger, Plan)> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("cargo");
        fs::create_dir_all(root.join("bin"))?;
        fs::write(root.join("bin/tool"), b"old binary")?;
        fs::write(root.join(".crates.toml"), b"old cargo receipt")?;
        fs::write(root.join(".crates2.json"), b"old cargo receipt 2")?;
        let ledger_path = dir.path().join("ledger.toml");
        fs::write(
            &ledger_path,
            format!(
                r#"schema_version = 1
[[apps]]
identity = "name:tool"
name = "tool"
category = "Tools"
disposition = "considering"
installed = true
version = "1.0.0"
[apps.provenance]
source = "cargo"
installer = "cargo-install"
package = "tool-package"
managed_by_apptrack = false
[apps.recipe]
source = "cargo"
installer = "cargo-install"
package = "tool-package"
registry = "crates-io"
root = {root:?}
bins = ["tool"]
"#,
                root = root.to_string_lossy()
            ),
        )?;
        let ledger = Ledger::open(&ledger_path)?;
        let plan = make_plan(
            &ledger.apps[0],
            0,
            "2.0.0".into(),
            Some(("1.0.0".into(), vec!["tool".into()])),
        )?;
        Ok((dir, ledger, plan))
    }

    #[test]
    fn exact_install_is_verified_and_records_each_managed_binary_hash() -> Result<()> {
        let (_dir, mut ledger, plan) = fixture()?;
        let inspections = Cell::new(0);
        apply_with(
            &mut ledger,
            &plan,
            &AtomicBool::new(false),
            |_| {},
            |plan, _| {
                fs::write(&plan.targets[0], b"new binary")?;
                fs::write(plan.root.join(".crates.toml"), b"new cargo receipt")?;
                fs::write(plan.root.join(".crates2.json"), b"new cargo receipt 2")?;
                Ok(())
            },
            |_, _| {
                let call = inspections.get();
                inspections.set(call + 1);
                Ok(Some(if call == 0 {
                    ("1.0.0".into(), vec!["tool".into()])
                } else {
                    ("2.0.0".into(), vec!["tool".into()])
                }))
            },
        )?;
        let reopened = Ledger::open(&ledger.path)?;
        assert_eq!(reopened.apps[0].version.as_deref(), Some("2.0.0"));
        assert!(reopened.apps[0].provenance.managed_by_apptrack);
        assert_eq!(
            reopened.apps[0].provenance.bin_sha256.get("tool"),
            Some(&sha256(&plan.targets[0])?)
        );
        assert!(reopened.record(0).contains("registry = \"crates-io\""));
        assert!(reopened.record(0).contains("[[apps.provenance_history]]"));
        Ok(())
    }

    #[test]
    fn imported_launch_moves_to_cargo_bin_only_after_receipt() -> Result<()> {
        let (dir, ledger, _) = fixture()?;
        let old = dir.path().join("local/tool");
        let path = ledger.path.clone();
        let text = fs::read_to_string(&path)?
            .replace("version = \"1.0.0\"", &format!("version = \"1.0.0\"\ninstalled_paths = [{old:?}]"));
        fs::write(&path, format!("{text}\n[apps.launch]\nprogram = {old:?}\nargs = []\ngui = false\n"))?;
        let ledger = Ledger::open(&path)?;
        let plan = make_plan(&ledger.apps[0], 0, "2.0.0".into(), None)?;
        let mut hashes = BTreeMap::new();
        hashes.insert("tool".to_string(), "verified hash".to_string());
        let receipt = receipt_document(&ledger, &plan, &hashes)?;
        assert_eq!(receipt["apps"][0]["launch"]["program"].as_str(), Some(plan.targets[0].to_str().unwrap()));
        assert_eq!(ledger.apps[0].launch.as_ref().unwrap().program, old.to_string_lossy());
        Ok(())
    }

    #[test]
    fn saved_cargo_reroute_moves_old_recipe_launch_but_preserves_custom_wrapper() -> Result<()> {
        let (dir, ledger, _) = fixture()?;
        let old = dir.path().join("old-bin/tool");
        let path = ledger.path.clone();
        let text = fs::read_to_string(&path)?
            .replace("bins = [\"tool\"]", &format!("bins = [\"tool\"]\nlaunch_from = {old:?}"));
        fs::write(&path, format!("{text}\n[apps.launch]\nprogram = {old:?}\nargs = []\ngui = false\n"))?;
        let ledger = Ledger::open(&path)?;
        let plan = make_plan(&ledger.apps[0], 0, "2.0.0".into(), None)?;
        let receipt = receipt_document(&ledger, &plan, &BTreeMap::new())?;
        assert_eq!(receipt["apps"][0]["launch"]["program"].as_str(), plan.targets[0].to_str());
        assert!(receipt["apps"][0]["recipe"].get("launch_from").is_none());
        let custom = fs::read_to_string(&path)?
            .replace(&format!("program = {old:?}"), "program = \"/tmp/custom-wrapper\"");
        fs::write(&path, custom)?;
        let ledger = Ledger::open(&path)?;
        let plan = make_plan(&ledger.apps[0], 0, "2.0.0".into(), None)?;
        let receipt = receipt_document(&ledger, &plan, &BTreeMap::new())?;
        assert_eq!(receipt["apps"][0]["launch"]["program"].as_str(), Some("/tmp/custom-wrapper"));
        Ok(())
    }

    #[test]
    fn failed_post_install_verification_restores_bins_and_cargo_metadata() -> Result<()> {
        let (_dir, mut ledger, plan) = fixture()?;
        let before_ledger = fs::read(&ledger.path)?;
        let inspections = Cell::new(0);
        let error = apply_with(
            &mut ledger,
            &plan,
            &AtomicBool::new(false),
            |_| {},
            |plan, _| {
                fs::write(&plan.targets[0], b"broken replacement")?;
                fs::write(plan.root.join(".crates.toml"), b"broken metadata")?;
                fs::write(plan.root.join(".crates2.json"), b"broken metadata 2")?;
                Ok(())
            },
            |_, _| {
                let call = inspections.get();
                inspections.set(call + 1);
                Ok(Some(if call == 0 {
                    ("1.0.0".into(), vec!["tool".into()])
                } else {
                    ("wrong".into(), vec!["tool".into()])
                }))
            },
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("previous binaries and Cargo receipt restored"));
        assert_eq!(fs::read(&plan.targets[0])?, b"old binary");
        assert_eq!(fs::read(plan.root.join(".crates.toml"))?, b"old cargo receipt");
        assert_eq!(fs::read(plan.root.join(".crates2.json"))?, b"old cargo receipt 2");
        assert_eq!(fs::read(&ledger.path)?, before_ledger);
        Ok(())
    }
}
