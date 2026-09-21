use crate::ledger::expand_path;
use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Read,
    os::unix::{ffi::OsStrExt, fs::PermissionsExt},
    path::{Component, Path, PathBuf},
    process::Command,
};

const FAMILIES: &[&str] = &["wayland", "gl", "gtk", "webkit", "fonts"];

pub(crate) fn validate_families(families: &[String]) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    for family in families {
        ensure!(
            FAMILIES.contains(&family.as_str()),
            "Unknown AppImage library family {family}; expected wayland, gl, gtk, webkit or fonts"
        );
        ensure!(seen.insert(family), "Duplicate AppImage library family {family}");
    }
    Ok(())
}

pub(crate) fn destination(raw: &str) -> Result<PathBuf> {
    let path = expand_path(raw);
    ensure!(
        path.is_absolute() && !path.components().any(|c| matches!(c, Component::ParentDir)),
        "AppDir destination must be an absolute path without '..'"
    );
    ensure!(!path.starts_with("/nix/store"), "Nix store paths are not AppDir destinations");
    ensure!(path.file_name().is_some(), "AppDir destination needs a directory name");
    if let Ok(metadata) = fs::symlink_metadata(&path) {
        ensure!(metadata.file_type().is_dir(), "Refusing a symlink or non-directory AppDir destination");
    }
    Ok(path)
}

pub(crate) fn current_hash(path: &Path) -> Result<Option<String>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(metadata.file_type().is_dir(), "Refusing a symlink or non-directory AppDir");
            Ok(Some(hash(path)?))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn hash(root: &Path) -> Result<String> {
    let metadata = fs::symlink_metadata(root)
        .with_context(|| format!("Cannot inspect AppDir {}", root.display()))?;
    ensure!(metadata.file_type().is_dir(), "AppDir root must be one real directory");
    let mut digest = Sha256::new();
    hash_dir(root, root, &mut digest)?;
    Ok(format!("{:x}", digest.finalize()))
}

fn hash_dir(root: &Path, directory: &Path, digest: &mut Sha256) -> Result<()> {
    let mut entries: Vec<_> = fs::read_dir(directory)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by(|a, b| a.file_name().as_bytes().cmp(b.file_name().as_bytes()));
    for entry in entries {
        let path = entry.path();
        let relative = path.strip_prefix(root)?;
        let metadata = fs::symlink_metadata(&path)?;
        let kind = metadata.file_type();
        digest.update((relative.as_os_str().as_bytes().len() as u64).to_le_bytes());
        digest.update(relative.as_os_str().as_bytes());
        digest.update((metadata.permissions().mode() & 0o7777).to_le_bytes());
        if kind.is_dir() {
            digest.update(b"d");
            hash_dir(root, &path, digest)?;
        } else if kind.is_file() {
            digest.update(b"f");
            digest.update(metadata.len().to_le_bytes());
            let mut file = fs::File::open(&path)?;
            let mut buffer = [0; 64 * 1024];
            loop {
                let count = file.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                digest.update(&buffer[..count]);
            }
        } else if kind.is_symlink() {
            digest.update(b"l");
            let target = fs::read_link(&path)?;
            digest.update((target.as_os_str().as_bytes().len() as u64).to_le_bytes());
            digest.update(target.as_os_str().as_bytes());
        } else {
            bail!("AppDir contains unsupported filesystem object: {}", path.display());
        }
    }
    Ok(())
}

pub(crate) fn prepare(
    image: &Path,
    destination: &Path,
    families: &[String],
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<String> {
    validate_families(families)?;
    ensure!(!destination.exists(), "Prepared AppDir destination already exists");
    let output = crate::work::output(
        Command::new("appimage-run").args(["-x"]).arg(destination).arg(image),
        true,
        Some(cancel),
    )
    .context("Cannot extract AppImage with appimage-run")?;
    ensure!(
        output.status.success(),
        "AppImage extraction failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let metadata = fs::symlink_metadata(destination.join("AppRun"))
        .context("Extracted AppDir has no AppRun entry")?;
    ensure!(
        metadata.file_type().is_file() || metadata.file_type().is_symlink(),
        "Extracted AppRun is not a file or symlink"
    );
    for family in families {
        let output = crate::work::output(
            Command::new("appimage-disable-libs").arg(destination).arg(family),
            true,
            Some(cancel),
        )
        .with_context(|| format!("Cannot disable AppImage {family} libraries"))?;
        ensure!(
            output.status.success(),
            "Disabling AppImage {family} libraries failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    hash(destination)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn tree_hash_covers_paths_modes_contents_and_links() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("AppDir");
        fs::create_dir(&root)?;
        fs::write(root.join("AppRun"), "one")?;
        fs::create_dir(root.join("usr"))?;
        symlink("../AppRun", root.join("usr/run"))?;
        let first = hash(&root)?;
        assert_eq!(first, hash(&root)?);
        fs::write(root.join("AppRun"), "two")?;
        assert_ne!(first, hash(&root)?);
        Ok(())
    }

    #[test]
    fn library_families_are_finite_and_unique() {
        assert!(validate_families(&["wayland".into(), "gl".into()]).is_ok());
        assert!(validate_families(&["shell".into()]).is_err());
        assert!(validate_families(&["gtk".into(), "gtk".into()]).is_err());
    }
}
