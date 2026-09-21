use crate::{
    ledger::{App, Disposition, Ledger},
    update::timestamp,
};
use anyhow::{Context, Result, bail, ensure};
use std::{process::Command, sync::atomic::AtomicBool};
use toml_edit::{Array, Item, Table, value};

#[derive(Clone, Debug)]
pub struct Plan {
    pub index: usize,
    pub name: String,
    pub identity: String,
    pub app_id: String,
    pub remote: String,
    pub remote_url: String,
    pub installation: String,
    pub arch: String,
    pub branch: String,
    pub ref_name: String,
    pub old_version: Option<String>,
    pub installed_version: Option<String>,
    pub installed_commit: Option<String>,
    pub installed_origin: Option<String>,
    pub release: String,
    pub commit: String,
    pub up_to_date: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemovalPlan {
    pub index: usize,
    pub name: String,
    identity: String,
    app_id: String,
    remote: String,
    remote_url: String,
    installation: String,
    arch: String,
    branch: String,
    ref_name: String,
    version: String,
    commit: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Installed {
    origin: String,
    version: Option<String>,
    commit: String,
}

struct ExactRecipe<'a> {
    app_id: &'a str,
    remote: &'a str,
    remote_url: &'a str,
    installation: &'a str,
    arch: &'a str,
    branch: &'a str,
}

fn safe_component(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn exact_recipe(app: &App) -> Result<ExactRecipe<'_>> {
    let recipe = app
        .recipe
        .as_ref()
        .context("No update recipe recorded for this app")?;
    ensure!(
        recipe.source == "flatpak" && recipe.installer == "flatpak",
        "Expected a flatpak / flatpak recipe"
    );
    let app_id = recipe
        .package
        .as_deref()
        .context("Flatpak recipe requires package (the application ID)")?;
    let remote = recipe
        .remote
        .as_deref()
        .context("Flatpak recipe requires remote")?;
    let remote_url = recipe
        .remote_url
        .as_deref()
        .context("Flatpak recipe requires remote_url")?;
    let installation = recipe
        .installation
        .as_deref()
        .context("Flatpak recipe requires installation")?;
    let arch = if recipe.arch.is_empty() {
        bail!("Flatpak recipe requires arch")
    } else {
        recipe.arch.as_str()
    };
    let branch = recipe
        .branch
        .as_deref()
        .context("Flatpak recipe requires branch")?;
    ensure!(safe_component(app_id) && app_id.contains('.'), "Invalid Flatpak application ID");
    ensure!(safe_component(remote), "Invalid Flatpak remote name");
    ensure!(
        remote_url.starts_with("https://") && remote_url.ends_with('/'),
        "Flatpak remote_url must be one exact HTTPS repository URL ending in '/'"
    );
    ensure!(matches!(installation, "user" | "system"), "Flatpak installation must be user or system");
    ensure!(safe_component(arch), "Invalid Flatpak architecture");
    ensure!(safe_component(branch), "Invalid Flatpak branch");
    Ok(ExactRecipe {
        app_id,
        remote,
        remote_url,
        installation,
        arch,
        branch,
    })
}

fn scope_arg(installation: &str) -> &'static str {
    if installation == "user" { "--user" } else { "--system" }
}

