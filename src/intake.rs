//! Explicit GitHub intake. Nothing is saved until the final review.
use crate::{
    archive,
    ledger::{App, Disposition, Launch, Ledger, Provenance, expand_path, normalize_identity},
    update::{self, Recipe},
};
use anyhow::{Context, Result, ensure};
use std::{
    fs,
    io::{self, Write},
    path::Path,
    process::Command,
};

pub(crate) fn prompt(label: &str, default: &str) -> Result<Option<String>> {
    if default.is_empty() {
        print!("{label}: ");
    } else {
        print!("{label} [{default}]: ");
    }
    io::stdout().flush()?;
    let mut text = String::new();
    if io::stdin().read_line(&mut text)? == 0 || text.trim() == "q" {
        return Ok(None);
    }
    Ok(Some(if text.trim().is_empty() {
        default.into()
    } else {
        text.trim().into()
    }))
}

pub(crate) fn choose(
    label: &str,
    names: &[String],
    default: Option<usize>,
) -> Result<Option<usize>> {
    for (i, name) in names.iter().enumerate() {
        println!("{}  {name}", i + 1);
    }
    loop {
        let Some(answer) = prompt(
            label,
            &default.map(|i| (i + 1).to_string()).unwrap_or_default(),
        )?
        else {
            return Ok(None);
        };
        if let Ok(n) = answer.parse::<usize>() {
            if n > 0 && n <= names.len() {
                return Ok(Some(n - 1));
            }
        }
        println!("Choose 1–{}, or q to cancel.", names.len());
    }
}

fn repository(input: &str) -> Result<String> {
    let input = input.trim().trim_end_matches('/').trim_end_matches(".git");
    let repo = input
        .strip_prefix("https://github.com/")
        .or_else(|| input.strip_prefix("http://github.com/"))
        .unwrap_or(input);
    let parts: Vec<_> = repo.split('/').collect();
    ensure!(
        parts.len() == 2 && parts.iter().all(|p| update::safe_name(p)),
        "Use a GitHub repository URL or owner/repo, not a release/file URL"
    );
    Ok(repo.into())
}

fn installer(name: &str) -> Option<&'static str> {
    if name.ends_with(".tar.gz") {
        Some("tar.gz")
    } else if name.ends_with(".tar.xz") {
        Some("tar.xz")
    } else if name.ends_with(".zip") {
        Some("zip")
    } else if name.ends_with(".AppImage") {
        Some("appimage")
    } else if name.ends_with(".exe")
        || name.ends_with(".deb")
        || name.ends_with(".rpm")
        || name.ends_with(".dmg")
        || name.ends_with(".txt")
        || name.ends_with(".json")
        || name.ends_with(".sha256")
        || name.ends_with(".sig")
        || name.ends_with(".asc")
        || name.ends_with(".tar.zst")
        || name.to_ascii_lowercase().contains("checksum")
    {
        None
    } else {
        Some("binary-copy")
    }
}

fn matches_machine(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    let arch = if std::env::consts::ARCH == "x86_64" {
        ["x86_64", "amd64"]
    } else {
        ["aarch64", "arm64"]
    };
    name.contains("linux") && arch.iter().any(|a| name.contains(a))
}

fn asset_pattern(name: &str, tag: &str) -> String {
    // Only a full, delimiter-bounded release token may become a placeholder.
    // Never strip arbitrary digits from a program name.
    for (token, replacement) in [
        (tag, "{tag}"),
        (tag.strip_prefix('v').unwrap_or(tag), "{version}"),
    ] {
        if token.is_empty() {
            continue;
        }
        if let Some((i, _)) = name.match_indices(token).find(|(i, _)| {
            (*i == 0 || matches!(name.as_bytes()[i - 1], b'-' | b'_'))
                && name
                    .as_bytes()
                    .get(i + token.len())
                    .is_some_and(|b| b"-_.".contains(b))
                && !(name.as_bytes().get(i + token.len()) == Some(&b'.')
                    && name
                        .as_bytes()
                        .get(i + token.len() + 1)
                        .is_some_and(u8::is_ascii_digit))
        }) {
            return format!("{}{}{}", &name[..i], replacement, &name[i + token.len()..]);
        }
    }
    name.into()
}

