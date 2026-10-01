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
    if !output.status.success() {
        let reason = String::from_utf8_lossy(&output.stderr);
        if reason.contains("HTTP 404") || reason.contains("Not Found") { return Ok(None); }
        anyhow::bail!("Cannot check {path} in {repo}: {}", reason.trim());
    }
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

fn readme_install(repo: &str, dialog: &dyn Dialog) -> Result<String> {
    let readme = match gh_raw(repo, "README.md", dialog)? {
        Some(value) => Some(value),
        None => gh_raw(repo, "readme.md", dialog)?,
    };
    let readme = readme.context("No GitHub README found for Cargo install review")?;
    let line = readme.lines().find(|line| line.to_ascii_lowercase().contains("cargo install"))
        .context("README does not document cargo install")?;
    Ok(line.trim().chars().take(180).collect())
}

fn git_revision(repo: &str, dialog: &dyn Dialog) -> Result<String> {
    let output = crate::work::output(Command::new("gh")
        .args(["api", &format!("repos/{repo}/commits/HEAD"), "--jq", ".sha"])
        .env("GH_PROMPT_DISABLED", "1"), true, Some(dialog.cancel_flag()))?;
    ensure!(output.status.success(), "GitHub commit lookup failed: {}", String::from_utf8_lossy(&output.stderr).trim());
    let revision = String::from_utf8(output.stdout)?.trim().to_string();
    ensure!(revision.len() == 40 && revision.bytes().all(|b| b.is_ascii_hexdigit()), "GitHub returned an invalid commit SHA");
    Ok(revision)
}

fn dz_checkout(repo: &str) -> Result<Option<String>> {
    let name = repo.split('/').next_back().context("Missing repository name")?;
    ensure!(crate::update::safe_name(name), "Unsafe checkout name");
    let path = crate::ledger::expand_path(&format!("~/repos/dz/{name}"));
    if !path.join("Cargo.toml").is_file() { return Ok(None); }
    let output = Command::new("git").args(["-C", path.to_str().context("Checkout path is not UTF-8")?, "remote", "get-url", "origin"]).output()?;
    if !output.status.success() { return Ok(None); }
    let origin = String::from_utf8(output.stdout)?;
    let origin = origin.trim().replace("git@github.com:", "https://github.com/");
    let expected = format!("https://github.com/{repo}");
    if crate::ledger::normalize_identity(&origin) != crate::ledger::normalize_identity(&expected) { return Ok(None); }
    Ok(Some(path.to_string_lossy().into_owned()))
}

pub(crate) fn available(repo: &str, dialog: &dyn Dialog) -> Result<Vec<String>> {
    let options = candidates(repo, dialog)?;
    if options.is_empty() {
        anyhow::bail!("No exact Cargo package manifest found at this repository");
    }
    let mut found = Vec::new();
    let mut rejected = Vec::new();
    let readme = readme_install(repo, dialog);
    for option in options {
        if option.bins.is_empty() && !option.main {
            rejected.push(format!("{} has no executable target", option.package));
            continue;
        }
        match cargo_info(&option.package, repo, dialog) {
            Ok(version) => found.push(format!("{} {version}", option.package)),
            Err(error) => rejected.push(format!("{}: {error:#}", option.package)),
        }
        if dialog.cancelled() {
            anyhow::bail!("Cargo source check cancelled");
        }
    }
    if let Ok(line) = &readme {
        found.push(format!("Git install documented: {line}"));
        if dz_checkout(repo)?.is_some() { found.push("Local ~/repos/dz checkout available".into()); }
    } else if let Err(error) = readme { rejected.push(format!("Git install: {error:#}")); }
    if found.is_empty() {
        anyhow::bail!("{}", rejected.join("; "));
    }
    Ok(found)
}