fn command_output(command: &mut Command, cancel: &AtomicBool, what: &str) -> Result<String> {
    let output = crate::work::output(command, true, Some(cancel))
        .with_context(|| format!("Cannot run {what}"))?;
    ensure!(
        output.status.success(),
        "{what} failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    String::from_utf8(output.stdout).with_context(|| format!("{what} returned non-UTF-8 output"))
}

fn configured_remote_url(
    installation: &str,
    remote: &str,
    cancel: &AtomicBool,
) -> Result<String> {
    let output = command_output(
        Command::new("flatpak").args([
            "remotes",
            scope_arg(installation),
            "--columns=name,url:full",
        ]),
        cancel,
        "flatpak remotes",
    )?;
    let matches: Vec<_> = output
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .filter(|(name, _)| *name == remote)
        .map(|(_, url)| url.to_string())
        .collect();
    ensure!(matches.len() == 1, "Flatpak remote {remote} is not configured exactly once for {installation}");
    Ok(matches[0].clone())
}

fn remote_value(
    recipe: &ExactRecipe<'_>,
    flag: &str,
    cancel: &AtomicBool,
) -> Result<String> {
    let selector = format!("{}//{}", recipe.app_id, recipe.branch);
    let arch = format!("--arch={}", recipe.arch);
    let output = command_output(
        Command::new("flatpak").args([
            "remote-info",
            scope_arg(recipe.installation),
            "--app",
            &arch,
            flag,
            recipe.remote,
            &selector,
        ]),
        cancel,
        "flatpak remote-info",
    )?;
    let lines: Vec<_> = output.lines().map(str::trim).filter(|line| !line.is_empty()).collect();
    ensure!(lines.len() == 1, "flatpak remote-info did not return exactly one value");
    Ok(lines[0].to_string())
}

fn remote_version(recipe: &ExactRecipe<'_>, cancel: &AtomicBool) -> Result<Option<String>> {
    let arch = format!("--arch={}", recipe.arch);
    let output = command_output(
        Command::new("flatpak").args([
            "remote-ls",
            scope_arg(recipe.installation),
            "--app",
            &arch,
            "--columns=application,arch,branch,version",
            recipe.remote,
        ]),
        cancel,
        "flatpak remote-ls",
    )?;
    parse_remote_version(&output, recipe.app_id, recipe.arch, recipe.branch)
}

fn parse_remote_version(
    output: &str,
    app_id: &str,
    arch: &str,
    branch: &str,
) -> Result<Option<String>> {
    let rows: Vec<_> = output
        .lines()
        .filter_map(|line| {
            let columns: Vec<_> = line.split('\t').collect();
            (matches!(columns.len(), 3 | 4)
                && columns[0] == app_id
                && columns[1] == arch
                && columns[2] == branch)
                .then(|| columns.get(3).map(|version| version.trim().to_string()))
        })
        .collect();
    ensure!(rows.len() <= 1, "Flatpak remote reported duplicate human versions for the exact application ref");
    let version = rows.into_iter().next().flatten();
    Ok(version.filter(|version| !version.is_empty() && version != "-"))
}

fn installed(recipe: &ExactRecipe<'_>, cancel: &AtomicBool) -> Result<Option<Installed>> {
    let output = command_output(
        Command::new("flatpak").args([
            "list",
            scope_arg(recipe.installation),
            "--app",
            "--columns=application,arch,branch,origin,version",
        ]),
        cancel,
        "flatpak list",
    )?;
    let rows: Vec<_> = output
        .lines()
        .filter_map(|line| {
            let columns: Vec<_> = line.split('\t').collect();
            (columns.len() == 5
                && columns[0] == recipe.app_id
                && columns[1] == recipe.arch
                && columns[2] == recipe.branch)
                .then(|| {
                    let version = columns[4].trim();
                    (
                        columns[3].to_string(),
                        (!version.is_empty() && version != "-").then(|| version.to_string()),
                    )
                })
        })
        .collect();
    ensure!(rows.len() <= 1, "Flatpak installation contains duplicate exact application refs");
    let Some((origin, version)) = rows.into_iter().next() else {
        return Ok(None);
    };
    let ref_name = format!("app/{}/{}/{}", recipe.app_id, recipe.arch, recipe.branch);
    let commit = command_output(
        Command::new("flatpak").args([
            "info",
            scope_arg(recipe.installation),
            "--show-commit",
            &ref_name,
        ]),
        cancel,
        "flatpak info --show-commit",
    )?;
    let commit = commit.trim().to_string();
    ensure!(valid_commit(&commit), "Flatpak reported an invalid installed commit");
    Ok(Some(Installed { origin, version, commit }))
}

fn valid_commit(commit: &str) -> bool {
    commit.len() == 64 && commit.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub fn check(ledger: &Ledger, index: usize, cancel: &AtomicBool) -> Result<Plan> {
    ledger.check_unchanged()?;
    let app = &ledger.apps[index];
    ensure!(
        app.disposition != Disposition::Archived,
        "Archived applications are excluded from updates; move it to Considering first"
    );
    let recipe = exact_recipe(app)?;
    let configured_url = configured_remote_url(recipe.installation, recipe.remote, cancel)?;
    ensure!(
        configured_url == recipe.remote_url,
        "Flatpak remote {} points to {}, recipe requires {}",
        recipe.remote,
        configured_url,
        recipe.remote_url
    );
    let expected_ref = format!("app/{}/{}/{}", recipe.app_id, recipe.arch, recipe.branch);
    let remote_ref = remote_value(&recipe, "--show-ref", cancel)?;
    ensure!(remote_ref == expected_ref, "Flatpak remote returned {remote_ref}, expected {expected_ref}");
    let commit = remote_value(&recipe, "--show-commit", cancel)?;
    ensure!(valid_commit(&commit), "Flatpak remote reported an invalid commit");
    let release = remote_version(&recipe, cancel)?.unwrap_or_else(|| "unreported".into());
    let observed = installed(&recipe, cancel)?;
    let installed_version = observed.as_ref().and_then(|found| found.version.clone());
    let installed_commit = observed.as_ref().map(|found| found.commit.clone());
    let installed_origin = observed.as_ref().map(|found| found.origin.clone());
    if let Some(origin) = &installed_origin {
        ensure!(origin == recipe.remote, "Installed Flatpak origin is {origin}, recipe requires {}", recipe.remote);
    }
    let up_to_date = app.provenance.managed_by_apptrack
        && app.provenance.source == "flatpak"
        && app.provenance.installer.as_deref() == Some("flatpak")
        && app.provenance.package.as_deref() == Some(recipe.app_id)
        && app.provenance.remote.as_deref() == Some(recipe.remote)
        && app.provenance.remote_url.as_deref() == Some(recipe.remote_url)
        && app.provenance.installation.as_deref() == Some(recipe.installation)
        && app.provenance.arch.as_deref() == Some(recipe.arch)
        && app.provenance.branch.as_deref() == Some(recipe.branch)
        && app.provenance.release.as_deref() == Some(&release)
        && app.provenance.commit.as_deref() == Some(&commit)
        && installed_commit.as_deref() == Some(&commit);
    let plan = Plan {
        index,
        name: app.name.clone(),
        identity: app.identity.clone(),
        app_id: recipe.app_id.into(),
        remote: recipe.remote.into(),
        remote_url: recipe.remote_url.into(),
        installation: recipe.installation.into(),
        arch: recipe.arch.into(),
        branch: recipe.branch.into(),
        ref_name: expected_ref,
        old_version: app.version.clone(),
        installed_version,
        installed_commit,
        installed_origin,
        release,
        commit,
        up_to_date,
    };
    ledger.check_unchanged()?;
    Ok(plan)
}

fn short(commit: &str) -> &str {
    commit.get(..12).unwrap_or(commit)
}

impl Plan {
    pub fn collision_key(&self) -> String {
        format!("flatpak:{}:{}", self.installation, self.ref_name)
    }

    pub fn summary(&self) -> String {
        format!(
            "{}\n{} → {}\nCommit {} → {}\n\nSource\n{} · {}\n\nApplication\n{}\n{} installation\n\nVerification\nHuman version plus exact remote URL, application ref, origin, and installed OSTree commit.\n\n{}",
            self.name,
            self.installed_version.as_deref().or(self.old_version.as_deref()).unwrap_or("not installed"),
            self.release,
            self.installed_commit.as_deref().map(short).unwrap_or("not installed"),
            short(&self.commit),
            self.remote,
            self.remote_url,
            self.ref_name,
            self.installation,
            if self.up_to_date {
                "Installed application matches the current AppTrack receipt."
            } else if self.installed_commit.is_some() {
                "Update the selected Flatpak application. Record its exact commit receipt."
            } else {
                "Install the selected Flatpak application. Record its exact commit receipt."
            }
        )
    }
}

impl RemovalPlan {
    pub fn summary(&self) -> String {
        format!(
            "Remove managed Flatpak installation?\n\n{} {}\n{}\n{} · {}\n{} installation\nCommit {}\n\nThe archive decision and reason are already saved. Keeping the installation is the default.",
            self.name,
            self.version,
            self.ref_name,
            self.remote,
            self.remote_url,
            self.installation,
            short(&self.commit),
        )
    }
}

pub fn removal_plan(ledger: &Ledger, index: usize) -> Result<Option<RemovalPlan>> {
    ledger.check_unchanged()?;
    let app = ledger.apps.get(index).context("Application no longer exists")?;
    if !app.provenance.managed_by_apptrack || app.provenance.source != "flatpak" {
        return Ok(None);
    }
    ensure!(app.provenance.installer.as_deref() == Some("flatpak"), "Managed Flatpak receipt has the wrong installer");
    ensure!(app.installed == Some(true), "Managed Flatpak receipt does not say the application is installed");
    let app_id = app.provenance.package.as_deref().context("Managed Flatpak receipt has no application ID")?;
    let remote = app.provenance.remote.as_deref().context("Managed Flatpak receipt has no remote")?;
    let remote_url = app.provenance.remote_url.as_deref().context("Managed Flatpak receipt has no remote URL")?;
    let installation = app.provenance.installation.as_deref().context("Managed Flatpak receipt has no installation scope")?;
    let arch = app.provenance.arch.as_deref().context("Managed Flatpak receipt has no architecture")?;
    let branch = app.provenance.branch.as_deref().context("Managed Flatpak receipt has no branch")?;
    let commit = app.provenance.commit.as_deref().context("Managed Flatpak receipt has no commit")?;
    let version = app.provenance.release.as_deref().or(app.version.as_deref()).unwrap_or("unreported");
    ensure!(safe_component(app_id) && app_id.contains('.'), "Managed Flatpak receipt has an invalid application ID");
    ensure!(safe_component(remote), "Managed Flatpak receipt has an invalid remote");
    ensure!(remote_url.starts_with("https://") && remote_url.ends_with('/'), "Managed Flatpak receipt has an invalid remote URL");
    ensure!(matches!(installation, "user" | "system"), "Managed Flatpak receipt has an invalid installation scope");
    ensure!(safe_component(arch) && safe_component(branch), "Managed Flatpak receipt has an invalid ref");
    ensure!(valid_commit(commit), "Managed Flatpak receipt has an invalid commit");
    Ok(Some(RemovalPlan {
        index,
        name: app.name.clone(),
        identity: app.identity.clone(),
        app_id: app_id.into(),
        remote: remote.into(),
        remote_url: remote_url.into(),
        installation: installation.into(),
        arch: arch.into(),
        branch: branch.into(),
        ref_name: format!("app/{app_id}/{arch}/{branch}"),
        version: version.into(),
        commit: commit.into(),
    }))
}

fn removal_recipe(plan: &RemovalPlan) -> ExactRecipe<'_> {
    ExactRecipe {
        app_id: &plan.app_id,
        remote: &plan.remote,
        remote_url: &plan.remote_url,
        installation: &plan.installation,
        arch: &plan.arch,
        branch: &plan.branch,
    }
}

fn plan_recipe(ledger: &Ledger, plan: &Plan) -> Result<()> {
    ensure!(
        ledger.apps.get(plan.index).is_some_and(|app| app.identity == plan.identity),
        "Application changed since planning"
    );
    let recipe = exact_recipe(&ledger.apps[plan.index])?;
    ensure!(
        recipe.app_id == plan.app_id
            && recipe.remote == plan.remote
            && recipe.remote_url == plan.remote_url
            && recipe.installation == plan.installation
            && recipe.arch == plan.arch
            && recipe.branch == plan.branch,
        "Flatpak recipe changed since planning"
    );
    Ok(())
}

fn planned_recipe(plan: &Plan) -> ExactRecipe<'_> {
    ExactRecipe {
        app_id: &plan.app_id,
        remote: &plan.remote,
        remote_url: &plan.remote_url,
        installation: &plan.installation,
        arch: &plan.arch,
        branch: &plan.branch,
    }
}

