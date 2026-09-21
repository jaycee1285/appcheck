use crate::{
    dialog::Dialog,
    ledger::{Ledger, expand_path},
    nix_discovery::{self, Classification, Selection},
    nix_strategy,
    update::timestamp,
};
use anyhow::{Context, Result, bail, ensure};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::AtomicBool,
};
use toml_edit::{Item, Table, value};

const CANONICAL_TRACKED: &str = "{ pkgs, ... }:\n# AppTrack-managed Home Manager packages. AppTrack is the only editor of this\n# file; every entry was individually confirmed and collision-checked.\n{\n  home.packages = with pkgs; [\n  ];\n}\n";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationPlan {
    pub index: usize,
    pub name: String,
    identity: String,
    expression: String,
    version: String,
    homepage: String,
    drv_path: String,
    main_program: String,
    tracked_text: String,
    tracked_file: PathBuf,
    created: bool,
    original_text: String,
    edited_text: String,
    line: usize,
}

impl MigrationPlan {
    pub fn summary(&self) -> String {
        format!(
            "Add exact Nix package declaration?\n\n{} {} · {}\n→ {}:{}\n\nThis edits only tracked.nix. AppTrack will not rebuild NixOS or Home Manager; the package is not installed until you rebuild. Keeping the declaration out is the default.",
            self.expression,
            if self.version.is_empty() {
                "version unreported"
            } else {
                &self.version
            },
            self.homepage,
            self.tracked_file.display(),
            self.line,
        )
    }
}

fn tracked_path() -> Result<(PathBuf, String)> {
    let raw = std::env::var("APPTRACK_NIX_TRACKED")
        .unwrap_or_else(|_| "~/repos/config/home/tracked.nix".into());
    let path = expand_path(&raw);
    ensure!(
        path.file_name().is_some_and(|name| name == "tracked.nix"),
        "Nix migration target must be tracked.nix"
    );
    if path.exists() {
        return Ok((nix_strategy::config_path(&raw)?, raw));
    }
    ensure!(
        path.is_absolute()
            && !path
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir)),
        "Nix config path must be absolute without '..'"
    );
    let parent = fs::canonicalize(path.parent().context("tracked.nix has no parent directory")?)
        .context("Cannot resolve the tracked.nix parent directory")?;
    let path = parent.join("tracked.nix");
    nix_strategy::ensure_config_target(&path)?;
    Ok((path, raw))
}

fn tracked_edit(original: Option<String>, expression: &str) -> Result<(String, String, usize)> {
    nix_discovery::candidate_path(expression)?;
    let original = original.unwrap_or_else(|| CANONICAL_TRACKED.into());
    ensure!(
        nix_strategy::expression_range(&original, expression)?.is_none(),
        "Nix expression is already declared in tracked.nix"
    );
    let openers: Vec<_> = original
        .lines()
        .enumerate()
        .filter(|(_, line)| line.trim() == "home.packages = with pkgs; [")
        .collect();
    ensure!(
        openers.len() == 1,
        "tracked.nix must hold exactly one 'home.packages = with pkgs; [' list"
    );
    let opener = openers[0].0;
    let mut lines: Vec<String> = original.lines().map(str::to_string).collect();
    let indent = lines[opener]
        .find("home.packages")
        .map(|column| " ".repeat(column + 2))
        .unwrap_or_else(|| "    ".into());
    lines.insert(opener + 1, format!("{indent}{expression}"));
    let mut edited = lines.join("\n");
    if original.ends_with('\n') {
        edited.push('\n');
    }
    let (.., line) = nix_strategy::expression_range(&edited, expression)?
        .context("Nix expression is not a standalone declaration after the proposed edit")?;
    Ok((original, edited, line))
}

