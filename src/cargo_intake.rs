//! Review a GitHub source against published Cargo metadata before saving a recipe.
use crate::{dialog::Dialog, ledger::{Launch, Ledger}, update::Recipe};
use anyhow::{Context, Result, ensure};
use std::process::Command;

#[derive(Clone)]
struct Candidate {
    package: String,
    bins: Vec<String>,
    main: bool,
    dir: String,
}

fn gh_raw(repo: &str, path: &str, dialog: &dyn Dialog) -> Result<Option<String>> {
    let output = crate::work::output(
        Command::new("gh")
            .args(["api", "-H", "Accept: application/vnd.github.raw+json", &format!("repos/{repo}/contents/{path}")])
            .env("GH_PROMPT_DISABLED", "1"),
        true,
        Some(dialog.cancel_flag()),
    )?;
    if !output.status.success() { return Ok(None); }
    Ok(Some(String::from_utf8(output.stdout)?))
}

fn manifest(repo: &str, dir: &str, dialog: &dyn Dialog) -> Result<Option<Candidate>> {
    let path = if dir.is_empty() { "Cargo.toml".to_string() } else { format!("{dir}/Cargo.toml") };
    let Some(raw) = gh_raw(repo, &path, dialog)? else { return Ok(None); };
    let parsed: toml::Value = toml::from_str(&raw).context("Source Cargo.toml is not valid TOML")?;
    let Some(package) = parsed.get("package").and_then(|v| v.get("name")).and_then(toml::Value::as_str) else { return Ok(None); };
    ensure!(crate::update::safe_name(package), "Cargo package name is not an exact name");
    let bins = parsed.get("bin").and_then(toml::Value::as_array)
        .into_iter().flatten()
        .filter_map(|bin| bin.get("name").and_then(toml::Value::as_str))
        .map(str::to_owned).collect();
    let main = gh_raw(repo, &if dir.is_empty() { "src/main.rs".to_string() } else { format!("{dir}/src/main.rs") }, dialog)?.is_some();
    Ok(Some(Candidate { package: package.into(), bins, main, dir: dir.into() }))
}

fn candidates(repo: &str, dialog: &dyn Dialog) -> Result<Vec<Candidate>> {
    let Some(raw) = gh_raw(repo, "Cargo.toml", dialog)? else { return Ok(vec![]); };
    let root: toml::Value = toml::from_str(&raw).context("Source Cargo.toml is not valid TOML")?;
    let mut found = vec![];
    if let Some(item) = manifest(repo, "", dialog)? { found.push(item); }
    if let Some(members) = root.get("workspace").and_then(|v| v.get("members")).and_then(toml::Value::as_array) {
        ensure!(members.len() <= 16, "Workspace has too many members for automatic Cargo review");
        for member in members {
            let Some(dir) = member.as_str() else { continue; };
            if dir.split('/').all(crate::update::safe_name) && !dir.contains('*') {
                if let Some(item) = manifest(repo, dir, dialog)? { found.push(item); }
            }
        }
    }
    Ok(found)
}

fn cargo_info(package: &str, repo: &str, dialog: &dyn Dialog) -> Result<String> {
    let output = crate::work::output(
        Command::new("cargo").args(["info", package, "--registry", "crates-io", "--color", "never"]),
        true,
        Some(dialog.cancel_flag()),
    )?;
    ensure!(output.status.success(), "Cargo registry lookup failed: {}", String::from_utf8_lossy(&output.stderr).trim());
    let info = String::from_utf8(output.stdout)?;
    let repository = info.lines().find_map(|line| line.strip_prefix("repository: ")).context("Cargo registry did not report a repository")?;
    let source = format!("https://github.com/{repo}");
    ensure!(crate::ledger::normalize_identity(repository) == crate::ledger::normalize_identity(&source),
        "Cargo package {package} belongs to {repository}, not {source}");
    ensure!(!info.to_ascii_lowercase().contains("placeholder") && !info.to_ascii_lowercase().contains("obsolete"),
        "Cargo package {package} is marked as an obsolete placeholder");
    let version = info.lines().find_map(|line| line.strip_prefix("version: "))
        .and_then(|line| line.split_whitespace().next()).context("Cargo registry did not report a version")?;
    ensure!(!version.contains('-'), "Cargo currently selects prerelease {version}; no stable Cargo recipe saved");
    Ok(version.into())
}

