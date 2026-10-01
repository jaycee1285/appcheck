use crate::{ledger::Ledger, update};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::process::Command;

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    upcoming_release: bool,
    assets: Assets,
}

#[derive(Debug, Deserialize)]
struct Assets {
    #[serde(default)]
    links: Vec<Link>,
}

#[derive(Debug, Deserialize)]
struct Link {
    name: String,
    direct_asset_url: String,
}

fn api_url(repo: &str) -> Result<String> {
    let parts: Vec<_> = repo.split('/').collect();
    ensure!(
        parts.len() >= 2 && parts.iter().all(|part| update::safe_name(part)),
        "GitLab recipe repo must be a namespace/project path"
    );
    Ok(format!(
        "https://gitlab.com/api/v4/projects/{}/releases/permalink/latest",
        parts.join("%2F")
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
    .context("Cannot query GitLab release API with curl")?;
    ensure!(
        output.status.success(),
        "GitLab release lookup failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    serde_json::from_slice(&output.stdout).context("Unexpected GitLab release JSON")
}

pub(crate) fn inspect_release(repo: &str, cancel: &std::sync::atomic::AtomicBool) -> Result<update::Release> {
    let release = latest(repo, cancel)?;
    ensure!(!release.upcoming_release, "Latest GitLab release is upcoming");
    ensure!(release.assets.links.len() <= 32, "Too many GitLab asset links for an interactive release review");
    let mut assets = Vec::new();
    for link in release.assets.links {
        if !link.direct_asset_url.starts_with("https://") { continue; }
        let size = content_length(&link.direct_asset_url, cancel)?;
        assets.push(update::Asset { name: link.name, api_url: link.direct_asset_url, size, digest: None });
    }
    Ok(update::Release { tag_name: release.tag_name, is_prerelease: false, assets })
}

fn content_length(url: &str, cancel: &std::sync::atomic::AtomicBool) -> Result<u64> {
    let output = crate::work::output(
        Command::new("curl").args([
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            "--head",
            url,
        ]),
        true,
        Some(cancel),
    )
    .context("Cannot inspect GitLab release asset with curl")?;
    ensure!(
        output.status.success(),
        "GitLab asset inspection failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    parse_content_length(&String::from_utf8_lossy(&output.stdout))
}

fn parse_content_length(headers: &str) -> Result<u64> {
    let length = headers.lines().rev().find_map(|line| {
        let (name, value) = line.trim_end_matches('\r').split_once(':')?;
        name.eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse::<u64>().ok())
            .flatten()
    });
    let length = length.context("GitLab asset did not report Content-Length")?;
    ensure!(length >= 64, "GitLab release asset is too small to contain an executable");
    Ok(length)
}

fn make_plan(
    ledger: &Ledger,
    index: usize,
    release: Release,
    asset_size: impl FnOnce(&str) -> Result<u64>,
) -> Result<update::Plan> {
    ensure!(!release.upcoming_release, "Upcoming GitLab releases are not selected");
    ensure!(!release.tag_name.is_empty(), "GitLab release has no tag");
    let recipe = ledger.apps[index]
        .recipe
        .as_ref()
        .context("No update recipe recorded for this app")?;
    ensure!(
        recipe.source == "gitlab"
            && matches!(recipe.installer.as_str(), "binary-copy" | "appimage" | "appimage-appdir"),
        "GitLab currently supports exact binary/AppImage/AppDir recipes"
    );
    let expected = update::substitute(&recipe.asset, &release.tag_name);
    let matches: Vec<_> = release
        .assets
        .links
        .into_iter()
        .filter(|link| link.name == expected)
        .collect();
    ensure!(
        matches.len() == 1,
        "Expected exactly one GitLab asset named {}; found {}",
        expected,
        matches.len()
    );
    let link = matches.into_iter().next().unwrap();
    ensure!(
        link.direct_asset_url.starts_with("https://")
            && !link.direct_asset_url.bytes().any(|byte| byte.is_ascii_whitespace()),
        "GitLab asset URL must be one HTTPS URL"
    );
    let size = asset_size(&link.direct_asset_url)?;
    update::make_plan(
        &ledger.apps[index],
        index,
        update::Release {
            tag_name: release.tag_name,
            is_prerelease: false,
            assets: vec![update::Asset {
                name: link.name,
                api_url: link.direct_asset_url,
                size,
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
    let release = latest(&recipe.repo, cancel)?;
    make_plan(ledger, index, release, |url| content_length(url, cancel))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn exact_gitlab_link_becomes_an_appimage_plan() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("ledger.toml");
        fs::write(
            &path,
            format!(r#"schema_version = 1
[[apps]]
identity = "https://gitlab.com/ArkHost/HelixNotes"
name = "helixnotes"
category = "PKM"
disposition = "considering"
[apps.recipe]
source = "gitlab"
repo = "ArkHost/HelixNotes"
asset = "HelixNotes_{{version}}_amd64.AppImage"
installer = "appimage"
destination = {destination:?}
os = "linux"
arch = "x86_64"
"#,
                destination = dir.path().join("helixnotes").to_string_lossy(),
            ),
        )?;
        let ledger = Ledger::open(&path)?;
        let release: Release = serde_json::from_str(r#"{
            "tag_name":"v1.3.5",
            "upcoming_release":false,
            "assets":{"links":[
                {"name":"other.AppImage","direct_asset_url":"https://example.com/other"},
                {"name":"HelixNotes_1.3.5_amd64.AppImage","direct_asset_url":"https://download.helixnotes.com/releases/v1.3.5/HelixNotes_1.3.5_amd64.AppImage"}
            ]}
        }"#)?;
        let plan = make_plan(&ledger, 0, release, |_| Ok(114_743_800))?;
        assert_eq!(plan.release, "v1.3.5");
        assert_eq!(plan.asset.name, "HelixNotes_1.3.5_amd64.AppImage");
        assert_eq!(plan.asset.size, 114_743_800);
        assert!(plan.asset.digest.is_none());
        Ok(())
    }

    #[test]
    fn exact_gitlab_link_becomes_a_direct_binary_plan() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("ledger.toml");
        fs::write(&path, format!(r#"schema_version = 1
[[apps]]
identity = "https://gitlab.com/paskidev/gitorii"
name = "torii"
category = "Git"
disposition = "considering"
[apps.recipe]
source = "gitlab"
repo = "paskidev/gitorii"
asset = "torii-linux-x86_64"
installer = "binary-copy"
destination = {destination:?}
os = "linux"
arch = "x86_64"
"#, destination = dir.path().join("torii").to_string_lossy()))?;
        let ledger = Ledger::open(&path)?;
        let release: Release = serde_json::from_str(r#"{
            "tag_name":"v0.7.15", "assets":{"links":[
                {"name":"torii-linux-aarch64","direct_asset_url":"https://gitlab.com/other"},
                {"name":"torii-linux-x86_64","direct_asset_url":"https://gitlab.com/api/v4/projects/81184073/packages/generic/gitorii/v0.7.15/torii-linux-x86_64"}
            ]}
        }"#)?;
        let plan = make_plan(&ledger, 0, release, |_| Ok(8_000_000))?;
        assert_eq!(plan.asset.name, "torii-linux-x86_64");
        assert_eq!(plan.recipe.installer, "binary-copy");
        Ok(())
    }

    #[test]
    fn header_parser_uses_the_final_redirect_length() -> Result<()> {
        let headers = "HTTP/2 302\r\ncontent-length: 42\r\n\r\nHTTP/2 200\r\nContent-Length: 114743800\r\n";
        assert_eq!(parse_content_length(headers)?, 114_743_800);
        assert!(parse_content_length("HTTP/2 200\r\n").is_err());
        Ok(())
    }
}
