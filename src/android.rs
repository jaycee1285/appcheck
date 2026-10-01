use crate::ledger::{Disposition, Ledger};
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use std::{collections::{HashMap, HashSet}, fs, path::{Path, PathBuf}};

#[derive(Clone, Debug)]
pub struct Listing {
    pub identity: String,
    pub name: String,
    pub categories: Vec<String>,
    pub disposition: Option<Disposition>,
    pub description: String,
    pub archived_because: String,
    pub author: String,
    pub url: String,
    pub installed_version: String,
    pub latest_version: String,
    pub note_index: Option<usize>,
    pub unlisted: bool,
}

#[derive(Clone, Debug)]
pub struct Catalog {
    pub file: PathBuf,
    pub apps: Vec<Listing>,
}

#[derive(Deserialize)]
struct Export {
    apps: Vec<ExportApp>,
}

#[derive(Deserialize)]
struct ExportApp {
    id: String,
    name: String,
    #[serde(default)]
    author: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    categories: Vec<String>,
    #[serde(rename = "installedVersion")]
    installed_version: Option<String>,
    #[serde(rename = "latestVersion")]
    latest_version: Option<String>,
}

fn newest(dir: &Path) -> Result<Option<PathBuf>> {
    let mut obtainx = Vec::new();
    let mut obtainium = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() { continue; }
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.ends_with(".json") { continue; }
        if name.starts_with("obtainx-export-") { obtainx.push(entry.path()); }
        else if name.starts_with("obtainium-export-") { obtainium.push(entry.path()); }
    }
    obtainx.sort();
    obtainium.sort();
    Ok(obtainx.pop().or_else(|| obtainium.pop()))
}

impl Catalog {
    pub fn load(ledger: &Ledger) -> Result<Self> {
        if let Ok(path) = std::env::var("APPTRACK_ANDROID_EXPORT") {
            return Self::from_export(ledger, Path::new(&path));
        }
        let near = ledger.path.parent().and_then(Path::parent).context("Ledger has no parent folder")?;
        let home = std::env::var("HOME").map(PathBuf::from).unwrap_or_default().join("syncthing");
        let file = match newest(near)? {
            Some(file) => file,
            None => newest(&home)?
                .context("No obtainx-export-*.json or obtainium-export-*.json found; set APPTRACK_ANDROID_EXPORT")?,
        };
        Self::from_export(ledger, &file)
    }