pub fn plan(
    ledger: &Ledger,
    index: usize,
    show_timer: bool,
    cancel: Option<&AtomicBool>,
    progress: &dyn Fn(&str),
) -> Result<Option<MigrationPlan>> {
    ledger.check_unchanged()?;
    let (app, selection) =
        nix_discovery::select_candidate(ledger, index, show_timer, cancel, progress)?;
    let selected = match selection {
        Selection::Unavailable => {
            bail!("app: {}\nresult: no normalized candidate attribute in the configured package sets; nothing to migrate", app.name)
        }
        Selection::Ambiguous(lines) => {
            let candidates = if lines.is_empty() {
                "none passed upstream identity verification".into()
            } else {
                lines.join("\n  ")
            };
            bail!("app: {}\nresult: ambiguous candidate lookup; no config change is safe\ncandidates:\n  {}", app.name, candidates)
        }
        Selection::One(selected) => selected,
    };
    if selected.classification != Classification::Absent {
        bail!(
            "{}\nmigration refused: only a verified absent package may be added",
            nix_discovery::report(
                &app,
                &selected.expression,
                &selected.evaluation,
                selected.classification,
                &selected.observed,
                &selected.locators,
            )
        );
    }
    let candidate = selected.evaluation.candidate.as_ref().unwrap();
    let (tracked_file, tracked_text) = tracked_path()?;
    let original = match fs::read_to_string(&tracked_file) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).context("Cannot read tracked.nix"),
    };
    let created = original.is_none();
    let (original_text, edited_text, line) = tracked_edit(original, &selected.expression)?;
    Ok(Some(MigrationPlan {
        index,
        name: app.name.clone(),
        identity: app.identity.clone(),
        expression: selected.expression,
        version: candidate.version.clone(),
        homepage: candidate.homepage.clone(),
        drv_path: candidate.drv_path.clone(),
        main_program: candidate.main_program.clone(),
        tracked_text,
        tracked_file,
        created,
        original_text,
        edited_text,
        line,
    }))
}

pub fn apply(plan: &MigrationPlan, progress: &dyn Fn(&str)) -> Result<()> {
    progress("Writing the confirmed declaration into tracked.nix…");
    if plan.created {
        nix_strategy::create_file(&plan.tracked_file, &plan.edited_text)
    } else {
        nix_strategy::replace_file(&plan.tracked_file, &plan.original_text, &plan.edited_text)
    }
}

pub fn rollback(plan: &MigrationPlan) -> Result<()> {
    if plan.created {
        if plan.tracked_file.exists() {
            fs::remove_file(&plan.tracked_file).context("Cannot remove the created tracked.nix")?;
        }
        Ok(())
    } else {
        nix_strategy::replace_file(&plan.tracked_file, &plan.edited_text, &plan.original_text)
    }
}

pub fn confirmed_present(
    plan: &MigrationPlan,
    show_timer: bool,
    cancel: Option<&AtomicBool>,
) -> Result<bool> {
    let evaluation = nix_discovery::evaluate(&plan.expression, show_timer, cancel)?;
    let candidate = evaluation
        .candidate
        .as_ref()
        .context("Migrated package no longer resolves in the configured package set")?;
    ensure!(
        candidate.drv_path == plan.drv_path,
        "Migrated package resolves to a different derivation after the edit"
    );
    Ok(evaluation
        .home
        .iter()
        .any(|observed| observed.drv_path == plan.drv_path))
}

pub fn seal(ledger: &mut Ledger, plan: &MigrationPlan) -> Result<()> {
    let mut doc = ledger.document();
    let app = doc["apps"]
        .as_array_of_tables_mut()
        .context("apps must be an array of tables")?
        .get_mut(plan.index)
        .context("Application disappeared while saving the Nix migration receipt")?;
    ensure!(
        app["identity"].as_str() == Some(plan.identity.as_str()),
        "Application changed before the Nix migration receipt"
    );
    let mut receipt = Table::new();
    receipt["package"] = value(&plan.expression);
    receipt["version"] = value(&plan.version);
    receipt["homepage"] = value(&plan.homepage);
    receipt["drv_path"] = value(&plan.drv_path);
    receipt["config_file"] = value(&plan.tracked_text);
    receipt["config_expression"] = value(&plan.expression);
    receipt["migrated_at_unix"] = value(timestamp());
    if !plan.main_program.is_empty() {
        let user = std::env::var("APPTRACK_NIX_USER")
            .or_else(|_| std::env::var("USER"))
            .unwrap_or_else(|_| "john".into());
        receipt["realized_path"] = value(format!(
            "/etc/profiles/per-user/{user}/bin/{}",
            plan.main_program
        ));
    }
    app["nix_migration"] = Item::Table(receipt);
    let _lock = ledger.write_lock()?;
    ledger.save_locked(doc)
}

