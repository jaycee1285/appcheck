use crate::ledger::{App, Disposition, Ledger, expand_path};
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Read,
    os::unix::fs::PermissionsExt,
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};
use toml_edit::{Array, Item, Table, value};

#[derive(Clone, Debug, Default, Deserialize, serde::Serialize)]
pub struct Recipe {
    pub source: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub repo: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub asset: String,
    pub installer: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub destination: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub os: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub arch: String,
    #[serde(default)]
    pub clean_filename: bool,
    pub member: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bins: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disable_libs: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_from: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Release {
    pub tag_name: String,
    pub is_prerelease: bool,
    pub assets: Vec<Asset>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Asset {
    pub name: String,
    pub api_url: String,
    pub size: u64,
    pub digest: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Plan {
    pub index: usize,
    pub name: String,
    pub identity: String,
    pub recipe: Recipe,
    /// `recipe.member` with `{tag}`/`{version}` resolved against this release.
    pub member: Option<String>,
    pub release: String,
    pub asset: Asset,
    pub destination: PathBuf,
    pub previous_hash: Option<String>,
    pub old_version: Option<String>,
    pub up_to_date: bool,
    pub renamed_from: Option<String>,
}

/// Fills `{tag}`, `{version}`, and `{version_underscores}` from a release tag.
/// Applies to the asset name and archive member, since release tarballs commonly
/// nest their binary under a versioned directory (`tool-v1.2.3/tool`).
pub(crate) fn substitute(template: &str, tag: &str) -> String {
    let version = tag.strip_prefix('v').unwrap_or(tag);
    template
        .replace("{tag}", tag)
        .replace("{version_underscores}", &version.replace('.', "_"))
        .replace("{version}", version)
}

fn forge_host(source: &str) -> Option<&'static str> {
    match source {
        "github" => Some("github.com"),
        "gitlab" => Some("gitlab.com"),
        "codeberg" => Some("codeberg.org"),
        _ => None,
    }
}

pub fn timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
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

pub(crate) fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

pub fn supported_installer(installer: &str) -> bool {
    matches!(installer, "binary-copy" | "tar.gz" | "tar.xz" | "zip" | "appimage" | "appimage-appdir")
}

fn validate_recipe(recipe: &Recipe) -> Result<()> {
    ensure!(
        ((recipe.source == "github" || recipe.source == "codeberg")
            && supported_installer(&recipe.installer))
            || (recipe.source == "gitlab" && matches!(recipe.installer.as_str(), "binary-copy" | "appimage" | "appimage-appdir")),
        "Supported forge installers: GitHub/Codeberg binary-copy/tar.gz/tar.xz/zip/appimage/appimage-appdir; GitLab binary-copy/appimage/appimage-appdir"
    );
    let parts: Vec<_> = recipe.repo.split('/').collect();
    ensure!(
        parts.len() >= 2 && parts.iter().all(|s| safe_name(s)),
        "Recipe repo must be a namespace/repository path"
    );
    ensure!(
        safe_name(
            &recipe
                .asset
                .replace("{tag}", "tag")
                .replace("{version_underscores}", "version")
                .replace("{version}", "version")
        ),
        "Recipe asset must be one exact filename (optional {{tag}}, {{version}}, or {{version_underscores}}), without globs or paths"
    );
    if !matches!(recipe.installer.as_str(), "binary-copy" | "appimage" | "appimage-appdir") {
        let member = recipe
            .member
            .as_deref()
            .context("Archive recipe requires an exact member path")?;
        // Validate the shape with placeholders filled, as the asset name is.
        crate::archive::validate_member(&substitute(member, "tag"))?;
    } else {
        ensure!(
            recipe.member.is_none(),
            "binary-copy and AppImage installers do not take an archive member"
        );
    }
    if recipe.installer == "appimage-appdir" {
        crate::appimage_tree::validate_families(&recipe.disable_libs)?;
    } else {
        ensure!(recipe.disable_libs.is_empty(), "disable_libs is only valid for appimage-appdir");
    }
    ensure!(
        recipe.os == std::env::consts::OS && recipe.arch == std::env::consts::ARCH,
        "Recipe targets {}/{}; this machine is {}/{}",
        recipe.os,
        recipe.arch,
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    ensure!(
        recipe.os == "linux" && ["x86_64", "aarch64"].contains(&recipe.arch.as_str()),
        "Installers currently validate Linux x86_64/aarch64 ELF files"
    );
    Ok(())
}

// Resolve only the parent. A leaf symlink must never be followed or replaced.
fn destination(raw: &str) -> Result<PathBuf> {
    let path = expand_path(raw);
    ensure!(
        path.is_absolute() && !path.components().any(|c| matches!(c, Component::ParentDir)),
        "Destination must be an absolute path without '..'"
    );
    let parent = path
        .parent()
        .context("Destination needs a parent directory")?
        .canonicalize()
        .context("Destination directory must already exist")?;
    ensure!(
        !parent.starts_with("/nix/store"),
        "Nix store artifacts must be updated through Nix"
    );
    let name = path.file_name().context("Destination needs a filename")?;
    Ok(parent.join(name))
}

fn current_hash(path: &Path) -> Result<Option<String>> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            ensure!(
                meta.file_type().is_file(),
                "Refusing to replace a symlink or non-file: {}",
                path.display()
            );
            Ok(Some(sha256(path)?))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn recipe_destination(recipe: &Recipe) -> Result<PathBuf> {
    if recipe.installer == "appimage-appdir" {
        crate::appimage_tree::destination(&recipe.destination)
    } else {
        destination(&recipe.destination)
    }
}

fn installed_hash(plan: &Plan) -> Result<Option<String>> {
    if plan.recipe.installer == "appimage-appdir" {
        crate::appimage_tree::current_hash(&plan.destination)
    } else {
        current_hash(&plan.destination)
    }
}

pub(crate) fn check_cancellable(
    ledger: &Ledger,
    index: usize,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<Plan> {
    ledger.check_unchanged()?;
    let app = &ledger.apps[index];
    ensure!(
        app.disposition != Disposition::Archived,
        "Archived applications are excluded from updates; move it to Considering first"
    );
    let recipe = app
        .recipe
        .as_ref()
        .context("No update recipe recorded for this app")?;
    validate_recipe(recipe)?;
    make_plan(app, index, release_cancellable(&recipe.repo, cancel)?)
}

pub(crate) fn release_cancellable(
    repo: &str,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<Release> {
    let output = crate::work::output(
        Command::new("gh")
            .args([
                "release",
                "view",
                "--repo",
                repo,
                "--json",
                "tagName,assets,isPrerelease",
            ])
            .env("GH_PROMPT_DISABLED", "1"),
        true,
        Some(cancel),
    )
    .context("Cannot run gh release view")?;
    ensure!(
        output.status.success(),
        "GitHub release lookup failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let release: Release =
        serde_json::from_slice(&output.stdout).context("Unexpected gh release JSON")?;
    Ok(release)
}

pub(crate) fn make_plan(app: &App, index: usize, release: Release) -> Result<Plan> {
    ensure!(
        app.disposition != Disposition::Archived,
        "Archived applications are excluded from updates"
    );
    let recipe = app.recipe.as_ref().context("Missing recipe")?.clone();
    validate_recipe(&recipe)?;
    ensure!(
        !release.is_prerelease,
        "Prereleases are not selected automatically"
    );
    ensure!(!release.tag_name.is_empty(), "Release has no tag");
    let asset_name = substitute(&recipe.asset, &release.tag_name);
    ensure!(
        safe_name(&asset_name),
        "Resolved asset must be one safe filename"
    );
    let matches: Vec<_> = release
        .assets
        .into_iter()
        .filter(|a| a.name == asset_name)
        .collect();
    ensure!(
        matches.len() == 1,
        "Expected exactly one asset named {}; found {}",
        asset_name,
        matches.len()
    );
    let asset = matches.into_iter().next().unwrap();
    if recipe.source == "github" {
        let prefix = format!(
            "https://api.github.com/repos/{}/releases/assets/",
            recipe.repo
        );
        ensure!(
            asset
                .api_url
                .get(..prefix.len())
                .is_some_and(|s| s.eq_ignore_ascii_case(&prefix)),
            "Asset download API URL does not match recipe repository"
        );
        let asset_id = &asset.api_url[prefix.len()..];
        ensure!(
            !asset_id.is_empty() && asset_id.bytes().all(|c| c.is_ascii_digit()),
            "Invalid GitHub asset ID"
        );
    } else {
        ensure!(
            asset.api_url.starts_with("https://")
                && !asset.api_url.bytes().any(|byte| byte.is_ascii_whitespace()),
            "Forge asset URL must be one HTTPS URL"
        );
    }
    if let Some(digest) = &asset.digest {
        let hash = digest
            .strip_prefix("sha256:")
            .context("Unsupported published asset digest")?;
        ensure!(
            hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()),
            "Invalid published SHA-256"
        );
    }
    ensure!(
        asset.size >= 64,
        "Release asset is too small to contain an ELF executable"
    );
    let destination = recipe_destination(&recipe)?;
    let previous_hash = if recipe.installer == "appimage-appdir" {
        crate::appimage_tree::current_hash(&destination)?
    } else {
        current_hash(&destination)?
    };
    let member = match &recipe.member {
        Some(template) => {
            let resolved = substitute(template, &release.tag_name);
            crate::archive::validate_member(&resolved)?;
            Some(resolved)
        }
        None => None,
    };
    let receipt_hash = if recipe.installer == "appimage-appdir" {
        app.provenance.appdir_sha256.as_ref()
    } else {
        app.provenance.sha256.as_ref()
    };
    let up_to_date = app.provenance.managed_by_apptrack
        && app.provenance.release.as_deref() == Some(&release.tag_name)
        && app.provenance.installer.as_deref() == Some(&recipe.installer)
        && app.provenance.asset.as_deref() == Some(&asset.name)
        && app.provenance.member == member
        && app.provenance.disabled_libs == recipe.disable_libs
        && previous_hash.as_ref().is_some_and(|hash| {
            receipt_hash == Some(hash)
                && if recipe.source == "github" {
                    asset.digest.as_ref().is_some_and(|digest| {
                        app.provenance
                            .asset_sha256
                            .as_ref()
                            .is_some_and(|h| digest.eq_ignore_ascii_case(&format!("sha256:{h}")))
                    })
                } else {
                    asset.digest.as_ref().is_none_or(|digest| {
                        app.provenance
                            .asset_sha256
                            .as_ref()
                            .is_some_and(|h| digest.eq_ignore_ascii_case(&format!("sha256:{h}")))
                    })
                }
        });
    let mut plan = Plan {
        index,
        name: app.name.clone(),
        identity: app.identity.clone(),
        recipe,
        member,
        release: release.tag_name,
        asset,
        destination,
        previous_hash,
        old_version: app.version.clone(),
        up_to_date,
        renamed_from: None,
    };
    if plan.recipe.clean_filename {
        plan.clean_destination()?;
    }
    if plan.renamed_from.is_none() {
        if let Some(launch) = &app.launch {
            if expand_path(&launch.program) != plan.destination
                && app
                    .installed_paths
                    .iter()
                    .any(|p| expand_path(p) == expand_path(&launch.program))
            {
                plan.renamed_from = Some(launch.program.clone());
            }
        }
    }
    Ok(plan)
}

impl Plan {
    pub fn clean_destination(&mut self) -> Result<bool> {
        ensure!(self.recipe.installer != "appimage-appdir", "AppDir destinations are explicit and cannot be filename-cleaned");
        let old_name = self
            .destination
            .file_name()
            .context("Destination has no filename")?
            .to_str()
            .context("Destination filename is not UTF-8")?;
        let clean = clean_filename(old_name);
        if clean == old_name {
            return Ok(false);
        }
        let target = self.destination.with_file_name(clean);
        ensure!(
            current_hash(&target)?.is_none(),
            "Clean destination already exists: {}; choose the destination explicitly in the recipe",
            target.display()
        );
        self.renamed_from = Some(self.recipe.destination.clone());
        self.recipe.destination = target.to_string_lossy().into_owned();
        self.destination = target;
        self.previous_hash = None;
        self.up_to_date = false;
        Ok(true)
    }

    pub fn summary(&self) -> String {
        let mut summary = format!(
            "{}\n{} → {}\n\nSource\n{}/{}\n\nAsset\n{} ({} bytes)\n\nInstall\n{}\n\nVerification\n{}\n\n{}",
            self.name,
            self.old_version.as_deref().unwrap_or("unknown"),
            self.release,
            forge_host(&self.recipe.source).unwrap_or(&self.recipe.source),
            self.recipe.repo,
            self.asset.name,
            self.asset.size,
            self.destination.display(),
            self.asset.digest.as_deref().unwrap_or(if matches!(self.recipe.installer.as_str(), "appimage" | "appimage-appdir") {
                "No published checksum; size, Type-2 AppImage magic and ELF architecture will be checked"
            } else {
                "No published checksum; size and ELF architecture will be checked"
            }),
            if self.up_to_date {
                if self.recipe.installer == "appimage-appdir" { "Installed AppDir tree matches the current receipt and release." } else { "Installed file matches the current receipt and release." }
            } else if self.previous_hash.is_some() {
                if self.recipe.installer == "appimage-appdir" { "Replace existing AppDir tree. Record a new tree receipt." } else { "Replace existing executable. Record a new installation receipt." }
            } else {
                if self.recipe.installer == "appimage-appdir" { "Extract and install an AppDir tree. Record a new tree receipt." } else { "Install executable. Record a new installation receipt." }
            }
        );
        if self.recipe.installer == "appimage-appdir" {
            summary.push_str("\n\nLaunch\nappimage-run -w <managed AppDir>");
            if self.recipe.disable_libs.is_empty() {
                summary.push_str("\nBundled library overrides: none");
            } else {
                summary.push_str(&format!("\nDisable bundled library families: {}", self.recipe.disable_libs.join(", ")));
            }
        }
        if let Some(member) = &self.member {
            summary.push_str(&format!("\n\nExtract ({})\n{member}\nOnly this regular file is installed; other archive contents are ignored.", self.recipe.installer));
        }
        if let Some(old) = &self.renamed_from {
            summary.push_str(&format!("\n\nSave the clean destination for future updates.\nPrevious filename retained: {old}\nA direct launch recipe targeting that filename will follow the new path."));
        }
        summary
    }
}

// Strip complete trailing platform tokens, never arbitrary digits or substrings.
pub(crate) fn clean_filename(name: &str) -> &str {
    let tokens = [
        "x86_64", "aarch64", "amd64", "arm64", "i686", "i386", "armv7", "linux", "windows",
        "darwin", "macos", "unknown", "gnu", "musl",
    ];
    let mut clean = name;
    loop {
        let next = tokens.iter().find_map(|token| {
            let prefix = clean.strip_suffix(token)?;
            let prefix = prefix
                .strip_suffix('-')
                .or_else(|| prefix.strip_suffix('_'))?;
            (!prefix.is_empty()).then_some(prefix)
        });
        match next {
            Some(prefix) => clean = prefix,
            None => return clean,
        }
    }
}

pub(crate) fn verify_download(path: &Path, plan: &Plan) -> Result<String> {
    ensure!(
        fs::metadata(path)?.len() == plan.asset.size,
        "Downloaded size does not match the planned release asset"
    );
    let hash = sha256(path)?;
    if let Some(digest) = &plan.asset.digest {
        ensure!(
            digest.eq_ignore_ascii_case(&format!("sha256:{hash}")),
            "SHA-256 mismatch; current executable was not changed"
        );
    }
    Ok(hash)
}

pub(crate) fn verify_binary(path: &Path, plan: &Plan) -> Result<String> {
    let hash = sha256(path)?;
    let mut header = [0; 64];
    fs::File::open(path)?.read_exact(&mut header)?;
    ensure!(
        &header[..4] == b"\x7fELF" && header[4] == 2 && header[5] == 1,
        "Expected a 64-bit little-endian ELF executable"
    );
    let machine = u16::from_le_bytes([header[18], header[19]]);
    let expected = if plan.recipe.arch == "x86_64" {
        62
    } else {
        183
    };
    ensure!(
        machine == expected,
        "ELF architecture does not match {}",
        plan.recipe.arch
    );
    let kind = u16::from_le_bytes([header[16], header[17]]);
    ensure!(
        kind == 2 || kind == 3,
        "ELF is not an executable/shared object"
    );
    Ok(hash)
}

pub(crate) fn verify_appimage(path: &Path, plan: &Plan) -> Result<String> {
    let hash = verify_binary(path, plan)?;
    let mut header = [0; 12];
    fs::File::open(path)?.read_exact(&mut header)?;
    ensure!(
        &header[8..12] == b"AI\x02\0",
        "Expected a Type-2 AppImage (AI 02 magic)"
    );
    Ok(hash)
}

pub(crate) fn verify_installable(path: &Path, plan: &Plan) -> Result<String> {
    if matches!(plan.recipe.installer.as_str(), "appimage" | "appimage-appdir") {
        verify_appimage(path, plan)
    } else {
        verify_binary(path, plan)
    }
}

pub(crate) fn download_cancellable(
    plan: &Plan,
    target: &Path,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<()> {
    let file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(target)?;
    let mut command = if plan.recipe.source == "github" {
        let mut command = Command::new("gh");
        command
            .args([
                "api",
                &plan.asset.api_url,
                "--header",
                "Accept: application/octet-stream",
            ])
            .env("GH_PROMPT_DISABLED", "1");
        command
    } else {
        let mut command = Command::new("curl");
        command.args([
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            &plan.asset.api_url,
        ]);
        command
    };
    let output = crate::work::output(
        command.stdout(Stdio::from(file)).stderr(Stdio::piped()),
        false,
        Some(cancel),
    )
    .context("Cannot download forge release asset")?;
    ensure!(
        output.status.success(),
        "Forge asset download failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

pub(crate) fn apply_in(
    ledger: &mut Ledger,
    plan: &Plan,
    staging_root: &Path,
    progress: &impl Fn(&str),
    download: impl FnOnce(&Plan, &Path) -> Result<()>,
) -> Result<()> {
    if plan.recipe.installer == "appimage-appdir" {
        return apply_appdir_in(
            ledger,
            plan,
            staging_root,
            progress,
            download,
            |artifact, prepared, families| {
                crate::appimage_tree::prepare(
                    artifact,
                    prepared,
                    families,
                    &std::sync::atomic::AtomicBool::new(false),
                )
            },
        );
    }
    ensure!(!plan.up_to_date, "Already up to date");
    ledger.check_unchanged()?;
    ensure!(
        ledger
            .apps
            .get(plan.index)
            .is_some_and(|a| a.identity == plan.identity),
        "Application changed since planning"
    );
    ensure!(
        recipe_destination(&plan.recipe)? == plan.destination,
        "Destination directory changed since planning"
    );
    ensure!(
        installed_hash(plan)? == plan.previous_hash,
        "Installed file changed since planning; check again"
    );
    fs::create_dir_all(staging_root)?;
    let staging = tempfile::Builder::new()
        .prefix("apptrack-")
        .tempdir_in(staging_root)?;
    let artifact = staging.path().join(&plan.asset.name);
    let result = (|| {
        progress("Staging release asset…");
        download(plan, &artifact)?;
        progress("Verifying downloaded size and checksum…");
        let asset_hash = verify_download(&artifact, plan)?;
        let binary = if matches!(plan.recipe.installer.as_str(), "binary-copy" | "appimage") {
            artifact.clone()
        } else {
            progress("Extracting the exact archive member…");
            let binary = staging.path().join("extracted-binary");
            crate::archive::extract(
                &artifact,
                &plan.recipe.installer,
                plan.member.as_deref().context("Missing archive member")?,
                &binary,
            )?;
            binary
        };
        progress(if plan.recipe.installer == "appimage" {
            "Verifying Type-2 AppImage and ELF architecture…"
        } else {
            "Verifying ELF architecture…"
        });
        let hash = verify_installable(&binary, plan)?;
        let parent = plan.destination.parent().unwrap();
        let mut prepared = tempfile::NamedTempFile::new_in(parent)
            .context("Cannot prepare executable beside destination")?;
        std::io::copy(&mut fs::File::open(&binary)?, prepared.as_file_mut())?;
        prepared
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o755))?;
        prepared.as_file().sync_all()?;
        // Serialize concurrent Track installs and refuse stale plans before mutation.
        let _lock = ledger.write_lock()?;
        ensure!(
            recipe_destination(&plan.recipe)? == plan.destination,
            "Destination directory changed during download"
        );
        ensure!(
            installed_hash(plan)? == plan.previous_hash,
            "Installed file changed during download; check again"
        );
        // A temporary copy exists only through receipt commit, never a rollback pile.
        let backup = if plan.previous_hash.is_some() {
            let backup = tempfile::NamedTempFile::new_in(parent)?;
            fs::copy(&plan.destination, backup.path())?;
            backup.as_file().sync_all()?;
            Some(backup)
        } else {
            None
        };
        let receipt = receipt_document(ledger, plan, &hash, &asset_hash)?;
        progress("Replacing executable and saving receipt…");
        prepared
            .persist(&plan.destination)
            .context("Cannot replace executable")?;
        if let Err(error) = ledger.save_locked(receipt) {
            match backup {
                Some(backup) => {
                    if let Err(rollback) = backup.persist(&plan.destination) {
                        let rollback_error = rollback.error.to_string();
                        let saved = rollback.file.into_temp_path().keep()?;
                        bail!(
                            "Receipt failed: {error:#}; rollback failed: {rollback_error}. Previous executable retained at {}",
                            saved.display()
                        );
                    }
                }
                None => fs::remove_file(&plan.destination)
                    .context("Receipt failed and new executable could not be removed")?,
            }
            bail!("Receipt could not be saved; executable restored: {error:#}");
        }
        progress("Installed; receipt saved.");
        Ok(())
    })();
    match result {
        Ok(()) => Ok(()),
        Err(error) => {
            let retained = staging.keep();
            Err(error.context(format!("Staging retained at {}", retained.display())))
        }
    }
}

fn apply_appdir_in(
    ledger: &mut Ledger,
    plan: &Plan,
    staging_root: &Path,
    progress: &impl Fn(&str),
    download: impl FnOnce(&Plan, &Path) -> Result<()>,
    prepare: impl FnOnce(&Path, &Path, &[String]) -> Result<String>,
) -> Result<()> {
    ensure!(!plan.up_to_date, "Already up to date");
    ledger.check_unchanged()?;
    ensure!(
        ledger.apps.get(plan.index).is_some_and(|app| app.identity == plan.identity),
        "Application changed since planning"
    );
    ensure!(
        recipe_destination(&plan.recipe)? == plan.destination,
        "AppDir destination changed since planning"
    );
    ensure!(installed_hash(plan)? == plan.previous_hash, "Installed AppDir changed since planning; check again");
    fs::create_dir_all(staging_root)?;
    let download_staging = tempfile::Builder::new()
        .prefix("apptrack-")
        .tempdir_in(staging_root)?;
    let artifact = download_staging.path().join(&plan.asset.name);
    let result = (|| {
        progress("Staging release asset…");
        download(plan, &artifact)?;
        progress("Verifying downloaded size and checksum…");
        let asset_hash = verify_download(&artifact, plan)?;
        progress("Verifying Type-2 AppImage and ELF architecture…");
        verify_installable(&artifact, plan)?;

        let parent = plan.destination.parent().context("AppDir destination has no parent")?;
        fs::create_dir_all(parent)?;
        let canonical_parent = parent.canonicalize()?;
        ensure!(!canonical_parent.starts_with("/nix/store"), "Nix store paths are not AppDir destinations");
        let tree_staging = tempfile::Builder::new()
            .prefix(".apptrack-appdir-")
            .tempdir_in(&canonical_parent)?;
        let prepared = tree_staging.path().join("prepared");
        progress("Extracting the managed AppDir tree…");
        let tree_hash = prepare(&artifact, &prepared, &plan.recipe.disable_libs)?;

        let _lock = ledger.write_lock()?;
        ensure!(recipe_destination(&plan.recipe)? == plan.destination, "AppDir destination changed during download");
        ensure!(installed_hash(plan)? == plan.previous_hash, "Installed AppDir changed during download; check again");

        let old_staging = tempfile::Builder::new()
            .prefix(".apptrack-previous-appdir-")
            .tempdir_in(&canonical_parent)?;
        let previous = old_staging.path().join("previous");
        let had_previous = plan.previous_hash.is_some();
        if had_previous {
            fs::rename(&plan.destination, &previous)
                .context("Cannot stage previous AppDir for rollback")?;
        }
        if let Err(error) = fs::rename(&prepared, &plan.destination) {
            if had_previous {
                if let Err(rollback) = fs::rename(&previous, &plan.destination) {
                    let retained = old_staging.keep();
                    bail!("Cannot install AppDir: {error}; previous AppDir rollback failed: {rollback}. Previous tree retained at {}", retained.display());
                }
            }
            return Err(error).context("Cannot install prepared AppDir");
        }

        let receipt = receipt_document(ledger, plan, &tree_hash, &asset_hash)?;
        progress("Replacing AppDir tree and saving receipt…");
        if let Err(error) = ledger.save_locked(receipt) {
            let rejected = tree_staging.path().join("rejected");
            if let Err(rollback) = fs::rename(&plan.destination, &rejected) {
                let old_retained = old_staging.keep();
                bail!("Receipt failed: {error:#}; new AppDir could not be staged for rollback: {rollback}. Previous tree retained at {}", old_retained.display());
            }
            if had_previous {
                if let Err(rollback) = fs::rename(&previous, &plan.destination) {
                    let old_retained = old_staging.keep();
                    let new_retained = tree_staging.keep();
                    bail!("Receipt failed: {error:#}; previous AppDir rollback failed: {rollback}. Trees retained at {} and {}", old_retained.display(), new_retained.display());
                }
            }
            bail!("Receipt could not be saved; AppDir restored: {error:#}");
        }
        progress("Installed AppDir; tree receipt saved.");
        Ok(())
    })();
    match result {
        Ok(()) => Ok(()),
        Err(error) => {
            let retained = download_staging.keep();
            Err(error.context(format!("Download staging retained at {}", retained.display())))
        }
    }
}

fn receipt_document(
    ledger: &Ledger,
    plan: &Plan,
    hash: &str,
    asset_hash: &str,
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
    provenance["source"] = value(&plan.recipe.source);
    provenance["repo"] = value(format!(
        "https://{}/{}",
        forge_host(&plan.recipe.source).context("Unknown forge source")?,
        plan.recipe.repo
    ));
    provenance["release"] = value(&plan.release);
    provenance["asset"] = value(&plan.asset.name);
    provenance["asset_api_url"] = value(&plan.asset.api_url);
    provenance["installer"] = value(&plan.recipe.installer);
    if matches!(plan.recipe.installer.as_str(), "appimage" | "appimage-appdir") {
        provenance["appimage_type"] = value(2);
    }
    provenance["asset_sha256"] = value(asset_hash);
    if let Some(member) = &plan.member {
        provenance["member"] = value(member);
    }
    provenance["managed_by_apptrack"] = value(true);
    if plan.recipe.installer == "appimage-appdir" {
        provenance["appdir_sha256"] = value(hash);
        let mut families = Array::new();
        for family in &plan.recipe.disable_libs {
            families.push(family.as_str());
        }
        provenance["disabled_libs"] = value(families);
    } else {
        provenance["sha256"] = value(hash);
    }
    provenance["installed_at_unix"] = value(timestamp());
    let mut paths = Array::new();
    paths.push(plan.destination.to_string_lossy().as_ref());
    provenance["installed_paths"] = value(paths);
    app["provenance"] = Item::Table(provenance);
    app["version"] = value(&plan.release);
    app["installed"] = value(true);
    let mut paths: Array = ledger.apps[plan.index]
        .installed_paths
        .iter()
        .map(|s| s.as_str())
        .collect();
    if !ledger.apps[plan.index]
        .installed_paths
        .iter()
        .any(|p| expand_path(p) == plan.destination)
    {
        paths.push(plan.recipe.destination.as_str());
    }
    app["installed_paths"] = value(paths);
    if let Some(old) = &plan.renamed_from {
        app["recipe"]["destination"] = value(&plan.recipe.destination);
        if ledger.apps[plan.index]
            .launch
            .as_ref()
            .is_some_and(|launch| expand_path(&launch.program) == expand_path(old))
        {
            app["launch"]["program"] = value(&plan.recipe.destination);
        }
    }
    let mut action = Table::new();
    action["action"] = value(if plan.previous_hash.is_some() {
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
    use std::io::Write;

    struct Fixture {
        dir: tempfile::TempDir,
        ledger: Ledger,
        plan: Plan,
        binary: Vec<u8>,
    }

    fn fixture() -> Result<Fixture> {
        fixture_named("tool")
    }

    fn fixture_named(filename: &str) -> Result<Fixture> {
        let dir = tempfile::tempdir()?;
        let target = dir.path().join(filename);
        fs::write(&target, b"known working executable")?;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o751))?;
        let ledger_path = dir.path().join("ledger.toml");
        fs::write(
            &ledger_path,
            format!(
                r#"schema_version = 1
# Keep human context.
[[apps]]
identity = "https://github.com/example/tool"
name = "tool"
category = "Tools"
disposition = "considering"
installed = true
installed_paths = [{target:?}]
outcome = "unknown"
review = "Keep this review"
[apps.provenance]
source = "manual"
managed_by_apptrack = false
extra_evidence = "retain in history"
[apps.launch]
program = {target:?}
args = []
gui = false
[apps.recipe]
source = "github"
repo = "example/tool"
asset = "tool-linux"
installer = "binary-copy"
destination = {target:?}
os = "linux"
arch = "{arch}"
"#,
                target = target.to_string_lossy(),
                arch = std::env::consts::ARCH
            ),
        )?;
        let ledger = Ledger::open(&ledger_path)?;
        let mut binary = vec![0; 64];
        binary[..6].copy_from_slice(b"\x7fELF\x02\x01");
        binary[16] = 2;
        binary[18] = if std::env::consts::ARCH == "x86_64" {
            62
        } else {
            183
        };
        let release = Release {
            tag_name: "v2".into(),
            is_prerelease: false,
            assets: vec![Asset {
                name: "tool-linux".into(),
                api_url: "https://api.github.com/repos/example/tool/releases/assets/123".into(),
                size: binary.len() as u64,
                digest: Some(format!("sha256:{:x}", Sha256::digest(&binary))),
            }],
        };
        let plan = make_plan(&ledger.apps[0], 0, release)?;
        Ok(Fixture {
            dir,
            ledger,
            plan,
            binary,
        })
    }

    #[test]
    fn archive_receipts_distinguish_download_and_installed_hashes() -> Result<()> {
        let mut f = fixture()?;
        let tar = crate::archive::tests::tar_bytes(&[(
            "tool-v2/2/bin/tool",
            tar::EntryType::Regular,
            &f.binary,
        )])?;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&tar)?;
        let bytes = encoder.finish()?;
        let mut doc = f.ledger.document();
        doc["apps"][0]["recipe"]["installer"] = value("tar.gz");
        doc["apps"][0]["recipe"]["member"] = value("tool-{tag}/{version}/bin/tool");
        doc["apps"][0]["recipe"]["asset"] = value("tool_{tag}_linux.tar.gz");
        fs::write(&f.ledger.path, doc.to_string())?;
        f.ledger = Ledger::open(&f.ledger.path)?;
        let asset = Asset {
            name: "tool_v2_linux.tar.gz".into(),
            size: bytes.len() as u64,
            digest: Some(format!("sha256:{:x}", Sha256::digest(&bytes))),
            ..f.plan.asset.clone()
        };
        let release = || Release {
            tag_name: "v2".into(),
            is_prerelease: false,
            assets: vec![asset.clone()],
        };
        let plan = make_plan(&f.ledger.apps[0], 0, release())?;
        assert_eq!(plan.member.as_deref(), Some("tool-v2/2/bin/tool"));
        assert!(plan.summary().contains("tool-v2/2/bin/tool"));
        apply_in(
            &mut f.ledger,
            &plan,
            &f.dir.path().join("downloads"),
            &|_| {},
            |_, path| {
                fs::write(path, &bytes)?;
                Ok(())
            },
        )?;
        assert_eq!(fs::read(&plan.destination)?, f.binary);
        let app = &f.ledger.apps[0];
        assert_eq!(
            app.provenance.sha256.as_deref(),
            Some(format!("{:x}", Sha256::digest(&f.binary)).as_str())
        );
        assert_eq!(
            app.provenance.asset_sha256.as_deref(),
            Some(format!("{:x}", Sha256::digest(&bytes)).as_str())
        );
        assert!(make_plan(app, 0, release())?.up_to_date);
        let mut changed = app.clone();
        changed.recipe.as_mut().unwrap().member = Some("different".into());
        assert!(!make_plan(&changed, 0, release())?.up_to_date);
        fs::write(&plan.destination, b"changed executable")?;
        assert!(!make_plan(app, 0, release())?.up_to_date);
        Ok(())
    }

    #[test]
    fn archive_failures_preserve_executable_and_ledger() -> Result<()> {
        for failure in [
            "checksum",
            "missing",
            "architecture",
            "duplicate",
            "truncated",
        ] {
            let mut f = fixture()?;
            let original_ledger = fs::read(&f.ledger.path)?;
            let original_binary = fs::read(&f.plan.destination)?;
            let mut binary = f.binary.clone();
            if failure == "architecture" {
                binary[18] = 0;
            }
            let name = if failure == "missing" {
                "other"
            } else {
                "tool"
            };
            let mut entries = vec![(name, tar::EntryType::Regular, binary.as_slice())];
            if failure == "duplicate" {
                entries.push(entries[0]);
            }
            let tar = crate::archive::tests::tar_bytes(&entries)?;
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(&tar)?;
            let mut bytes = encoder.finish()?;
            if failure == "truncated" {
                bytes.truncate(bytes.len() - 8);
            }
            f.plan.recipe.installer = "tar.gz".into();
            f.plan.recipe.member = Some("tool".into());
            f.plan.asset.size = bytes.len() as u64;
            f.plan.asset.digest = Some(format!("sha256:{:x}", Sha256::digest(&bytes)));
            if failure == "checksum" {
                bytes[0] ^= 1;
            }
            let result = apply_in(
                &mut f.ledger,
                &f.plan,
                &f.dir.path().join("downloads"),
                &|_| {},
                |_, path| {
                    fs::write(path, &bytes)?;
                    Ok(())
                },
            );
            assert!(result.is_err(), "{failure}");
            assert_eq!(fs::read(&f.plan.destination)?, original_binary, "{failure}");
            assert_eq!(fs::read(&f.ledger.path)?, original_ledger, "{failure}");
        }
        Ok(())
    }

    #[test]
    fn successful_install_records_exact_authority_and_keeps_prior_provenance() -> Result<()> {
        let mut f = fixture()?;
        apply_in(
            &mut f.ledger,
            &f.plan,
            &f.dir.path().join("downloads"),
            &|_| {},
            |_, path| {
                fs::write(path, &f.binary)?;
                Ok(())
            },
        )?;
        assert_eq!(fs::read(&f.plan.destination)?, f.binary);
        assert_eq!(
            fs::metadata(&f.plan.destination)?.permissions().mode() & 0o777,
            0o755
        );
        let app = &f.ledger.apps[0];
        assert_eq!(app.disposition, Disposition::Considering);
        assert_eq!(app.review, "Keep this review");
        assert_eq!(app.outcome, "unknown");
        assert_eq!(app.version.as_deref(), Some("v2"));
        assert!(app.provenance.managed_by_apptrack);
        let record = f.ledger.record(0);
        assert!(record.contains("[[apps.provenance_history]]"));
        assert!(record.contains("retain in history"));
        assert!(record.contains("result = \"success\""));
        assert!(fs::read_to_string(&f.ledger.path)?.contains("# Keep human context."));
        assert_eq!(fs::read_dir(f.dir.path().join("downloads"))?.count(), 0);
        let next = make_plan(
            app,
            0,
            Release {
                tag_name: "v2".into(),
                is_prerelease: false,
                assets: vec![f.plan.asset.clone()],
            },
        )?;
        assert!(next.up_to_date);
        let mut without_digest = f.plan.asset.clone();
        without_digest.digest = None;
        assert!(
            !make_plan(
                app,
                0,
                Release {
                    tag_name: "v2".into(),
                    is_prerelease: false,
                    assets: vec![without_digest]
                }
            )?
            .up_to_date
        );
        Ok(())
    }

    #[test]
    fn failed_verification_preserves_executable_and_retains_download() -> Result<()> {
        let mut f = fixture()?;
        let original = fs::read(&f.ledger.path)?;
        let result = apply_in(
            &mut f.ledger,
            &f.plan,
            &f.dir.path().join("downloads"),
            &|_| {},
            |_, path| {
                let mut corrupted = f.binary.clone();
                corrupted[32] = 1;
                fs::write(path, corrupted)?;
                Ok(())
            },
        );
        assert!(format!("{:#}", result.unwrap_err()).contains("SHA-256 mismatch"));
        assert_eq!(fs::read(&f.plan.destination)?, b"known working executable");
        assert_eq!(fs::read(&f.ledger.path)?, original);
        assert_eq!(fs::read_dir(f.dir.path().join("downloads"))?.count(), 1);
        f.ledger
            .record_failure(0, "update", "SHA-256 mismatch; staging retained")?;
        assert!(!f.ledger.apps[0].provenance.managed_by_apptrack);
        assert!(f.ledger.record(0).contains("result = \"failed\""));
        Ok(())
    }

    #[test]
    fn changed_executable_during_download_is_not_clobbered() -> Result<()> {
        let mut f = fixture()?;
        let result = apply_in(
            &mut f.ledger,
            &f.plan,
            &f.dir.path().join("downloads"),
            &|_| {},
            |_, path| {
                fs::write(path, &f.binary)?;
                fs::write(&f.plan.destination, "manual replacement")?;
                Ok(())
            },
        );
        assert!(format!("{:#}", result.unwrap_err()).contains("changed during download"));
        assert_eq!(
            fs::read_to_string(&f.plan.destination)?,
            "manual replacement"
        );
        Ok(())
    }

    #[test]
    fn receipt_failure_restores_binary_and_mode_without_overwriting_external_ledger() -> Result<()>
    {
        let mut f = fixture()?;
        let ledger_path = f.ledger.path.clone();
        let external = format!("{}\n# concurrent edit\n", fs::read_to_string(&ledger_path)?);
        let result = apply_in(
            &mut f.ledger,
            &f.plan,
            &f.dir.path().join("downloads"),
            &|phase| {
                if phase.starts_with("Replacing") {
                    fs::write(&ledger_path, &external).unwrap();
                }
            },
            |_, path| {
                fs::write(path, &f.binary)?;
                Ok(())
            },
        );
        assert!(format!("{:#}", result.unwrap_err()).contains("executable restored"));
        assert_eq!(fs::read(&f.plan.destination)?, b"known working executable");
        assert_eq!(
            fs::metadata(&f.plan.destination)?.permissions().mode() & 0o777,
            0o751
        );
        assert_eq!(fs::read_to_string(&ledger_path)?, external);
        Ok(())
    }

    #[test]
    fn rejects_ambiguous_assets_wrong_architecture_archives_and_symlinks() -> Result<()> {
        let f = fixture()?;
        let release = |assets| Release {
            tag_name: "v2".into(),
            is_prerelease: false,
            assets,
        };
        assert!(
            make_plan(
                &f.ledger.apps[0],
                0,
                release(vec![f.plan.asset.clone(), f.plan.asset.clone()])
            )
            .is_err()
        );
        assert!(make_plan(&f.ledger.apps[0], 0, release(vec![])).is_err());
        let mut app = f.ledger.apps[0].clone();
        app.disposition = Disposition::Archived;
        assert!(make_plan(&app, 0, release(vec![f.plan.asset.clone()])).is_err());
        let mut app = f.ledger.apps[0].clone();
        app.recipe.as_mut().unwrap().arch = "wrong".into();
        assert!(make_plan(&app, 0, release(vec![f.plan.asset.clone()])).is_err());
        let link = f.dir.path().join("link");
        std::os::unix::fs::symlink(&f.plan.destination, &link)?;
        assert!(current_hash(&link).is_err());
        let wrong_binary = f.dir.path().join("wrong");
        let mut binary = f.binary.clone();
        binary[18] = 0;
        fs::write(&wrong_binary, &binary)?;
        let mut plan = f.plan.clone();
        plan.asset.digest = None;
        assert!(verify_binary(&wrong_binary, &plan).is_err());
        Ok(())
    }

    #[test]
    fn appimage_requires_type_two_magic_and_records_exact_authority() -> Result<()> {
        let mut f = fixture()?;
        let mut doc = f.ledger.document();
        doc["apps"][0]["recipe"]["installer"] = value("appimage");
        doc["apps"][0]["recipe"]["asset"] = value("tool-{version}.AppImage");
        fs::write(&f.ledger.path, doc.to_string())?;
        f.ledger = Ledger::open(&f.ledger.path)?;

        let mut image = f.binary.clone();
        image[8..12].copy_from_slice(b"AI\x02\0");
        let asset = Asset {
            name: "tool-2.AppImage".into(),
            api_url: "https://api.github.com/repos/example/tool/releases/assets/456".into(),
            size: image.len() as u64,
            digest: Some(format!("sha256:{:x}", Sha256::digest(&image))),
        };
        let release = || Release {
            tag_name: "v2".into(),
            is_prerelease: false,
            assets: vec![asset.clone()],
        };
        let plan = make_plan(&f.ledger.apps[0], 0, release())?;

        let ordinary_elf = f.dir.path().join("ordinary-elf");
        fs::write(&ordinary_elf, &f.binary)?;
        assert!(verify_appimage(&ordinary_elf, &plan).is_err());

        apply_in(
            &mut f.ledger,
            &plan,
            &f.dir.path().join("downloads"),
            &|_| {},
            |_, path| {
                fs::write(path, &image)?;
                Ok(())
            },
        )?;
        assert_eq!(fs::read(&plan.destination)?, image);
        assert_eq!(f.ledger.apps[0].provenance.installer.as_deref(), Some("appimage"));
        assert_eq!(f.ledger.apps[0].provenance.appimage_type, Some(2));
        assert!(f.ledger.record(0).contains("appimage_type = 2"));
        assert!(make_plan(&f.ledger.apps[0], 0, release())?.up_to_date);
        Ok(())
    }

    #[test]
    fn appdir_install_records_and_rechecks_the_complete_tree() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let destination = dir.path().join("managed-AppDir");
        let ledger_path = dir.path().join("ledger.toml");
        fs::write(
            &ledger_path,
            format!(r#"schema_version = 1
[[apps]]
identity = "https://github.com/example/tool"
name = "tool"
category = "Tools"
disposition = "considering"
[apps.recipe]
source = "github"
repo = "example/tool"
asset = "tool-{{version}}.AppImage"
installer = "appimage-appdir"
destination = {destination:?}
os = "linux"
arch = {arch:?}
disable_libs = ["wayland"]
"#,
                destination = destination.to_string_lossy(),
                arch = std::env::consts::ARCH,
            ),
        )?;
        let mut image = vec![0; 64];
        image[..6].copy_from_slice(b"\x7fELF\x02\x01");
        image[8..12].copy_from_slice(b"AI\x02\0");
        image[16] = 2;
        image[18] = if std::env::consts::ARCH == "x86_64" { 62 } else { 183 };
        let asset = Asset {
            name: "tool-1.AppImage".into(),
            api_url: "https://api.github.com/repos/example/tool/releases/assets/789".into(),
            size: image.len() as u64,
            digest: Some(format!("sha256:{:x}", Sha256::digest(&image))),
        };
        let release = || Release {
            tag_name: "v1".into(),
            is_prerelease: false,
            assets: vec![asset.clone()],
        };
        let mut ledger = Ledger::open(&ledger_path)?;
        let plan = make_plan(&ledger.apps[0], 0, release())?;
        apply_appdir_in(
            &mut ledger,
            &plan,
            &dir.path().join("downloads"),
            &|_| {},
            |_, path| {
                fs::write(path, &image)?;
                Ok(())
            },
            |_, prepared, families| {
                assert_eq!(families, ["wayland"]);
                fs::create_dir(prepared)?;
                fs::write(prepared.join("AppRun"), "runner")?;
                fs::create_dir_all(prepared.join("usr/lib/disabled-wayland"))?;
                fs::write(
                    prepared.join("usr/lib/disabled-wayland/libwayland.so"),
                    "bundled",
                )?;
                crate::appimage_tree::hash(prepared)
            },
        )?;
        assert!(destination.join("AppRun").is_file());
        let app = &ledger.apps[0];
        assert_eq!(app.provenance.installer.as_deref(), Some("appimage-appdir"));
        assert_eq!(app.provenance.appimage_type, Some(2));
        assert_eq!(app.provenance.disabled_libs, ["wayland"]);
        assert_eq!(app.provenance.installed_paths, [destination.to_string_lossy()]);
        assert!(app.provenance.appdir_sha256.is_some());
        assert!(make_plan(app, 0, release())?.up_to_date);
        fs::write(destination.join("AppRun"), "changed")?;
        assert!(!make_plan(app, 0, release())?.up_to_date);
        Ok(())
    }

    #[test]
    fn filename_cleanup_strips_platform_suffixes_without_eating_application_digits() {
        assert_eq!(clean_filename("ttt-linux-amd64"), "ttt");
        assert_eq!(clean_filename("tool-x86_64-unknown-linux-musl"), "tool");
        assert_eq!(clean_filename("docx2md-linux-x86_64"), "docx2md");
        assert_eq!(clean_filename("7zip-v24.09-linux-amd64"), "7zip-v24.09");
        assert_eq!(clean_filename("linux-tools"), "linux-tools");
        assert_eq!(clean_filename("arm64"), "arm64");
        assert_eq!(clean_filename("tool64"), "tool64");
    }

    #[test]
    fn clean_install_updates_recipe_and_launch_only_after_commit() -> Result<()> {
        let mut f = fixture_named("tool-linux-amd64")?;
        let mut auto_app = f.ledger.apps[0].clone();
        auto_app.recipe.as_mut().unwrap().clean_filename = true;
        let automatic = make_plan(
            &auto_app,
            0,
            Release {
                tag_name: "v2".into(),
                is_prerelease: false,
                assets: vec![f.plan.asset.clone()],
            },
        )?;
        assert_eq!(automatic.destination, f.dir.path().join("tool"));
        let original = f.plan.destination.clone();
        assert!(f.plan.clean_destination()?);
        assert_eq!(f.plan.destination, f.dir.path().join("tool"));
        assert!(
            f.ledger.apps[0]
                .recipe
                .as_ref()
                .unwrap()
                .destination
                .ends_with("tool-linux-amd64")
        );
        apply_in(
            &mut f.ledger,
            &f.plan,
            &f.dir.path().join("downloads"),
            &|_| {},
            |_, path| {
                fs::write(path, &f.binary)?;
                Ok(())
            },
        )?;
        assert_eq!(fs::read(&original)?, b"known working executable");
        assert_eq!(fs::read(&f.plan.destination)?, f.binary);
        assert_eq!(
            expand_path(&f.ledger.apps[0].recipe.as_ref().unwrap().destination),
            f.plan.destination
        );
        assert_eq!(
            expand_path(&f.ledger.apps[0].launch.as_ref().unwrap().program),
            f.plan.destination
        );
        assert_eq!(f.ledger.apps[0].installed_paths.len(), 2);
        assert!(f.ledger.record(0).contains("tool-linux-amd64"));
        Ok(())
    }

    #[test]
    fn cleanup_rejects_existing_target_and_failed_clean_install_keeps_original_recipe() -> Result<()>
    {
        let mut f = fixture_named("tool-linux-amd64")?;
        let target = f.dir.path().join("tool");
        fs::write(&target, "unrelated app")?;
        assert!(f.plan.clean_destination().is_err());
        assert_eq!(fs::read_to_string(&target)?, "unrelated app");
        let mut f = fixture_named("tool-linux-amd64")?;
        let original = fs::read(&f.ledger.path)?;
        f.plan.clean_destination()?;
        let result = apply_in(
            &mut f.ledger,
            &f.plan,
            &f.dir.path().join("downloads"),
            &|_| {},
            |_, path| {
                fs::write(path, b"invalid")?;
                Ok(())
            },
        );
        assert!(result.is_err());
        assert!(!f.plan.destination.exists());
        assert_eq!(fs::read(&f.ledger.path)?, original);
        Ok(())
    }
}