pub fn configure(ledger: &mut Ledger, index: usize, dialog: &dyn Dialog) -> Result<String> {
    let app = ledger.apps.get(index).context("Application no longer exists")?.clone();
    ensure!(app.recipe.is_none(), "{} already has an install recipe", app.name);
    ensure!(app.disposition != crate::ledger::Disposition::Archived, "Archived app must move to Considering first");
    let repo = app.identity.strip_prefix("https://github.com/").context("Cargo source review needs a GitHub repository identity")?;
    dialog.message(format!("Checking Cargo manifests in {repo}…"));
    let options = candidates(repo, dialog)?;
    ensure!(!options.is_empty(), "No exact Cargo package manifest found at this repository");
    let labels: Vec<_> = options.iter().map(|item| format!("{} ({})", item.package, if item.dir.is_empty() { "root" } else { &item.dir })).collect();
    let Some(choice) = dialog.choose("Cargo package", &labels, (options.len() == 1).then_some(0))? else { return Ok("Cargo source review cancelled.".into()); };
    let selected = &options[choice];
    let version = cargo_info(&selected.package, repo, dialog)?;
    let mut bins = selected.bins.clone();
    if bins.is_empty() && selected.main { bins.push(selected.package.clone()); }
    ensure!(!bins.is_empty(), "Cargo source has no verified executable target");
    ensure!(bins.iter().all(|bin| crate::update::safe_name(bin)), "Cargo binary name is not exact");
    let Some(bin_choice) = dialog.choose("Executable", &bins, (bins.len() == 1).then_some(0))? else { return Ok("Cargo source review cancelled.".into()); };
    let bin = bins[bin_choice].clone();
    let root = std::env::var("CARGO_INSTALL_ROOT").unwrap_or_else(|_| "~/.cargo".into());
    let target = format!("{root}/bin/{bin}");
    ensure!(!ledger.apps.iter().enumerate().any(|(i, other)| i != index &&
        (other.installed_paths.iter().any(|path| crate::ledger::expand_path(path) == crate::ledger::expand_path(&target))
            || other.recipe.as_ref().is_some_and(|r| r.source == "cargo" && r.root.as_deref() == Some(&root) && r.bins.contains(&bin)))),
        "Another app already owns the Cargo command {target}");
    let mut reviewed = app.clone();
    reviewed.recipe = Some(Recipe { source: "cargo".into(), installer: "cargo-install".into(),
        package: Some(selected.package.clone()), registry: Some("crates-io".into()), root: Some(root), bins: vec![bin.clone()], ..Recipe::default() });
    if reviewed.launch.is_none() {
        let directory = selected.dir.rsplit('/').next().unwrap_or("");
        let gui_default = if directory == "cli" { false } else if directory == "gui" { true }
            else { app.tags.iter().any(|tag| tag == "gui") };
        let Some(mode) = dialog.choose("Launch mode", &["Terminal".into(), "GUI".into()], Some(usize::from(gui_default)))?
        else { return Ok("Cargo source review cancelled; nothing saved.".into()); };
        reviewed.launch = Some(Launch { program: target.clone(), args: vec![], gui: mode == 1 });
    } else {
        let launch = reviewed.launch.as_ref().unwrap();
        ensure!(crate::ledger::expand_path(&launch.program) == crate::ledger::expand_path(&target)
            || app.installed_paths.iter().any(|path| crate::ledger::expand_path(path) == crate::ledger::expand_path(&launch.program)),
            "Existing launch is a custom wrapper; review migration in TOML");
    }
    dialog.message(format!("Review Cargo install\n{} {version}\nregistry crates-io · package {}\nbinary {bin} → {target}\nNo installation yet.", app.name, selected.package));
    match dialog.choose("Save or install", &["Save recipe only".into(), "Save and install".into(), "Cancel".into()], None)? {
        Some(0) => { ledger.register_recipe(&reviewed)?; Ok(format!("{} Cargo recipe saved; nothing installed.", app.name)) }
        Some(1) => {
            ledger.register_recipe(&reviewed)?;
            crate::dialog::install_one(ledger, index, dialog)
        }
        _ => Ok("Cargo source review cancelled; nothing saved.".into()),
    }
}