fn draft(repo: &str, name: &str, category: &str, description: &str) -> App {
    App {
        identity: normalize_identity(&format!("https://github.com/{repo}")),
        name: name.into(),
        category: category.into(),
        description: description.into(),
        tags: vec![],
        disposition: Disposition::Considering,
        installed: None,
        outcome: "unknown".into(),
        version: None,
        installed_paths: vec![],
        review: String::new(),
        archived_because: String::new(),
        provenance: Provenance {
            source: "unknown".into(),
            ..Provenance::default()
        },
        launch: None,
        recipe: None,
        installations: vec![],
        nix_migration: None,
    }
}

pub fn interactive(ledger: &mut Ledger, seed: Option<&str>, category: &str) -> Result<String> {
    interactive_with(ledger, seed, category, None, &crate::dialog::Console)
}

macro_rules! say { ($dialog:expr, $($args:tt)*) => { $dialog.message(format!($($args)*)) }; }

pub(crate) fn interactive_with(
    ledger: &mut Ledger,
    seed: Option<&str>,
    category: &str,
    fields: Option<&[String; 5]>,
    dialog: &dyn crate::dialog::Dialog,
) -> Result<String> {
    say!(
        dialog,
        "Add from GitHub — q cancels at any prompt.\nNothing is installed or saved until review."
    );
    let cancel = || Ok("Add cancelled; ledger and installed files unchanged.".to_string());
    let input = match seed {
        Some(url) => url.to_string(),
        None => {
            let Some(url) = dialog.prompt("Repository URL or owner/repo", "")? else {
                return cancel();
            };
            url
        }
    };
    let repo = repository(&input)?;
    let identity = normalize_identity(&format!("https://github.com/{repo}"));
    let existing = ledger
        .apps
        .iter()
        .position(|a| normalize_identity(&a.identity) == identity);
    let mut app = if let Some(i) = existing {
        let app = ledger.apps[i].clone();
        ensure!(
            app.disposition != Disposition::Archived,
            "{} is Archived; move it to Considering before configuring installation",
            app.name
        );
        if app.recipe.is_some() {
            return Ok(format!(
                "{} is already tracked and configured. Use u to update it; nothing changed.",
                app.name
            ));
        }
        say!(
            dialog,
            "\nAlready tracked: {} ({})\nAdding its missing recipe; preserving decisions, notes and installed paths.",
            app.name,
            app.category
        );
        app
    } else {
        say!(dialog, "Looking up {repo}…");
        let output = crate::work::output(
            Command::new("gh")
                .args(["repo", "view", &repo, "--json", "name,description"])
                .env("GH_PROMPT_DISABLED", "1"),
            true,
            Some(dialog.cancel_flag()),
        )?;
        ensure!(
            output.status.success(),
            "Repository lookup failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let info: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        let name = if let Some(fields) = fields {
            if fields[0].trim().is_empty() {
                info["name"]
                    .as_str()
                    .unwrap_or(repo.split('/').next_back().unwrap())
                    .to_string()
            } else {
                fields[0].trim().to_string()
            }
        } else {
            let Some(name) = dialog.prompt(
                "Name",
                info["name"]
                    .as_str()
                    .unwrap_or(repo.split('/').next_back().unwrap()),
            )?
            else {
                return cancel();
            };
            name
        };
        let category = if let Some(fields) = fields {
            fields[1].clone()
        } else {
            let categories = crate::ui::categories(ledger);
            say!(dialog, "\nCategories:");
            for (i, name) in categories.iter().enumerate() {
                say!(dialog, "{}  {name}", i + 1);
            }
            let Some(category) = dialog.prompt(
                "Category number or new category",
                if category.is_empty() {
                    "Development Workspaces"
                } else {
                    category
                },
            )?
            else {
                return cancel();
            };
            let category = match category.parse::<usize>() {
                Ok(n) => categories
                    .get(n.wrapping_sub(1))
                    .context("Invalid category number")?
                    .clone(),
                Err(_) => category,
            };
            category
        };
        ensure!(
            !name.is_empty() && !category.is_empty(),
            "Name and category are required"
        );
        let mut app = draft(
            &repo,
            &name,
            &category,
            info["description"].as_str().unwrap_or_default(),
        );
        if let Some(fields) = fields {
            if !fields[3].trim().is_empty() {
                app.description = fields[3].trim().into();
            }
            app.tags = fields[4]
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
        }
        app
    };
    say!(dialog, "\nLooking up latest stable release…");
    let release = update::release_cancellable(&repo, dialog.cancel_flag())?;
    if dialog.cancelled() {
        return cancel();
    }
    let mut assets: Vec<_> = release
        .assets
        .iter()
        .filter(|a| installer(&a.name).is_some())
        .cloned()
        .collect();
    assets.sort_by_key(|a| (!matches_machine(&a.name), a.name.clone()));
    ensure!(
        !assets.is_empty(),
        "No supported release assets. Use Ctrl-S in the Add form for a record-only entry."
    );
    say!(
        dialog,
        "\n{} — this machine: {}/{}\nPlatform matches are listed first; ELF architecture is verified after download.",
        release.tag_name,
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    let names: Vec<_> = assets
        .iter()
        .map(|a| {
            format!(
                "{} ({} bytes){}",
                a.name,
                a.size,
                if matches_machine(&a.name) { " *" } else { "" }
            )
        })
        .collect();
    let default = (assets.iter().filter(|a| matches_machine(&a.name)).count() == 1).then_some(0);
    let Some(choice) = dialog.choose("Asset number (downloads for inspection)", &names, default)?
    else {
        return cancel();
    };
    let asset = &assets[choice];
    let kind = installer(&asset.name).unwrap();
    let bin_dir = std::env::var("APPTRACK_BIN_DIR").unwrap_or_else(|_| "~/.local/bin".into());
    let mut recipe = Recipe {
        source: "github".into(),
        repo,
        asset: asset_pattern(&asset.name, &release.tag_name),
        installer: kind.into(),
        destination: format!("{bin_dir}/{}", app.identity.rsplit('/').next().unwrap()),
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        clean_filename: false,
        member: (!matches!(kind, "binary-copy" | "appimage")).then(|| "pending".into()),
        ..Recipe::default()
    };
    app.recipe = Some(recipe.clone());
    // Reuse the installer planner to validate the selected asset URL and digest
    // before making any download request. Member selection follows inspection.
    let inspect_plan = update::make_plan(&app, existing.unwrap_or(0), release.clone())?;
    let downloads =
        expand_path(&std::env::var("APPTRACK_DOWNLOADS").unwrap_or_else(|_| "~/Downloads".into()));
    fs::create_dir_all(&downloads)?;
    let staging = tempfile::Builder::new()
        .prefix("apptrack-add-")
        .tempdir_in(&downloads)?;
    let result = (|| {
        let artifact = staging.path().join(&asset.name);
        say!(dialog, "Downloading {} for inspection…", asset.name);
        update::download_cancellable(&inspect_plan, &artifact, dialog.cancel_flag())?;
        update::verify_download(&artifact, &inspect_plan)?;
        if dialog.cancelled() {
            return cancel();
        }
        let default_command = if kind == "appimage" {
            app.identity.rsplit('/').next().unwrap().to_string()
        } else if kind == "binary-copy" {
            update::clean_filename(&asset.name).to_string()
        } else {
            let members = archive::binaries(&artifact, kind, &recipe.arch)?;
            ensure!(
                !members.is_empty(),
                "No supported {}/{} ELF executable found in archive",
                recipe.os,
                recipe.arch
            );
            say!(dialog, "\nExecutable members (only one will be installed):");
            let Some(member) = dialog.choose(
                "Executable number",
                &members,
                (members.len() == 1).then_some(0),
            )?
            else {
                return cancel();
            };
            recipe.member = Some(members[member].clone());
            update::clean_filename(
                Path::new(&members[member])
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap(),
            )
            .to_string()
        };
        let command = loop {
            let Some(command) =
                dialog.prompt(&format!("Command name in {bin_dir}"), &default_command)?
            else {
                return cancel();
            };
            if !update::safe_name(&command) || command.starts_with('-') {
                say!(dialog, "Use one filename, not a path or shell command.");
                continue;
            }
            recipe.destination = format!("{bin_dir}/{command}");
            let candidate = expand_path(&recipe.destination);
            if let Some(owner) = ledger.apps.iter().find(|other| {
                normalize_identity(&other.identity) != normalize_identity(&app.identity)
                    && (other
                        .installed_paths
                        .iter()
                        .any(|p| expand_path(p) == candidate)
                        || other
                            .recipe
                            .as_ref()
                            .is_some_and(|r| expand_path(&r.destination) == candidate))
            }) {
                say!(
                    dialog,
                    "That path is recorded for {}. Choose another command name.",
                    owner.name
                );
                continue;
            }
            break command;
        };
        let launch = if kind == "appimage" {
            let gui = if let Some(old) = &app.launch {
                old.gui
            } else {
                let Some(mode) =
                    dialog.choose("Launch mode", &["Terminal".into(), "GUI".into()], Some(1))?
                else {
                    return cancel();
                };
                mode == 1
            };
            Launch {
                program: "appimage-run".into(),
                args: vec![recipe.destination.clone()],
                gui,
            }
        } else if let Some(old) = &app.launch {
            ensure!(
                expand_path(&old.program) == expand_path(&recipe.destination)
                    || app
                        .installed_paths
                        .iter()
                        .any(|p| expand_path(p) == expand_path(&old.program)),
                "Existing launch is a custom wrapper; configure its migration explicitly in TOML"
            );
            Launch {
                program: recipe.destination.clone(),
                ..old.clone()
            }
        } else {
            let Some(mode) =
                dialog.choose("Launch mode", &["Terminal".into(), "GUI".into()], Some(0))?
            else {
                return cancel();
            };
            Launch {
                program: recipe.destination.clone(),
                args: vec![],
                gui: mode == 1,
            }
        };
        app.recipe = Some(recipe.clone());
        // Preserve an old launch until install succeeds; make_plan shows its
        // migration and the receipt switches it only after replacement succeeds.
        let mut plan = update::make_plan(&app, existing.unwrap_or(0), release.clone())?;
        let binary = if matches!(kind, "binary-copy" | "appimage") {
            artifact.clone()
        } else {
            let binary = staging.path().join("selected-binary");
            archive::extract(&artifact, kind, recipe.member.as_deref().unwrap(), &binary)?;
            binary
        };
        update::verify_installable(&binary, &plan)?;
        say!(
            dialog,
            "\nReview\n{} · {}\n{}\n\n{}\n\nFuture asset: {}\nLaunch: {} ({})\n",
            app.name,
            app.category,
            if existing.is_some() {
                "Complete existing record; keep decisions and notes."
            } else {
                "New record in Considering; installation remains unknown until installed."
            },
            plan.summary(),
            recipe.asset,
            launch.program,
            if launch.gui { "GUI" } else { "terminal" }
        );
        let action = dialog.choose(
            "Save or install",
            &[
                "Save recipe only".into(),
                "Save and install".into(),
                "Cancel".into(),
            ],
            None,
        )?;
        let action = match action {
            Some(0) => "s",
            Some(1) => "i",
            _ => return cancel(),
        };
        if dialog.cancelled() {
            return cancel();
        }
        app.launch = Some(launch);
        plan.index = ledger.register_recipe(&app)?;
        if dialog.cancelled() {
            return Ok("Recipe saved; installation cancelled.".into());
        }
        if action == "s" {
            return Ok(format!(
                "{} saved with its update recipe. No executable changed; use u or G to install later.",
                app.name
            ));
        }
        if plan.up_to_date {
            return Ok(format!(
                "{} recipe saved; executable is already current.",
                app.name
            ));
        }
        let result = update::apply_in(
            ledger,
            &plan,
            &downloads,
            &|s| say!(dialog, "{s}"),
            |_, target| {
                fs::copy(&artifact, target)?;
                Ok(())
            },
        );
        if let Err(error) = result {
            ledger.record_failure(plan.index, "update", &format!("{error:#}"))?;
            return Err(error.context("Recipe saved, but installation failed"));
        }
        Ok(format!(
            "{} saved and installed as {command}. Ready for future updates.",
            app.name
        ))
    })();
    match result {
        Ok(message) => Ok(message),
        Err(error) => Err(error.context(format!(
            "Inspection download retained at {}",
            staging.keep().display()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    #[test]
    fn repository_inputs_and_version_tokens_are_explicit() -> Result<()> {
        assert_eq!(
            repository(" https://github.com/TysonLabs/lazyide.git/ ")?,
            "TysonLabs/lazyide"
        );
        assert_eq!(
            repository("paradise-runner/toast")?,
            "paradise-runner/toast"
        );
        for input in [
            "https://example.com/a/b",
            "https://github.com/a/b/releases",
            "a/../b",
            "a/b?x",
            "a/b;cmd",
        ] {
            assert!(repository(input).is_err());
        }
        assert_eq!(
            asset_pattern("keepkit_v0.4.0_linux.tar.gz", "v0.4.0"),
            "keepkit_{tag}_linux.tar.gz"
        );
        assert_eq!(
            asset_pattern("tool_0.4.0_linux.zip", "v0.4.0"),
            "tool_{version}_linux.zip"
        );
        assert_eq!(
            asset_pattern("docx2md-linux-amd64", "v2"),
            "docx2md-linux-amd64"
        );
        assert_eq!(
            asset_pattern("tool_12_linux.zip", "v2"),
            "tool_12_linux.zip"
        );
        assert_eq!(
            asset_pattern("tool_v2.1_linux.zip", "v2"),
            "tool_v2.1_linux.zip"
        );
        assert_eq!(installer("lazyide-linux-x86_64.tar.gz"), Some("tar.gz"));
        assert_eq!(installer("toast-linux-amd64.zip"), Some("zip"));
        assert_eq!(installer("tty7-26.9.2-linux-x86_64.AppImage"), Some("appimage"));
        assert_eq!(installer("checksums.sha256"), None);
        Ok(())
    }

    fn configured(dir: &Path) -> App {
        let mut app = draft("example/tool", "tool", "Tools", "A new tool");
        app.recipe = Some(Recipe {
            source: "github".into(),
            repo: "example/tool".into(),
            asset: "tool-{version}".into(),
            installer: "binary-copy".into(),
            destination: dir.join("tool").to_string_lossy().into(),
            os: "linux".into(),
            arch: std::env::consts::ARCH.into(),
            clean_filename: false,
            member: None,
            ..Recipe::default()
        });
        app.launch = Some(Launch {
            program: app.recipe.as_ref().unwrap().destination.clone(),
            args: vec![],
            gui: false,
        });
        app
    }

    #[test]
    fn registering_new_recipe_is_atomic_and_rejects_duplicates_and_stale_writes() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("ledger.toml");
        fs::write(&path, "schema_version = 1\napps = []\n")?;
        let mut ledger = Ledger::open(&path)?;
        let mut app = configured(dir.path());
        // No half-added record if recipe validation/serialization fails.
        app.launch = None;
        assert!(ledger.register_recipe(&app).is_err());
        assert!(Ledger::open(&path)?.apps.is_empty());
        app = configured(dir.path());
        let mut stale = Ledger::open(&path)?;
        assert_eq!(ledger.register_recipe(&app)?, 0);
        assert_eq!(ledger.apps[0].installed, None);
        assert_eq!(ledger.apps[0].disposition, Disposition::Considering);
        assert!(ledger.apps[0].recipe.is_some());
        assert!(ledger.register_recipe(&app).is_err());
        assert!(stale.register_recipe(&app).is_err());
        assert_eq!(Ledger::open(&path)?.apps.len(), 1);
        assert!(!dir.path().join("tool").exists());
        Ok(())
    }

    #[test]
    fn existing_notes_and_launch_survive_setup_and_failed_install() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let old = dir.path().join("tool-linux-amd64");
        fs::write(&old, "working old executable")?;
        let path = dir.path().join("ledger.toml");
        fs::write(
            &path,
            format!(
                r#"schema_version = 1
[[apps]]
identity = "https://github.com/Example/Tool"
name = "My tool"
category = "Existing category"
disposition = "using"
review = "Keep my review"
installed = true
installed_paths = [{old:?}]
custom_evidence = "keep unknown fields"
[apps.launch]
program = {old:?}
args = []
gui = false
"#,
                old = old.to_string_lossy()
            ),
        )?;
        let mut ledger = Ledger::open(&path)?;
        let app = configured(dir.path());
        let index = ledger.register_recipe(&app)?;
        assert_eq!(ledger.apps.len(), 1);
        assert_eq!(ledger.apps[index].name, "My tool");
        assert_eq!(ledger.apps[index].disposition, Disposition::Using);
        assert_eq!(ledger.apps[index].review, "Keep my review");
        assert_eq!(
            ledger.apps[index].launch.as_ref().unwrap().program,
            old.to_string_lossy()
        );
        assert!(ledger.record(index).contains("keep unknown fields"));
        let mut binary = vec![0; 64];
        binary[..6].copy_from_slice(b"\x7fELF\x02\x01");
        binary[16] = 2;
        binary[18] = if std::env::consts::ARCH == "x86_64" {
            62
        } else {
            183
        };
        let plan = update::make_plan(
            &ledger.apps[index],
            index,
            update::Release {
                tag_name: "v2".into(),
                is_prerelease: false,
                assets: vec![update::Asset {
                    name: "tool-2".into(),
                    api_url: "https://api.github.com/repos/EXAMPLE/TOOL/releases/assets/123".into(),
                    size: 64,
                    digest: Some(format!("sha256:{:x}", Sha256::digest(&binary))),
                }],
            },
        )?;
        assert_eq!(plan.renamed_from.as_deref(), old.to_str());
        assert!(
            update::apply_in(
                &mut ledger,
                &plan,
                &dir.path().join("downloads"),
                &|_| {},
                |_, _| anyhow::bail!("download failed")
            )
            .is_err()
        );
        assert_eq!(
            ledger.apps[index].launch.as_ref().unwrap().program,
            old.to_string_lossy()
        );
        update::apply_in(
            &mut ledger,
            &plan,
            &dir.path().join("downloads"),
            &|_| {},
            |_, p| {
                fs::write(p, &binary)?;
                Ok(())
            },
        )?;
        assert_eq!(
            ledger.apps[index].launch.as_ref().unwrap().program,
            dir.path().join("tool").to_string_lossy()
        );
        assert_eq!(fs::read_to_string(old)?, "working old executable");
        assert_eq!(ledger.apps[index].review, "Keep my review");
        Ok(())
    }
}
