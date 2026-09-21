use crate::{ledger::Ledger, update};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::process::Command;

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    size: u64,
}

fn api_url(repo: &str) -> Result<String> {
    let parts: Vec<_> = repo.split('/').collect();
    ensure!(
        parts.len() == 2 && parts.iter().all(|part| update::safe_name(part)),
        "Codeberg recipe repo must be an owner/repository path"
    );
    Ok(format!(
        "https://codeberg.org/api/v1/repos/{repo}/releases/latest"
    ))
}

fn latest(repo: &str, cancel: &std::sync::atomic::AtomicBool) -> Result<Release> {
    let output = crate::work::output(
        Command::new("curl").args([
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            &api_url(repo)?,
        ]),
        true,
        Some(cancel),
    )
    .context("Cannot query Codeberg release API with curl")?;
    ensure!(
        output.status.success(),
        "Codeberg release lookup failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    serde_json::from_slice(&output.stdout).context("Unexpected Codeberg release JSON")
}

fn make_plan(ledger: &Ledger, index: usize, release: Release) -> Result<update::Plan> {
    ensure!(!release.draft, "Draft Codeberg releases are not selected");
    ensure!(
        !release.prerelease,
        "Prerelease Codeberg releases are not selected"
    );
    ensure!(!release.tag_name.is_empty(), "Codeberg release has no tag");
    let recipe = ledger.apps[index]
        .recipe
        .as_ref()
        .context("No update recipe recorded for this app")?;
    ensure!(
        recipe.source == "codeberg" && update::supported_installer(&recipe.installer),
        "Codeberg currently supports exact direct-artifact recipes"
    );
    let expected = update::substitute(&recipe.asset, &release.tag_name);
    let matches: Vec<_> = release
        .assets
        .into_iter()
        .filter(|asset| asset.name == expected)
        .collect();
    ensure!(
        matches.len() == 1,
        "Expected exactly one Codeberg asset named {}; found {}",
        expected,
        matches.len()
    );
    let asset = matches.into_iter().next().unwrap();
    let expected_url = format!(
        "https://codeberg.org/{}/releases/download/{}/{}",
        recipe.repo, release.tag_name, asset.name
    );
    ensure!(
        asset.browser_download_url == expected_url,
        "Codeberg asset URL does not match the recipe repository, release, and filename"
    );
    update::make_plan(
        &ledger.apps[index],
        index,
        update::Release {
            tag_name: release.tag_name,
            is_prerelease: false,
            assets: vec![update::Asset {
                name: asset.name,
                api_url: asset.browser_download_url,
                size: asset.size,
                digest: None,
            }],
        },
    )
}

pub(crate) fn check(
    ledger: &Ledger,
    index: usize,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<update::Plan> {
    ledger.check_unchanged()?;
    let recipe = ledger.apps[index]
        .recipe
        .as_ref()
        .context("No update recipe recorded for this app")?;
    make_plan(ledger, index, latest(&recipe.repo, cancel)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn exact_codeberg_asset_becomes_a_binary_plan() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("ledger.toml");
        fs::write(
            &path,
            format!(
                r#"schema_version = 1
[[apps]]
identity = "https://codeberg.org/Nifou/fibbo"
name = "fibbo"
category = "PKM"
disposition = "considering"
[apps.recipe]
source = "codeberg"
repo = "Nifou/fibbo"
asset = "fibbo_desktop_linux_x86_64_v{{version_underscores}}"
installer = "binary-copy"
destination = {destination:?}
os = "linux"
arch = "x86_64"
"#,
                destination = dir.path().join("fibbo").to_string_lossy(),
            ),
        )?;
        let ledger = Ledger::open(&path)?;
        let release: Release = serde_json::from_str(
            r#"{
                "tag_name":"v0.7.1",
                "draft":false,
                "prerelease":false,
                "assets":[
                    {"name":"fibbo_android_armv8_v0_7_1.apk","size":22262533,"browser_download_url":"https://codeberg.org/Nifou/fibbo/releases/download/v0.7.1/fibbo_android_armv8_v0_7_1.apk"},
                    {"name":"fibbo_desktop_linux_x86_64_v0_7_1","size":21914072,"browser_download_url":"https://codeberg.org/Nifou/fibbo/releases/download/v0.7.1/fibbo_desktop_linux_x86_64_v0_7_1"}
                ]
            }"#,
        )?;
        let plan = make_plan(&ledger, 0, release)?;
        assert_eq!(plan.release, "v0.7.1");
        assert_eq!(plan.asset.name, "fibbo_desktop_linux_x86_64_v0_7_1");
        assert_eq!(plan.asset.size, 21_914_072);
        assert!(plan.asset.digest.is_none());
        Ok(())
    }

    #[test]
    fn codeberg_asset_url_must_belong_to_the_exact_release() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("ledger.toml");
        fs::write(
            &path,
            format!(
                "schema_version = 1\n[[apps]]\nidentity = 'name:tool'\nname = 'tool'\ncategory = 'Tools'\ndisposition = 'considering'\n[apps.recipe]\nsource = 'codeberg'\nrepo = 'owner/tool'\nasset = 'tool'\ninstaller = 'binary-copy'\ndestination = {:?}\nos = 'linux'\narch = 'x86_64'\n",
                dir.path().join("tool").to_string_lossy()
            ),
        )?;
        let ledger = Ledger::open(&path)?;
        let release: Release = serde_json::from_str(
            r#"{"tag_name":"v1","assets":[{"name":"tool","size":64,"browser_download_url":"https://example.com/tool"}]}"#,
        )?;
        assert!(make_plan(&ledger, 0, release).is_err());
        Ok(())
    }
}