fn run_change(command: &mut Command, cancel: Option<&AtomicBool>, what: &str) -> Result<()> {
    let output = crate::work::output_long_running(command, true, cancel)
        .with_context(|| format!("Cannot run {what}"))?;
    ensure!(
        output.status.success(),
        "{what} failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

fn rollback(plan: &Plan) -> Result<()> {
    if let Some(commit) = &plan.installed_commit {
        run_change(
            Command::new("flatpak").args([
                "update",
                scope_arg(&plan.installation),
                "--app",
                &format!("--arch={}", plan.arch),
                "--noninteractive",
                "--assumeyes",
                &format!("--commit={commit}"),
                &plan.ref_name,
            ]),
            None,
            "Flatpak rollback update",
        )
    } else {
        run_change(
            Command::new("flatpak").args([
                "uninstall",
                scope_arg(&plan.installation),
                "--app",
                &format!("--arch={}", plan.arch),
                "--noninteractive",
                "--assumeyes",
                &plan.ref_name,
            ]),
            None,
            "Flatpak rollback uninstall",
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
    plan_recipe(ledger, plan)?;
    let recipe = planned_recipe(plan);
    let _lock = ledger.write_lock()?;
    ensure!(
        installed(&recipe, cancel)?.as_ref().map(|found| (&found.origin, &found.commit))
            == plan.installed_origin.as_ref().zip(plan.installed_commit.as_ref()),
        "Flatpak installation changed since planning; check again"
    );
    progress(if plan.installed_commit.is_some() {
        "Updating the exact Flatpak application ref and commit…"
    } else {
        "Installing the exact Flatpak application ref…"
    });
    let changed = if plan.installed_commit.is_some() {
        run_change(
            Command::new("flatpak").args([
                "update",
                scope_arg(&plan.installation),
                "--app",
                &format!("--arch={}", plan.arch),
                "--noninteractive",
                "--assumeyes",
                &format!("--commit={}", plan.commit),
                &plan.ref_name,
            ]),
            Some(cancel),
            "flatpak update",
        )
    } else {
        run_change(
            Command::new("flatpak").args([
                "install",
                scope_arg(&plan.installation),
                "--app",
                &format!("--arch={}", plan.arch),
                "--noninteractive",
                "--assumeyes",
                &plan.remote,
                &plan.ref_name,
            ]),
            Some(cancel),
            "flatpak install",
        )
    };
    if let Err(error) = changed {
        let current = installed(&recipe, &AtomicBool::new(false))
            .context("Flatpak operation failed and its resulting state could not be inspected")?;
        let mutated = current.as_ref().map(|found| &found.commit) != plan.installed_commit.as_ref();
        if mutated {
            rollback(plan).context("Flatpak operation failed and rollback also failed")?;
            return Err(error.context("Flatpak operation failed; previous installation restored"));
        }
        return Err(error);
    }
    let result = (|| -> Result<()> {
        progress("Verifying Flatpak origin, ref, and installed commit…");
        let observed = installed(&recipe, &AtomicBool::new(false))?
            .context("Flatpak reported success but the application is not installed")?;
        ensure!(observed.origin == plan.remote, "Installed Flatpak origin changed to {}", observed.origin);
        ensure!(observed.commit == plan.commit, "Flatpak installed commit {}, expected {}", observed.commit, plan.commit);
        ensure!(observed.version.as_deref().unwrap_or("unreported") == plan.release, "Flatpak installed version {:?}, expected {}", observed.version, plan.release);
        progress("Saving the Flatpak installation receipt…");
        ledger.save_locked(receipt_document(ledger, plan)?)?;
        Ok(())
    })();
    if let Err(error) = result {
        if let Err(rollback_error) = rollback(plan) {
            return Err(error.context(format!("Flatpak verification or receipt failed and rollback also failed: {rollback_error:#}")));
        }
        return Err(error.context("Flatpak verification or receipt failed; previous installation restored"));
    }
    progress("Installed; Flatpak state and AppTrack receipt verified.");
    Ok(())
}

fn restore_removed(plan: &RemovalPlan) -> Result<()> {
    run_change(
        Command::new("flatpak").args([
            "install",
            scope_arg(&plan.installation),
            "--app",
            &format!("--arch={}", plan.arch),
            "--noninteractive",
            "--assumeyes",
            &plan.remote,
            &plan.ref_name,
        ]),
        None,
        "Flatpak removal rollback install",
    )?;
    run_change(
        Command::new("flatpak").args([
            "update",
            scope_arg(&plan.installation),
            "--app",
            &format!("--arch={}", plan.arch),
            "--noninteractive",
            "--assumeyes",
            &format!("--commit={}", plan.commit),
            &plan.ref_name,
        ]),
        None,
        "Flatpak removal rollback commit",
    )?;
    let observed = installed(&removal_recipe(plan), &AtomicBool::new(false))?
        .context("Flatpak removal rollback did not restore the application")?;
    ensure!(observed.origin == plan.remote && observed.commit == plan.commit, "Flatpak removal rollback restored the wrong origin or commit");
    Ok(())
}

pub fn remove(
    ledger: &mut Ledger,
    plan: &RemovalPlan,
    cancel: &AtomicBool,
    progress: impl Fn(&str),
) -> Result<()> {
    ledger.check_unchanged()?;
    ensure!(
        removal_plan(ledger, plan.index)?.as_ref() == Some(plan),
        "Flatpak removal authority changed; review the archived record again"
    );
    let recipe = removal_recipe(plan);
    let _lock = ledger.write_lock()?;
    let before = installed(&recipe, cancel)?
        .context("Managed Flatpak application is no longer installed")?;
    ensure!(before.origin == plan.remote, "Installed Flatpak origin is {}, receipt requires {}", before.origin, plan.remote);
    ensure!(before.commit == plan.commit, "Installed Flatpak commit changed since receipt; refusing removal");
    progress("Removing the exact managed Flatpak application…");
    let operation = run_change(
        Command::new("flatpak").args([
            "uninstall",
            scope_arg(&plan.installation),
            "--app",
            &format!("--arch={}", plan.arch),
            "--noninteractive",
            "--assumeyes",
            &plan.ref_name,
        ]),
        Some(cancel),
        "flatpak uninstall",
    );
    if let Err(error) = operation {
        match installed(&recipe, &AtomicBool::new(false))? {
            Some(found) if found.origin == plan.remote && found.commit == plan.commit => {
                return Err(error);
            }
            None => {
                restore_removed(plan).context("Flatpak removal failed after mutation and rollback also failed")?;
                return Err(error.context("Flatpak removal failed after mutation; installation restored"));
            }
            Some(_) => bail!("Flatpak removal failed and left an unexpected installed origin or commit"),
        }
    }
    progress("Verifying the managed Flatpak ref is absent…");
    if installed(&recipe, &AtomicBool::new(false))?.is_some() {
        bail!("Flatpak reported successful removal but the exact application ref remains installed");
    }
    progress("Saving the removal receipt…");
    let saved = removal_receipt_document(ledger, plan)
        .and_then(|receipt| ledger.save_locked(receipt));
    if let Err(error) = saved {
        if let Err(rollback) = restore_removed(plan) {
            return Err(error.context(format!("Flatpak was removed, the receipt failed, and rollback also failed: {rollback:#}")));
        }
        return Err(error.context("Flatpak removal receipt failed; installation restored"));
    }
    progress("Removed; absence and AppTrack receipt verified.");
    Ok(())
}

fn removal_receipt_document(ledger: &Ledger, plan: &RemovalPlan) -> Result<toml_edit::DocumentMut> {
    let mut doc = ledger.document();
    let app = doc["apps"]
        .as_array_of_tables_mut()
        .context("apps must be an array of tables")?
        .get_mut(plan.index)
        .context("Application disappeared while saving removal receipt")?;
    app["installed"] = value(false);
    app["installed_paths"] = value(Array::new());
    let provenance = app["provenance"]
        .as_table_mut()
        .context("Managed Flatpak provenance disappeared while saving removal receipt")?;
    provenance["managed_by_apptrack"] = value(false);
    provenance["removed_at_unix"] = value(timestamp());
    let mut action = Table::new();
    action["action"] = value("remove");
    action["result"] = value("success");
    action["at_unix"] = value(timestamp());
    action["release"] = value(&plan.version);
    action["commit"] = value(&plan.commit);
    app["last_action"] = Item::Table(action);
    Ok(doc)
}

fn receipt_document(ledger: &Ledger, plan: &Plan) -> Result<toml_edit::DocumentMut> {
    let mut doc = ledger.document();
    let app = doc["apps"]
        .as_array_of_tables_mut()
        .context("apps must be an array of tables")?
        .get_mut(plan.index)
        .context("Application disappeared while saving receipt")?;
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
    provenance["source"] = value("flatpak");
    provenance["installer"] = value("flatpak");
    provenance["package"] = value(&plan.app_id);
    provenance["remote"] = value(&plan.remote);
    provenance["remote_url"] = value(&plan.remote_url);
    provenance["installation"] = value(&plan.installation);
    provenance["arch"] = value(&plan.arch);
    provenance["branch"] = value(&plan.branch);
    provenance["commit"] = value(&plan.commit);
    provenance["release"] = value(&plan.release);
    provenance["managed_by_apptrack"] = value(true);
    provenance["installed_at_unix"] = value(timestamp());
    app["provenance"] = Item::Table(provenance);
    app["version"] = value(&plan.release);
    app["installed"] = value(true);
    app["installed_paths"] = value(Array::new());
    let mut launch = Table::new();
    launch["program"] = value("flatpak");
    let mut args = Array::new();
    args.push("run");
    args.push(&plan.app_id);
    launch["args"] = value(args);
    launch["gui"] = value(true);
    app["launch"] = Item::Table(launch);
    let mut action = Table::new();
    action["action"] = value(if plan.installed_commit.is_some() { "update" } else { "install" });
    action["result"] = value("success");
    action["at_unix"] = value(timestamp());
    action["release"] = value(&plan.release);
    app["last_action"] = Item::Table(action);
    Ok(doc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn commit_validation_is_exact() {
        assert!(valid_commit(&"a".repeat(64)));
        assert!(!valid_commit(&"a".repeat(63)));
        assert!(!valid_commit(&format!("{}g", "a".repeat(63))));
    }

    #[test]
    fn remote_version_is_optional_appstream_metadata() -> Result<()> {
        assert_eq!(
            parse_remote_version(
                "io.github.suchnsuch.Tangent\tx86_64\tstable\t0.12.9\n",
                "io.github.suchnsuch.Tangent",
                "x86_64",
                "stable",
            )?,
            Some("0.12.9".into())
        );
        assert_eq!(
            parse_remote_version(
                "io.github.suchnsuch.Tangent\tx86_64\tstable\n",
                "io.github.suchnsuch.Tangent",
                "x86_64",
                "stable",
            )?,
            None
        );
        Ok(())
    }

    #[test]
    fn receipt_records_full_flatpak_authority_and_launch() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("ledger.toml");
        fs::write(
            &path,
            r#"schema_version = 1
[[apps]]
identity = "name:tangent"
name = "tangent"
category = "PKM"
disposition = "considering"
[apps.provenance]
source = "unknown"
managed_by_apptrack = false
[apps.recipe]
source = "flatpak"
installer = "flatpak"
package = "io.github.suchnsuch.Tangent"
remote = "flathub"
remote_url = "https://dl.flathub.org/repo/"
installation = "user"
arch = "x86_64"
branch = "stable"
"#,
        )?;
        let mut ledger = Ledger::open(&path)?;
        let commit = "a".repeat(64);
        let plan = Plan {
            index: 0,
            name: "tangent".into(),
            identity: "name:tangent".into(),
            app_id: "io.github.suchnsuch.Tangent".into(),
            remote: "flathub".into(),
            remote_url: "https://dl.flathub.org/repo/".into(),
            installation: "user".into(),
            arch: "x86_64".into(),
            branch: "stable".into(),
            ref_name: "app/io.github.suchnsuch.Tangent/x86_64/stable".into(),
            old_version: None,
            installed_version: None,
            installed_commit: None,
            installed_origin: None,
            release: "0.12.9".into(),
            commit: commit.clone(),
            up_to_date: false,
        };
        let receipt = receipt_document(&ledger, &plan)?;
        let _lock = ledger.write_lock()?;
        ledger.save_locked(receipt)?;
        let reopened = Ledger::open(&path)?;
        let app = &reopened.apps[0];
        assert_eq!(app.provenance.remote.as_deref(), Some("flathub"));
        assert_eq!(app.provenance.remote_url.as_deref(), Some("https://dl.flathub.org/repo/"));
        assert_eq!(app.provenance.installation.as_deref(), Some("user"));
        assert_eq!(app.provenance.arch.as_deref(), Some("x86_64"));
        assert_eq!(app.provenance.branch.as_deref(), Some("stable"));
        assert_eq!(app.provenance.commit.as_deref(), Some(commit.as_str()));
        assert_eq!(app.version.as_deref(), Some("0.12.9"));
        assert_eq!(app.launch.as_ref().unwrap().program, "flatpak");
        assert_eq!(app.launch.as_ref().unwrap().args, ["run", "io.github.suchnsuch.Tangent"]);
        assert!(reopened.record(0).contains("[[apps.provenance_history]]"));
        Ok(())
    }
}