pub fn migrate(
    ledger: &mut Ledger,
    index: usize,
    dialog: &dyn Dialog,
    show_timer: bool,
    cancel: Option<&AtomicBool>,
) -> Result<String> {
    let progress = |message: &str| dialog.message(message.to_string());
    let Some(plan) = plan(ledger, index, show_timer, cancel, &progress)? else {
        bail!("No migration plan available");
    };
    dialog.message(plan.summary());
    let choices = vec!["Add to tracked.nix (no rebuild)".to_string(), "Cancel".to_string()];
    if dialog.choose("Edit tracked.nix?", &choices, Some(1))? != Some(0) {
        return Ok("Cancelled; tracked.nix unchanged.".into());
    }
    apply(&plan, &progress)?;
    let present = match confirmed_present(&plan, show_timer, cancel) {
        Ok(present) => present,
        Err(error) => {
            let detail = format!("{error:#}");
            return match rollback(&plan) {
                Ok(()) => Err(error.context("post-edit evaluation failed; original tracked.nix restored")),
                Err(rollback) => Err(error.context(format!(
                    "post-edit evaluation failed and tracked.nix rollback also failed: {rollback:#}\n{detail}"
                ))),
            };
        }
    };
    if !present {
        rollback(&plan).context("Migrated package was not evaluated after the edit; tracked.nix rollback also failed")?;
        bail!("Migrated package did not appear in the evaluated Home Manager packages; original tracked.nix restored");
    }
    if let Err(error) = seal(ledger, &plan) {
        return match rollback(&plan) {
            Ok(()) => Err(error.context("Nix migration receipt failed; original tracked.nix restored")),
            Err(rollback) => Err(error.context(format!(
                "Nix migration receipt failed and tracked.nix rollback also failed: {rollback:#}"
            ))),
        };
    }
    Ok(format!(
        "app: {}\ndeclared: {} → {}:{}\nevaluated: package appears in the evaluated Home Manager packages\nledger: durable Nix migration receipt recorded\nauthority: tracked.nix edited only; rebuild and switch remain human actions",
        plan.name,
        plan.expression,
        plan.tracked_file.display(),
        plan.line,
    ))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnmigrationPlan {
    pub index: usize,
    pub name: String,
    identity: String,
    expression: String,
    version: String,
    homepage: String,
    drv_path: String,
    tracked_text: String,
    tracked_file: PathBuf,
    original_text: String,
    edited_text: String,
    line: usize,
}

impl UnmigrationPlan {
    pub fn summary(&self) -> String {
        format!(
            "Remove exact Nix package declaration?\n\n{} {} · {}\n{}:{}\n\nThis edits only tracked.nix. AppTrack will not rebuild NixOS or Home Manager; the package may remain realized until you rebuild. Keeping the declaration is the default.",
            self.expression,
            if self.version.is_empty() {
                "version unreported"
            } else {
                &self.version
            },
            self.homepage,
            self.tracked_file.display(),
            self.line,
        )
    }
}

fn migration_receipt(app: &crate::ledger::App) -> Result<Option<MigratedAuthority>> {
    let Some(receipt) = app.nix_migration.as_ref().filter(|receipt| receipt.active()) else {
        return Ok(None);
    };
    ensure!(
        !receipt.package.is_empty()
            && !receipt.config_file.is_empty()
            && !receipt.config_expression.is_empty()
            && !receipt.drv_path.is_empty()
            && receipt.migrated_at_unix.is_some(),
        "Nix migration receipt is missing package, config_file, config_expression, drv_path, or migrated_at_unix"
    );
    Ok(Some(MigratedAuthority {
        config_file: receipt.config_file.clone(),
        expression: receipt.config_expression.clone(),
        drv_path: receipt.drv_path.clone(),
        version: receipt.version.clone(),
        homepage: receipt.homepage.clone(),
    }))
}

struct MigratedAuthority {
    config_file: String,
    expression: String,
    drv_path: String,
    version: String,
    homepage: String,
}

pub fn unplan(ledger: &Ledger, index: usize) -> Result<Option<UnmigrationPlan>> {
    ledger.check_unchanged()?;
    let app = ledger.apps.get(index).context("Application no longer exists")?;
    let Some(authority) = migration_receipt(app)? else {
        return Ok(None);
    };
    let expression = authority.expression;
    ensure!(
        expression == expression.trim() && !expression.contains(['\n', '\r']),
        "Nix migration receipt expression must be exact and on one line"
    );
    let tracked_file = nix_strategy::config_path(&authority.config_file)?;
    ensure!(
        tracked_file.file_name().is_some_and(|name| name == "tracked.nix"),
        "Nix migration removal authority covers only tracked.nix"
    );
    let original_text = fs::read_to_string(&tracked_file)?;
    let (range, line) = nix_strategy::expression_range(&original_text, &expression)?
        .context("Migrated Nix declaration is no longer present in tracked.nix")?;
    let mut edited_text = original_text.clone();
    edited_text.replace_range(range, "");
    ensure!(
        nix_strategy::expression_range(&edited_text, &expression)?.is_none(),
        "Nix expression remained after the proposed tracked.nix edit"
    );
    Ok(Some(UnmigrationPlan {
        index,
        name: app.name.clone(),
        identity: app.identity.clone(),
        expression,
        version: authority.version,
        homepage: authority.homepage,
        drv_path: authority.drv_path,
        tracked_text: authority.config_file,
        tracked_file,
        original_text,
        edited_text,
        line,
    }))
}

fn confirmed_absent(plan: &UnmigrationPlan) -> Result<bool> {
    let evaluation = nix_discovery::evaluate(&plan.expression, true, None)?;
    let candidate = evaluation
        .candidate
        .as_ref()
        .context("Removed package no longer resolves in the configured package set")?;
    ensure!(
        candidate.drv_path == plan.drv_path,
        "Removed package resolves to a different derivation after the edit"
    );
    Ok(!evaluation
        .home
        .iter()
        .any(|observed| observed.drv_path == plan.drv_path))
}

fn seal_removal(ledger: &mut Ledger, plan: &UnmigrationPlan) -> Result<()> {
    let mut doc = ledger.document();
    let app = doc["apps"]
        .as_array_of_tables_mut()
        .context("apps must be an array of tables")?
        .get_mut(plan.index)
        .context("Application disappeared while saving the Nix removal receipt")?;
    ensure!(
        app["identity"].as_str() == Some(plan.identity.as_str()),
        "Application changed before the Nix removal receipt"
    );
    let receipt = app["nix_migration"]
        .as_table_mut()
        .context("Nix migration receipt disappeared")?;
    receipt["removed_at_unix"] = value(timestamp());
    let _lock = ledger.write_lock()?;
    ledger.save_locked(doc)
}

fn rollback_unmigration(plan: &UnmigrationPlan) -> Result<()> {
    nix_strategy::replace_file(&plan.tracked_file, &plan.edited_text, &plan.original_text)
}

pub fn unmigrate(ledger: &mut Ledger, index: usize, dialog: &dyn Dialog) -> Result<String> {
    let Some(plan) = unplan(ledger, index)? else {
        bail!("No active Nix migration receipt; nothing to remove from tracked.nix");
    };
    dialog.message(plan.summary());
    let choices = vec!["Remove from tracked.nix (no rebuild)".to_string(), "Cancel".to_string()];
    if dialog.choose("Edit tracked.nix?", &choices, Some(1))? != Some(0) {
        return Ok("Cancelled; tracked.nix unchanged.".into());
    }
    ensure!(
        unplan(ledger, index)?.as_ref() == Some(&plan),
        "Nix migration removal authority changed; review the record again"
    );
    nix_strategy::replace_file(&plan.tracked_file, &plan.original_text, &plan.edited_text)?;
    let absent = match confirmed_absent(&plan) {
        Ok(absent) => absent,
        Err(error) => {
            return match rollback_unmigration(&plan) {
                Ok(()) => Err(error.context("post-edit evaluation failed; original tracked.nix restored")),
                Err(rollback) => Err(error.context(format!(
                    "post-edit evaluation failed and tracked.nix rollback also failed: {rollback:#}"
                ))),
            };
        }
    };
    if !absent {
        rollback_unmigration(&plan).context("Removed package was still evaluated after the edit; tracked.nix rollback also failed")?;
        bail!("Removed package still appears in the evaluated Home Manager packages (declared elsewhere?); original tracked.nix restored");
    }
    if let Err(error) = seal_removal(ledger, &plan) {
        return match rollback_unmigration(&plan) {
            Ok(()) => Err(error.context("Nix removal receipt failed; original tracked.nix restored")),
            Err(rollback) => Err(error.context(format!(
                "Nix removal receipt failed and tracked.nix rollback also failed: {rollback:#}"
            ))),
        };
    }
    Ok(format!(
        "app: {}\nremoved: {} from {}:{}\nevaluated: package no longer appears in the evaluated Home Manager packages\nledger: Nix migration receipt marked removed\nauthority: tracked.nix edited only; the package may remain realized until you rebuild",
        plan.name,
        plan.expression,
        plan.tracked_file.display(),
        plan.line,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracked_edit_creates_canonical_file_and_inserts_the_expression() -> Result<()> {
        let (original, edited, line) = tracked_edit(None, "pkgs.unstable.tool")?;
        assert_eq!(original, CANONICAL_TRACKED);
        assert_eq!(line, 6);
        assert!(edited.contains("    pkgs.unstable.tool\n"), "{edited}");
        assert_eq!(
            nix_strategy::expression_range(&edited, "pkgs.unstable.tool")?.unwrap().1,
            6
        );
        assert!(edited.contains("# AppTrack-managed"));
        Ok(())
    }

    #[test]
    fn tracked_edit_appends_to_an_existing_list_and_refuses_duplicates() -> Result<()> {
        let existing = "{ pkgs, ... }:\n{\n  home.packages = with pkgs; [\n    one\n  ];\n}\n";
        let (original, edited, line) = tracked_edit(Some(existing.into()), "two")?;
        assert_eq!(original, existing);
        assert_eq!(line, 4);
        assert!(edited.contains("    two\n    one\n"), "{edited}");
        assert!(tracked_edit(Some(edited), "two").is_err());
        Ok(())
    }

    #[test]
    fn tracked_edit_fails_closed_on_mangled_files_and_expressions() {
        assert!(tracked_edit(Some("{\n}\n".into()), "tool").is_err());
        assert!(tracked_edit(
            Some("{\n  home.packages = with pkgs; [\n  ];\n  home.packages = with pkgs; [\n  ];\n}\n".into()),
            "tool"
        )
        .is_err());
        assert!(tracked_edit(None, "tool; builtins.abort \"x\"").is_err());
        assert!(tracked_edit(None, "multi word").is_err());
    }

    fn fixture(created: bool) -> Result<(tempfile::TempDir, MigrationPlan, Ledger)> {
        let dir = tempfile::Builder::new()
            .prefix("apptrack-smoke-")
            .tempdir_in("/tmp")?;
        let home = dir.path().join("nix-config/home");
        fs::create_dir_all(&home)?;
        let tracked = home.join("tracked.nix");
        let original = if created {
            None
        } else {
            fs::write(&tracked, CANONICAL_TRACKED)?;
            Some(CANONICAL_TRACKED.to_string())
        };
        let (original_text, edited_text, line) = tracked_edit(original, "tool")?;
        let ledger_path = dir.path().join("ledger.toml");
        fs::write(
            &ledger_path,
            r#"schema_version = 1
# keep this note
[[apps]]
identity = "https://github.com/example/tool"
name = "tool"
category = "Tools"
disposition = "using"
[apps.provenance]
source = "unknown"
managed_by_apptrack = false
"#,
        )?;
        let ledger = Ledger::open(&ledger_path)?;
        let plan = MigrationPlan {
            index: 0,
            name: "tool".into(),
            identity: "https://github.com/example/tool".into(),
            expression: "tool".into(),
            version: "2.0".into(),
            homepage: "https://github.com/example/tool".into(),
            drv_path: "/nix/store/tool.drv".into(),
            main_program: "tool".into(),
            tracked_text: tracked.to_string_lossy().into(),
            tracked_file: tracked,
            created,
            original_text,
            edited_text,
            line,
        };
        Ok((dir, plan, ledger))
    }

    #[test]
    fn apply_seal_and_rollback_cover_created_and_existing_tracked_files() -> Result<()> {
        for created in [true, false] {
            let (_dir, plan, mut ledger) = fixture(created)?;
            assert_eq!(plan.tracked_file.exists(), !created);
            apply(&plan, &|_| {})?;
            assert_eq!(fs::read_to_string(&plan.tracked_file)?, plan.edited_text);
            seal(&mut ledger, &plan)?;
            let text = fs::read_to_string(&ledger.path)?;
            assert!(text.contains("# keep this note"), "{text}");
            assert!(text.contains("[apps.nix_migration]"), "{text}");
            assert!(text.contains("config_expression = \"tool\""), "{text}");
            assert!(text.contains("realized_path = \"/etc/profiles/per-user/"), "{text}");
            assert!(!text.contains("managed_by_apptrack = true"), "{text}");
            rollback(&plan)?;
            if created {
                assert!(!plan.tracked_file.exists());
            } else {
                assert_eq!(fs::read_to_string(&plan.tracked_file)?, CANONICAL_TRACKED);
            }
        }
        Ok(())
    }

    #[test]
    fn apply_refuses_a_drifted_tracked_file() -> Result<()> {
        let (_dir, plan, _ledger) = fixture(false)?;
        fs::write(&plan.tracked_file, "{\n}\n")?;
        assert!(apply(&plan, &|_| {}).is_err());
        assert_eq!(fs::read_to_string(&plan.tracked_file)?, "{\n}\n");
        Ok(())
    }

    fn unmigration_fixture() -> Result<(tempfile::TempDir, Ledger, PathBuf)> {
        let (dir, plan, mut ledger) = fixture(false)?;
        apply(&plan, &|_| {})?;
        seal(&mut ledger, &plan)?;
        Ok((dir, ledger, plan.tracked_file))
    }

    #[test]
    fn unplan_removes_the_exact_migrated_declaration_only_once() -> Result<()> {
        let (_dir, mut ledger, tracked) = unmigration_fixture()?;
        let plan = unplan(&ledger, 0)?.unwrap();
        assert_eq!(plan.expression, "tool");
        assert_eq!(plan.line, 6);
        assert_eq!(plan.edited_text, CANONICAL_TRACKED);
        nix_strategy::replace_file(&tracked, &plan.original_text, &plan.edited_text)?;
        seal_removal(&mut ledger, &plan)?;
        assert!(!fs::read_to_string(&tracked)?.contains("tool"));
        let text = fs::read_to_string(&ledger.path)?;
        assert!(text.contains("removed_at_unix"), "{text}");
        assert!(text.contains("[apps.nix_migration]"), "{text}");
        assert!(unplan(&ledger, 0)?.is_none(), "a removed receipt grants no further authority");
        Ok(())
    }

    #[test]
    fn unplan_requires_a_receipt_and_an_undrifted_declaration() -> Result<()> {
        let (_dir, _plan, ledger) = fixture(false)?;
        assert!(unplan(&ledger, 0)?.is_none(), "no receipt, no authority");
        let (_dir, ledger, tracked) = unmigration_fixture()?;
        fs::write(&tracked, CANONICAL_TRACKED)?;
        assert!(unplan(&ledger, 0).is_err(), "declaration drifted away");
        fs::write(&tracked, "{\n}\n")?;
        assert!(unplan(&ledger, 0).is_err(), "tracked.nix lost its canonical shape");
        Ok(())
    }
}