pub fn configure(ledger: &mut Ledger, index: usize, dialog: &dyn Dialog) -> Result<String> {
    let app = ledger.apps.get(index).context("Application no longer exists")?.clone();
    ensure!(app.disposition != crate::ledger::Disposition::Archived, "Archived app must move to Considering first");
    let repo = app.identity.strip_prefix("https://github.com/").context("Cargo source review needs a GitHub repository identity")?;
    dialog.message(format!("Checking Cargo manifests in {repo}…"));
    let options = candidates(repo, dialog)?;
    ensure!(!options.is_empty(), "No exact Cargo package manifest found at this repository");
    let labels: Vec<_> = options.iter().map(|item| format!("{} ({})", item.package, if item.dir.is_empty() { "root" } else { &item.dir })).collect();
    let Some(choice) = dialog.choose("Cargo package", &labels, (options.len() == 1).then_some(0))? else { return Ok("Cargo source review cancelled.".into()); };
    let selected = &options[choice];
    let registry_version = cargo_info(&selected.package, repo, dialog).ok();
    let readme = readme_install(repo, dialog).ok();
    let checkout = if readme.is_some() { dz_checkout(repo)? } else { None };
    let mut routes = Vec::new();
    if let Some(version) = &registry_version { routes.push(("crates-io", format!("crates.io {version}"))); }
    if let Some(line) = &readme {
        routes.push(("git", format!("GitHub Git install · {line}")));
        if checkout.is_some() { routes.push(("path", "Local ~/repos/dz checkout".into())); }
    }
    ensure!(!routes.is_empty(), "No verified Cargo install route (registry or README + manifest)");
    let labels: Vec<_> = routes.iter().map(|(_, label)| label.clone()).collect();
    let Some(route_choice) = dialog.choose("Cargo install source", &labels, (routes.len() == 1).then_some(0))? else { return Ok("Cargo source review cancelled.".into()); };
    let route = routes[route_choice].0;
    if route == "path" {
        let root = checkout.as_deref().context("Local checkout disappeared")?;
        let manifest = std::path::Path::new(root).join(&selected.dir).join("Cargo.toml");
        let local: toml::Value = toml::from_str(&std::fs::read_to_string(&manifest)
            .with_context(|| format!("Cannot read local {}", manifest.display()))?)?;
        ensure!(local.get("package").and_then(|p| p.get("name")).and_then(toml::Value::as_str)
            == Some(selected.package.as_str()), "Local checkout package differs from GitHub manifest");
    }
    let revision = if route == "git" { Some(git_revision(repo, dialog)?) } else { None };
    let version = match route {
        "crates-io" => registry_version.unwrap(),
        "git" => revision.clone().unwrap(),
        _ => "current checkout".into(),
    };
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
        repo: repo.into(), package: Some(selected.package.clone()), registry: Some(route.into()),
        root: Some(root), bins: vec![bin.clone()], branch: revision,
        remote: if route == "path" { checkout.map(|root| if selected.dir.is_empty() { root } else { format!("{root}/{}", selected.dir) }) } else { None }, ..Recipe::default() });
    if reviewed.launch.is_none() {
        let directory = selected.dir.rsplit('/').next().unwrap_or("");
        let gui_default = if directory == "cli" { false } else if directory == "gui" { true }
            else { app.tags.iter().any(|tag| tag == "gui") };
        let Some(mode) = dialog.choose("Launch mode", &["Terminal".into(), "GUI".into()], Some(usize::from(gui_default)))?
        else { return Ok("Cargo source review cancelled; nothing saved.".into()); };
        reviewed.launch = Some(Launch { program: target.clone(), args: vec![], gui: mode == 1 });
    }
    let launch_note = reviewed.launch.as_ref().map(|launch| {
        let old_destination = app.recipe.as_ref().map(|recipe| crate::ledger::expand_path(&recipe.destination));
        if crate::ledger::expand_path(&launch.program) == crate::ledger::expand_path(&target) {
            "Launch already points at the Cargo command.".to_string()
        } else if old_destination.as_ref().is_some_and(|path| *path == crate::ledger::expand_path(&launch.program)) {
            "Launch uses the old managed command; it will switch to the Cargo command after a successful install.".to_string()
        } else {
            format!("Custom launch {} will be preserved. Review it after installation if you want to launch the Cargo command.", launch.program)
        }
    }).unwrap_or_default();
    if let Some(launch) = reviewed.launch.as_ref() {
        let old_recipe_command = app.recipe.as_ref().is_some_and(|recipe| !recipe.destination.is_empty()
            && crate::ledger::expand_path(&recipe.destination) == crate::ledger::expand_path(&launch.program));
        let old_installed_command = app.installed_paths.iter().any(|path|
            crate::ledger::expand_path(path) == crate::ledger::expand_path(&launch.program));
        if crate::ledger::expand_path(&launch.program) != crate::ledger::expand_path(&target)
            && (old_recipe_command || old_installed_command) {
            reviewed.recipe.as_mut().unwrap().launch_from = Some(launch.program.clone());
        }
    }
    dialog.message(format!("Review Cargo install\n{} {version}\nsource {route} · package {}\nbinary {bin} → {target}\n{launch_note}\nNo installation yet.", app.name, selected.package));
    match dialog.choose("Save or install", &["Save recipe only".into(), "Save and install".into(), "Cancel".into()], None)? {
        Some(0) => { if app.recipe.is_some() { ledger.replace_recipe(&reviewed)?; } else { ledger.register_recipe(&reviewed)?; } Ok(format!("{} Cargo recipe saved; nothing installed.", app.name)) }
        Some(1) => {
            if app.recipe.is_some() { ledger.replace_recipe(&reviewed)?; } else { ledger.register_recipe(&reviewed)?; }
            crate::dialog::install_one(ledger, index, dialog)
        }
        _ => Ok("Cargo source review cancelled; nothing saved.".into()),
    }
}