    pub fn from_export(ledger: &Ledger, file: &Path) -> Result<Self> {
        let text = fs::read_to_string(file).with_context(|| format!("Cannot read Android export {}", file.display()))?;
        let mut export: Export = serde_json::from_str(&text).with_context(|| format!("Invalid Android export {}", file.display()))?;
        let import_path = file.parent().context("Android export has no parent folder")?.join("obtainium-import.json");
        if import_path.is_file() {
            let pending: Export = serde_json::from_str(&fs::read_to_string(&import_path)?)
                .with_context(|| format!("Invalid pending Android categories {}", import_path.display()))?;
            let by_id: HashMap<_, _> = pending.apps.into_iter().map(|app| (app.id, app.categories)).collect();
            for app in &mut export.apps {
                if let Some(categories) = by_id.get(&app.id) { app.categories = categories.clone(); }
            }
        }
        let notes = ledger.android_records()?;
        let by_id: HashMap<_, _> = notes.iter().enumerate().map(|(i, note)| (note.identity.as_str(), i)).collect();
        let mut seen = HashSet::new();
        let mut apps = Vec::with_capacity(export.apps.len() + notes.len());
        for app in export.apps {
            ensure!(!app.id.trim().is_empty() && !app.name.trim().is_empty(), "Android export has an app without id or name");
            let identity = format!("android:{}", app.id);
            ensure!(seen.insert(identity.clone()), "Android export repeats {identity}");
            let note_index = by_id.get(identity.as_str()).copied();
            let note = note_index.map(|index| &notes[index]);
            apps.push(Listing {
                identity,
                name: app.name,
                categories: app.categories,
                disposition: note.and_then(|note| note.disposition),
                description: note.map_or(String::new(), |note| note.description.clone()),
                archived_because: note.map_or(String::new(), |note| note.archived_because.clone()),
                author: app.author,
                url: app.url,
                installed_version: app.installed_version.unwrap_or_default(),
                latest_version: app.latest_version.unwrap_or_default(),
                note_index,
                unlisted: false,
            });
        }
        for (index, note) in notes.into_iter().enumerate() {
            if seen.contains(&note.identity) { continue; }
            apps.push(Listing {
                identity: note.identity,
                name: note.name,
                categories: note.categories,
                disposition: note.disposition,
                description: note.description,
                archived_because: note.archived_because,
                author: note.author,
                url: String::new(),
                installed_version: note.installed_version,
                latest_version: note.latest_version,
                note_index: Some(index),
                unlisted: true,
            });
        }
        if apps.is_empty() { bail!("Android export has no apps"); }
        Ok(Self { file: file.to_path_buf(), apps })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_rows_overlay_notes_and_keep_unlisted_snapshots() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let ledger_path = dir.path().join("apptrack.toml");
        fs::write(&ledger_path, "schema_version = 1\n[[inbox.android]]\nidentity = 'android:a.app'\nname = 'A'\ndisposition = 'using'\ndescription = 'keep note'\n\n[[inbox.android]]\nidentity = 'android:gone.app'\nname = 'Gone'\ndisplay_categories = '[\"Old category\"]'\ndisposition = 'archived'\n")?;
        let export = dir.path().join("obtainx-export-test.json");
        fs::write(&export, r#"{"apps":[{"id":"a.app","name":"A current","categories":["Tools"],"installedVersion":"1"},{"id":"b.app","name":"B","categories":["Tools","Daily"]}]}"#)?;
        let mut ledger = Ledger::open(&ledger_path)?;
        let catalog = Catalog::from_export(&ledger, &export)?;
        assert_eq!(catalog.apps.len(), 3);
        assert_eq!(catalog.apps[0].description, "keep note");
        assert_eq!(catalog.apps[0].disposition, Some(Disposition::Using));
        assert_eq!(catalog.apps[1].categories, ["Tools", "Daily"]);
        assert!(catalog.apps[1].note_index.is_none());
        assert!(catalog.apps[2].unlisted);
        assert_eq!(catalog.apps[2].categories, ["Old category"]);

        let b = &catalog.apps[1];
        ledger.add_android(&crate::ledger::AndroidRecord {
            identity: b.identity.clone(), name: b.name.clone(), categories: b.categories.clone(),
            has_category_snapshot: true, disposition: Some(Disposition::Considering),
            description: "new note".into(), archived_because: String::new(), author: String::new(),
            installed_version: String::new(), latest_version: String::new(),
            imported_from: "obtainx-export-test.json".into(), observed_on: "2026-09-30".into(),
        })?;
        let reopened = Catalog::from_export(&Ledger::open(&ledger_path)?, &export)?;
        assert_eq!(reopened.apps[1].description, "new note");
        assert_eq!(reopened.apps[1].disposition, Some(Disposition::Considering));
        assert_eq!(reopened.apps[2].name, "Gone");
        fs::write(dir.path().join("obtainium-import.json"), r#"{"apps":[{"id":"b.app","name":"B","categories":["Pending"]}]}"#)?;
        let pending = Catalog::from_export(&Ledger::open(&ledger_path)?, &export)?;
        assert_eq!(pending.apps[1].categories, ["Pending"]);
        Ok(())
    }

    #[test]
    fn obtainx_export_wins_when_both_export_types_exist() -> Result<()> {
        let dir = tempfile::tempdir()?;
        fs::write(dir.path().join("obtainx-export-older.json"), "{}")?;
        fs::write(dir.path().join("obtainium-export-newer.json"), "{}")?;
        assert_eq!(newest(dir.path())?.unwrap().file_name().unwrap(), "obtainx-export-older.json");
        Ok(())
    }

    #[test]
    fn supplied_export_projects_without_live_ledger_writes() -> Result<()> {
        let Ok(file) = std::env::var("APPTRACK_ANDROID_EXPORT") else { return Ok(()); };
        let dir = tempfile::tempdir()?;
        let ledger_path = if let Ok(path) = std::env::var("APPTRACK_ANDROID_LEDGER") {
            PathBuf::from(path)
        } else {
            let path = dir.path().join("apptrack.toml");
            fs::write(&path, "schema_version = 1\n")?;
            path
        };
        let catalog = Catalog::from_export(&Ledger::open(&ledger_path)?, Path::new(&file))?;
        let source: Export = serde_json::from_str(&fs::read_to_string(&file)?)?;
        assert_eq!(catalog.apps.iter().filter(|app| !app.unlisted).count(), source.apps.len());
        println!("Projected {} listed + {} unlisted Android apps from {}", source.apps.len(), catalog.apps.len() - source.apps.len(), catalog.file.display());
        Ok(())
    }
}
