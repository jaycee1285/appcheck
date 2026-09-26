use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    io::Write,
    path::{Path, PathBuf},
};
use toml_edit::{DocumentMut, Item, Table, value};

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Disposition {
    Using,
    Considering,
    Archived,
}

impl Disposition {
    pub fn label(self) -> &'static str {
        match self {
            Self::Using => "using",
            Self::Considering => "considering",
            Self::Archived => "archived",
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct App {
    pub identity: String,
    pub name: String,
    pub category: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub disposition: Disposition,
    pub installed: Option<bool>,
    #[serde(default = "unknown")]
    pub outcome: String,
    pub version: Option<String>,
    #[serde(default)]
    pub installed_paths: Vec<String>,
    #[serde(default)]
    pub review: String,
    #[serde(default)]
    pub archived_because: String,
    #[serde(default = "unknown_provenance")]
    pub provenance: Provenance,
    pub launch: Option<Launch>,
    pub recipe: Option<crate::update::Recipe>,
    #[serde(default)]
    pub installations: Vec<Installation>,
    pub nix_migration: Option<NixMigration>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct NixMigration {
    #[serde(default)]
    pub package: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub homepage: String,
    #[serde(default)]
    pub drv_path: String,
    #[serde(default)]
    pub config_file: String,
    #[serde(default)]
    pub config_expression: String,
    pub migrated_at_unix: Option<i64>,
    pub removed_at_unix: Option<i64>,
    pub realized_path: Option<String>,
}

impl NixMigration {
    pub fn active(&self) -> bool {
        self.removed_at_unix.is_none()
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Installation {
    #[serde(default = "unknown")]
    pub source: String,
    pub installer: Option<String>,
    pub package: Option<String>,
    pub version: Option<String>,
    #[serde(default)]
    pub installed_paths: Vec<String>,
    #[serde(default)]
    pub preferred: bool,
    pub config_file: Option<String>,
    pub config_line: Option<usize>,
    pub config_expression: Option<String>,
    pub config_removed_at_unix: Option<i64>,
}

fn unknown() -> String {
    "unknown".into()
}

fn unknown_provenance() -> Provenance {
    Provenance {
        source: unknown(),
        ..Provenance::default()
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Provenance {
    #[serde(default = "unknown")]
    pub source: String,
    pub repo: Option<String>,
    pub installer: Option<String>,
    pub package: Option<String>,
    pub registry: Option<String>,
    pub root: Option<String>,
    pub remote: Option<String>,
    pub remote_url: Option<String>,
    pub installation: Option<String>,
    pub arch: Option<String>,
    pub branch: Option<String>,
    pub commit: Option<String>,
    pub removed_at_unix: Option<i64>,
    pub config_file: Option<String>,
    pub config_line: Option<usize>,
    pub config_expression: Option<String>,
    pub config_removed_at_unix: Option<i64>,
    #[serde(default)]
    pub managed_by_apptrack: bool,
    pub release: Option<String>,
    pub sha256: Option<String>,
    pub asset_sha256: Option<String>,
    pub asset: Option<String>,
    pub member: Option<String>,
    pub appimage_type: Option<u8>,
    pub appdir_sha256: Option<String>,
    #[serde(default)]
    pub disabled_libs: Vec<String>,
    #[serde(default)]
    pub installed_paths: Vec<String>,
    #[serde(default)]
    pub bin_sha256: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, serde::Serialize)]
pub struct Launch {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub gui: bool,
}

#[derive(Deserialize)]
struct Data {
    schema_version: u32,
    #[serde(default)]
    apps: Vec<App>,
}

#[derive(Clone)]
pub struct Ledger {
    pub path: PathBuf,
    pub apps: Vec<App>,
    doc: DocumentMut,
    original: String,
}

pub fn expand_path(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home_dir) = std::env::var_os("HOME") {
            return PathBuf::from(home_dir).join(rest);
        }
    }
    PathBuf::from(path)
}

pub fn normalize_identity(identity: &str) -> String {
    let trimmed = identity
        .trim()
        .trim_end_matches('/')
        .trim_end_matches(".git");
    if let Some(repo) = trimmed
        .strip_prefix("https://github.com/")
        .or_else(|| trimmed.strip_prefix("http://github.com/"))
    {
        format!("https://github.com/{}", repo.to_lowercase())
    } else {
        trimmed.to_string()
    }
}

fn parse(text: &str) -> Result<(DocumentMut, Vec<App>)> {
    let data: Data = toml::from_str(text).context("Invalid ledger TOML")?;
    ensure!(
        data.schema_version == 1,
        "Unsupported schema_version {}",
        data.schema_version
    );
    let mut identities = HashSet::new();
    for app in &data.apps {
        ensure!(
            !app.name.trim().is_empty()
                && !app.category.trim().is_empty()
                && !app.identity.trim().is_empty(),
            "Each app needs identity, name and category"
        );
        ensure!(
            identities.insert(normalize_identity(&app.identity)),
            "Duplicate logical application: {}",
            app.identity
        );
    }
    Ok((text.parse()?, data.apps))
}

impl Ledger {
    pub fn open(path: &Path) -> Result<Self> {
        let path = fs::canonicalize(path).with_context(|| {
            format!(
                "Cannot open {}. Pass --file /path/to/apptrack.toml",
                path.display()
            )
        })?;
        let original = fs::read_to_string(&path)?;
        let (doc, apps) = parse(&original)?;
        Ok(Self {
            path,
            apps,
            doc,
            original,
        })
    }

    /// Wait this long for a contended ledger before giving up. Bounded rather
    /// than blocking: brief overlap should cost milliseconds, but a genuinely
    /// stuck holder must surface as an error instead of hanging the TUI.
    const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

    pub(crate) fn write_lock(&self) -> Result<fs::File> {
        let lock_path = self.path.with_extension("toml.lock");
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(lock_path)?;
        let deadline = std::time::Instant::now() + Self::LOCK_WAIT;
        loop {
            match lock.try_lock() {
                Ok(()) => break,
                Err(error) if std::time::Instant::now() < deadline => {
                    let _ = error;
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(error) => {
                    return Err(anyhow::Error::new(error))
                        .context("Another AppTrack process is saving this ledger");
                }
            }
        }
        self.check_unchanged()?;
        Ok(lock)
    }

    pub(crate) fn check_unchanged(&self) -> Result<()> {
        ensure!(
            fs::read_to_string(&self.path)? == self.original,
            "Ledger changed on disk. Press r to reload before editing; your external changes were preserved."
        );
        Ok(())
    }

    fn save(&mut self, doc: DocumentMut) -> Result<()> {
        let _lock = self.write_lock()?;
        self.save_locked(doc)
    }

    // Caller holds write_lock for the full executable/ledger commit.
    pub(crate) fn save_locked(&mut self, doc: DocumentMut) -> Result<()> {
        let text = doc.to_string();
        let (_, apps) = parse(&text)?;
        self.check_unchanged()?;
        let mut temp = tempfile::NamedTempFile::new_in(self.path.parent().unwrap())?;
        temp.as_file()
            .set_permissions(fs::metadata(&self.path)?.permissions())?;
        temp.write_all(text.as_bytes())?;
        temp.as_file().sync_all()?;
        temp.persist(&self.path)
            .context("Could not atomically replace ledger")?;
        self.doc = doc;
        self.apps = apps;
        self.original = text;
        Ok(())
    }

    pub(crate) fn document(&self) -> DocumentMut {
        self.doc.clone()
    }

    /// Two-strike rule: one transient network or forge hiccup is noise, a second
    /// identical failure is a problem worth reading. Only the last two are kept.
    pub const FAILURES_KEPT: usize = 2;

    /// `kind` is the attempted action, such as "check", "update", or "remove".
    pub fn record_failure(&mut self, index: usize, kind: &str, error: &str) -> Result<()> {
        let mut doc = self.doc.clone();
        let app = doc["apps"]
            .as_array_of_tables_mut()
            .unwrap()
            .get_mut(index)
            .unwrap();
        let mut action = Table::new();
        action["action"] = value(kind);
        action["result"] = value("failed");
        action["at_unix"] = value(crate::update::timestamp());
        action["detail"] = value(error);
        app["last_action"] = Item::Table(action.clone());
        if app.get("failure_history").is_none() {
            app["failure_history"] = Item::ArrayOfTables(toml_edit::ArrayOfTables::new());
        }
        let history = app["failure_history"]
            .as_array_of_tables_mut()
            .context("failure_history must use [[apps.failure_history]] tables")?;
        history.push(action);
        while history.len() > Self::FAILURES_KEPT {
            history.remove(0);
        }
        self.save(doc)
    }

    pub fn decide(
        &mut self,
        index: usize,
        disposition: Disposition,
        review: Option<&str>,
    ) -> Result<()> {
        ensure!(index < self.apps.len(), "Application no longer exists");
        if disposition == Disposition::Archived {
            ensure!(
                review.is_some_and(|s| !s.trim().is_empty()),
                "An archive reason is required"
            );
        }
        let mut doc = self.doc.clone();
        let app = doc["apps"]
            .as_array_of_tables_mut()
            .unwrap()
            .get_mut(index)
            .unwrap();
        set_text(app, "disposition", disposition.label());
        if let Some(review) = review {
            set_text(app, "archived_because", review.trim());
        }
        self.save(doc)
    }

    pub fn add(
        &mut self,
        name: &str,
        category: &str,
        upstream: &str,
        description: &str,
        tags: &str,
    ) -> Result<()> {
        let doc = self.add_document(name, category, upstream, description, tags)?;
        self.save(doc)
    }

    pub fn edit_record(
        &mut self,
        index: usize,
        name: &str,
        category: &str,
        upstream: &str,
        description: &str,
        tags: &str,
    ) -> Result<()> {
        ensure!(index < self.apps.len(), "Application no longer exists");
        ensure!(!name.trim().is_empty() && !category.trim().is_empty(), "Name and category are required");
        let old = &self.apps[index];
        let identity = if upstream.trim().is_empty() {
            format!("name:{}", name.trim().to_lowercase().split_whitespace().collect::<Vec<_>>().join("-"))
        } else {
            normalize_identity(upstream)
        };
        ensure!(
            !self.apps.iter().enumerate().any(|(i, app)| i != index && normalize_identity(&app.identity) == identity),
            "Already tracked: {identity}"
        );
        let source_changed = identity != normalize_identity(&old.identity);
        ensure!(
            !source_changed || (old.recipe.is_none()
                && !old.provenance.managed_by_apptrack
                && matches!(old.provenance.source.as_str(), "unknown" | "manual")
                && old.nix_migration.is_none()),
            "This source has reviewed install authority; its identity cannot be changed in the edit form"
        );
        let mut doc = self.document();
        let app = doc["apps"].as_array_of_tables_mut().unwrap().get_mut(index).unwrap();
        set_text(app, "identity", &identity);
        set_text(app, "name", name.trim());
        set_text(app, "category", category.trim());
        set_text(app, "description", description.trim());
        let mut array = toml_edit::Array::new();
        for tag in tags.split(',').map(str::trim).filter(|s| !s.is_empty()) { array.push(tag); }
        app["tags"] = value(array);
        if source_changed {
            if let Some(provenance) = app.get_mut("provenance").and_then(Item::as_table_mut) {
                if identity.starts_with("https://") { set_text(provenance, "repo", &identity); }
                else { provenance.remove("repo"); }
            }
        }
        self.save(doc)
    }

    fn add_document(
        &self,
        name: &str,
        category: &str,
        upstream: &str,
        description: &str,
        tags: &str,
    ) -> Result<DocumentMut> {
        ensure!(
            !name.trim().is_empty() && !category.trim().is_empty(),
            "Name and category are required"
        );
        let identity = if upstream.trim().is_empty() {
            format!(
                "name:{}",
                name.trim()
                    .to_lowercase()
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join("-")
            )
        } else {
            normalize_identity(upstream)
        };
        ensure!(
            !self
                .apps
                .iter()
                .any(|a| normalize_identity(&a.identity) == identity
                    || (upstream.trim().is_empty() && a.name.eq_ignore_ascii_case(name.trim()))),
            "Already tracked: {identity}"
        );
        let mut doc = self.doc.clone();
        let mut table = Table::new();
        table
            .decor_mut()
            .set_prefix("\n\n# Added explicitly — installation/provenance not yet verified.\n");
        table["identity"] = value(&identity);
        table["name"] = value(name.trim());
        table["category"] = value(category.trim());
        table["description"] = value(description.trim());
        table["disposition"] = value("considering");
        table["outcome"] = value("unknown");
        let mut array = toml_edit::Array::new();
        for tag in tags.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            array.push(tag);
        }
        table["tags"] = value(array);
        if identity.starts_with("https://") {
            let mut provenance = Table::new();
            provenance["source"] = value("unknown");
            provenance["repo"] = value(identity.clone());
            provenance["managed_by_apptrack"] = value(false);
            table["provenance"] = Item::Table(provenance);
        }
        if doc.get("apps").is_none()
            || doc
                .get("apps")
                .and_then(Item::as_array)
                .is_some_and(|apps| apps.is_empty())
        {
            doc["apps"] = Item::ArrayOfTables(toml_edit::ArrayOfTables::new());
        }
        let entries = doc["apps"].as_array_of_tables_mut().unwrap();
        let insert_at = self
            .apps
            .iter()
            .rposition(|app| app.category == category.trim())
            .map_or(entries.len(), |i| i + 1);
        let mut grouped = toml_edit::ArrayOfTables::new();
        for (index, entry) in entries.iter().enumerate() {
            if index == insert_at {
                grouped.push(table.clone());
            }
            grouped.push(entry.clone());
        }
        if insert_at == entries.len() {
            grouped.push(table);
        }
        *entries = grouped;
        Ok(doc)
    }

    pub(crate) fn register_recipe(&mut self, app: &App) -> Result<usize> {
        let existing = self
            .apps
            .iter()
            .position(|a| normalize_identity(&a.identity) == normalize_identity(&app.identity));
        let mut doc = if existing.is_some() {
            self.document()
        } else {
            self.add_document(
                &app.name,
                &app.category,
                &app.identity,
                &app.description,
                &app.tags.join(","),
            )?
        };
        let tables = doc["apps"].as_array_of_tables_mut().unwrap();
        let index = tables
            .iter()
            .position(|t| {
                t["identity"]
                    .as_str()
                    .is_some_and(|id| normalize_identity(id) == normalize_identity(&app.identity))
            })
            .context("New record not found")?;
        let table = tables.get_mut(index).unwrap();
        ensure!(
            table.get("recipe").is_none(),
            "Already configured; existing recipe was not changed"
        );
        let recipe = app.recipe.as_ref().context("Missing recipe")?;
        let recipe_doc: DocumentMut = toml::to_string(recipe)?.parse()?;
        table["recipe"] = Item::Table(recipe_doc.as_table().clone());
        // Keep an existing launch working until installation succeeds. Decisions,
        // notes, installed paths and provenance remain untouched here.
        let launch = app.launch.as_ref().context("Missing launch recipe")?;
        let launch_doc: DocumentMut = toml::to_string(launch)?.parse()?;
        if table.get("launch").is_none() {
            table["launch"] = Item::Table(launch_doc.as_table().clone());
        }
        self.save(doc)?;
        Ok(index)
    }

    pub fn find(&self, query: &str) -> Result<usize> {
        let matches: Vec<_> = self
            .apps
            .iter()
            .enumerate()
            .filter(|(_, a)| {
                a.name.eq_ignore_ascii_case(query)
                    || normalize_identity(&a.identity) == normalize_identity(query)
            })
            .collect();
        match matches.as_slice() {
            [(index, _)] => Ok(*index),
            [] => bail!("No app named {query:?}"),
            _ => bail!("Ambiguous app name; use its full identity"),
        }
    }

    pub fn record(&self, index: usize) -> String {
        let table = self.doc["apps"]
            .as_array_of_tables()
            .unwrap()
            .get(index)
            .unwrap();
        let mut doc = DocumentMut::new();
        let mut apps = toml_edit::ArrayOfTables::new();
        apps.push(table.clone());
        doc["apps"] = Item::ArrayOfTables(apps);
        doc.to_string()
    }
}

fn set_text(table: &mut Table, key: &str, text: &str) {
    let mut item = value(text);
    if let Some(old) = table.get(key).and_then(Item::as_value) {
        *item.as_value_mut().unwrap().decor_mut() = old.decor().clone();
    }
    table[key] = item;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_history_keeps_the_last_two_strikes_of_either_kind() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("ledger.toml");
        fs::write(
            &path,
            "schema_version = 1\n[[apps]]\nidentity = 'name:a'\nname = 'a'\ncategory = 'Tools'\ndisposition = 'using'\n",
        )?;
        let mut ledger = Ledger::open(&path)?;
        ledger.record_failure(0, "check", "first network hiccup")?;
        let record = Ledger::open(&path)?.record(0);
        assert_eq!(record.matches("[[apps.failure_history]]").count(), 1);
        assert!(record.contains("first network hiccup"));

        ledger.record_failure(0, "check", "second, same forge")?;
        ledger.record_failure(0, "update", "third, asset gone")?;
        let record = Ledger::open(&path)?.record(0);
        // Two strikes kept, oldest dropped, newest also in last_action.
        assert_eq!(
            record.matches("[[apps.failure_history]]").count(),
            Ledger::FAILURES_KEPT
        );
        assert!(!record.contains("first network hiccup"));
        assert!(record.contains("second, same forge"));
        assert!(record.contains("third, asset gone"));
        assert!(record.contains(r#"action = "update""#));
        assert!(record.contains(r#"action = "check""#));
        Ok(())
    }

    #[test]
    fn competing_writer_cannot_enter_a_held_commit_lock() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("ledger.toml");
        fs::write(&path, "schema_version = 1\napps = []\n")?;
        let ledger = Ledger::open(&path)?;
        let lock = ledger.write_lock()?;
        let competing_path = path.clone();
        std::thread::spawn(move || {
            let mut competing = Ledger::open(&competing_path).unwrap();
            assert!(competing.add("tool", "Tools", "", "", "").is_err());
        })
        .join()
        .unwrap();
        assert!(Ledger::open(&path)?.apps.is_empty());
        drop(lock);
        Ledger::open(&path)?.add("tool", "Tools", "", "", "")?;
        assert_eq!(Ledger::open(&path)?.apps.len(), 1);
        Ok(())
    }
    const FIXTURE: &str = "schema_version = 1\n\n[[inbox.nix]]\nnote = 'an incomplete Nix thought'\n\n[[inbox.appimage]]\nname = 'rough AppImage note'\n\n# Keep this human note\n[[apps]]\nidentity = 'name:foo'\nname = 'foo'\ncategory = 'Editing'\ndisposition = 'considering' # Keep inline note too\nfuture_field = 'retain me'\n";

    #[test]
    fn editing_imported_source_preserves_evidence_and_refuses_managed_identity_change() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("ledger.toml");
        fs::write(&path, format!("{FIXTURE}\n[apps.provenance]\nsource = 'unknown'\nmanaged_by_apptrack = false\n\n[apps.evidence]\nimported_from = 'notes.md'\n"))?;
        let mut ledger = Ledger::open(&path)?;
        ledger.edit_record(0, "foo", "Editing", "https://github.com/owner/foo", "New description", "tui")?;
        let saved = fs::read_to_string(&path)?;
        assert!(saved.contains("imported_from = 'notes.md'"));
        assert!(saved.contains("future_field = 'retain me'"));
        assert!(saved.contains("description = \"New description\""));
        assert_eq!(ledger.apps[0].provenance.source, "unknown");
        assert_eq!(ledger.apps[0].provenance.repo.as_deref(), Some("https://github.com/owner/foo"));
        let unchanged = saved.clone();
        let mut doc = ledger.document();
        doc["apps"].as_array_of_tables_mut().unwrap().get_mut(0).unwrap()["provenance"]["managed_by_apptrack"] = value(true);
        ledger.save(doc)?;
        assert!(ledger.edit_record(0, "foo", "Editing", "https://github.com/other/foo", "New description", "tui").is_err());
        assert_ne!(fs::read_to_string(&path)?, unchanged);
        Ok(())
    }

    #[test]
    fn decisions_preserve_unknown_fields_comments_and_external_edits() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("apps.toml");
        fs::write(&path, FIXTURE)?;
        let mut ledger = Ledger::open(&path)?;
        ledger.decide(0, Disposition::Archived, Some("cannot focus panels"))?;
        let saved = fs::read_to_string(&path)?;
        assert!(saved.contains("# Keep this human note"));
        assert!(saved.contains("# Keep inline note too"));
        assert!(saved.contains("future_field = 'retain me'"));
        assert!(saved.contains("[[inbox.nix]]"));
        assert!(saved.contains("an incomplete Nix thought"));
        assert!(saved.contains("[[inbox.appimage]]"));
        assert!(saved.contains("rough AppImage note"));
        assert_eq!(
            Ledger::open(&path)?.apps[0].archived_because,
            "cannot focus panels"
        );
        fs::write(&path, format!("{saved}\n# external edit\n"))?;
        assert!(ledger.decide(0, Disposition::Using, None).is_err());
        assert!(fs::read_to_string(&path)?.contains("# external edit"));
        assert_eq!(ledger.apps[0].disposition, Disposition::Archived);
        Ok(())
    }

    #[test]
    fn duplicate_repository_spellings_are_one_identity() {
        assert_eq!(
            normalize_identity("https://github.com/Owner/Repo.git/"),
            normalize_identity("https://github.com/owner/repo")
        );
    }

    #[test]
    fn adding_groups_records_and_leaves_installation_unknown() -> Result<()> {
        let mut file = tempfile::NamedTempFile::new()?;
        write!(
            file,
            "{FIXTURE}\n# Tools\n[[apps]]\nidentity='name:bar'\nname='bar'\ncategory='Tools'\ndisposition='considering'\n"
        )?;
        let mut ledger = Ledger::open(file.path())?;
        ledger.add(
            "new editor",
            "Editing",
            "https://github.com/Owner/Editor.git",
            "Review me",
            "tui, tree",
        )?;
        assert_eq!(
            ledger
                .apps
                .iter()
                .map(|a| a.name.as_str())
                .collect::<Vec<_>>(),
            ["foo", "new editor", "bar"]
        );
        assert_eq!(ledger.apps[1].installed, None);
        assert_eq!(ledger.apps[1].provenance.source, "unknown");
        assert_eq!(ledger.apps[1].provenance.repo.as_deref(), Some("https://github.com/owner/editor"));
        assert!(!ledger.apps[1].provenance.managed_by_apptrack);
        assert!(
            ledger
                .add(
                    "other name",
                    "Editing",
                    "https://github.com/owner/editor/",
                    "",
                    ""
                )
                .is_err()
        );
        assert!(fs::read_to_string(file.path())?.contains("# Tools"));
        Ok(())
    }

    #[test]
    fn archived_apps_only_require_display_identity() -> Result<()> {
        let mut file = tempfile::NamedTempFile::new()?;
        write!(
            file,
            "schema_version = 1\n[[apps]]\nidentity = 'name:nope'\nname = 'nope'\ncategory = 'Editing'\ndisposition = 'archived'\narchived_because = 'Too slow and too clever.'\n"
        )?;
        let ledger = Ledger::open(file.path())?;
        let app = &ledger.apps[0];
        assert!(app.description.is_empty());
        assert!(app.tags.is_empty());
        assert_eq!(app.installed, None);
        assert_eq!(app.outcome, "unknown");
        assert_eq!(app.archived_because, "Too slow and too clever.");
        Ok(())
    }

    #[test]
    fn displayed_apps_require_one_explicit_disposition() -> Result<()> {
        let without = "schema_version = 1\n[[apps]]\nidentity='name:a'\nname='a'\ncategory='Tools'\n";
        assert!(parse(without).is_err());

        for (text, expected) in [
            ("using", Disposition::Using),
            ("considering", Disposition::Considering),
            ("archived", Disposition::Archived),
        ] {
            let ledger = format!(
                "schema_version = 1\n[[apps]]\nidentity='name:{text}'\nname='{text}'\ncategory='Tools'\ndisposition='{text}'\n"
            );
            assert_eq!(parse(&ledger)?.1[0].disposition, expected);
        }
        Ok(())
    }
}

#[cfg(test)]
mod lock_probe {
    use super::*;
    #[test]
    fn many_sequential_saves_never_self_contend() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("ledger.toml");
        fs::write(&path, "schema_version = 1\n[[apps]]\nidentity = 'name:a'\nname = 'a'\ncategory = 'Tools'\ndisposition = 'using'\n")?;
        let mut ledger = Ledger::open(&path)?;
        let mut errors = vec![];
        for i in 0..300 {
            if let Err(e) = ledger.record_failure(0, "check", &format!("attempt {i}")) {
                errors.push(format!("{i}: {e:#}"));
            }
        }
        assert!(errors.is_empty(), "{} of 300 saves failed:\n{}", errors.len(), errors.join("\n"));
        Ok(())
    }
}
