use crate::{
    bun_strategy,
    cargo_strategy,
    codeberg_strategy,
    flatpak_strategy,
    gitlab_strategy,
    ledger::{Disposition, Ledger},
    update,
};
use anyhow::Result;
use std::{collections::{BTreeMap, HashMap}, io::{self, Write}};

#[derive(Clone, Debug)]
pub enum Plan {
    Github(update::Plan),
    Gitlab(update::Plan),
    Codeberg(update::Plan),
    Cargo(cargo_strategy::Plan),
    Bun(bun_strategy::Plan),
    Flatpak(flatpak_strategy::Plan),
}

impl Plan {
    #[cfg(test)]
    pub fn index(&self) -> usize {
        match self {
            Self::Github(plan) => plan.index,
            Self::Gitlab(plan) => plan.index,
            Self::Codeberg(plan) => plan.index,
            Self::Cargo(plan) => plan.index,
            Self::Bun(plan) => plan.index,
            Self::Flatpak(plan) => plan.index,
        }
    }
    pub fn name(&self) -> &str {
        match self {
            Self::Github(plan) => &plan.name,
            Self::Gitlab(plan) => &plan.name,
            Self::Codeberg(plan) => &plan.name,
            Self::Cargo(plan) => &plan.name,
            Self::Bun(plan) => &plan.name,
            Self::Flatpak(plan) => &plan.name,
        }
    }
    pub fn old_version(&self) -> Option<&str> {
        match self {
            Self::Github(plan) => plan.old_version.as_deref(),
            Self::Gitlab(plan) => plan.old_version.as_deref(),
            Self::Codeberg(plan) => plan.old_version.as_deref(),
            Self::Cargo(plan) => plan.old_version.as_deref().or(plan.installed_version.as_deref()),
            Self::Bun(plan) => plan.old_version.as_deref().or(plan.installed_version.as_deref()),
            Self::Flatpak(plan) => plan.installed_version.as_deref().or(plan.old_version.as_deref()),
        }
    }
    pub fn release(&self) -> &str {
        match self {
            Self::Github(plan) => &plan.release,
            Self::Gitlab(plan) => &plan.release,
            Self::Codeberg(plan) => &plan.release,
            Self::Cargo(plan) => &plan.release,
            Self::Bun(plan) => &plan.release,
            Self::Flatpak(plan) => &plan.release,
        }
    }
    pub fn up_to_date(&self) -> bool {
        match self {
            Self::Github(plan) => plan.up_to_date,
            Self::Gitlab(plan) => plan.up_to_date,
            Self::Codeberg(plan) => plan.up_to_date,
            Self::Cargo(plan) => plan.up_to_date,
            Self::Bun(plan) => plan.up_to_date,
            Self::Flatpak(plan) => plan.up_to_date,
        }
    }
    pub fn summary(&self) -> String {
        match self {
            Self::Github(plan) => plan.summary(),
            Self::Gitlab(plan) => plan.summary(),
            Self::Codeberg(plan) => plan.summary(),
            Self::Cargo(plan) => plan.summary(),
            Self::Bun(plan) => plan.summary(),
            Self::Flatpak(plan) => plan.summary(),
        }
    }
    fn targets(&self) -> Vec<String> {
        match self {
            Self::Github(plan) => vec![plan.destination.display().to_string()],
            Self::Gitlab(plan) => vec![plan.destination.display().to_string()],
            Self::Codeberg(plan) => vec![plan.destination.display().to_string()],
            Self::Cargo(plan) => plan.targets.iter().map(|path| path.display().to_string()).collect(),
            Self::Bun(plan) => plan.targets.iter().map(|path| path.display().to_string()).collect(),
            Self::Flatpak(plan) => vec![plan.collision_key()],
        }
    }
}

pub(crate) fn check_one(
    ledger: &Ledger,
    index: usize,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<Plan> {
    let recipe = ledger.apps[index]
        .recipe
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("No update recipe recorded for this app"))?;
    match (recipe.source.as_str(), recipe.installer.as_str()) {
        ("github", installer) if update::supported_installer(installer) => {
            Ok(Plan::Github(update::check_cancellable(ledger, index, cancel)?))
        }
        ("gitlab", "appimage" | "appimage-appdir") => Ok(Plan::Gitlab(gitlab_strategy::check(
            ledger, index, cancel,
        )?)),
        ("codeberg", installer) if update::supported_installer(installer) => Ok(
            Plan::Codeberg(codeberg_strategy::check(ledger, index, cancel)?),
        ),
        ("cargo", "cargo-install") => {
            Ok(Plan::Cargo(cargo_strategy::check(ledger, index, cancel)?))
        }
        ("bun", "bun-global") => {
            Ok(Plan::Bun(bun_strategy::check(ledger, index, cancel)?))
        }
        ("flatpak", "flatpak") => {
            Ok(Plan::Flatpak(flatpak_strategy::check(ledger, index, cancel)?))
        }
        _ => anyhow::bail!("Unsupported recipe: {} / {}", recipe.source, recipe.installer),
    }
}

pub struct Batch {
    pub plans: Vec<Plan>,
    pub skipped: Vec<(String, String)>,
    pub errors: Vec<(String, String)>,
    /// Failed checks as (app index, detail), for receipts written by the caller
    /// that holds the write lock. Workers never touch the ledger.
    pub failed: Vec<(usize, String)>,
    pub considering_excluded: usize,
    pub archived_excluded: usize,
}

/// Writes a receipt for every failed check. Call only after the workers have
/// finished, from the thread that owns the ledger; workers never mutate it.
pub fn record_check_failures(ledger: &mut Ledger, batch: &Batch, report: impl Fn(&str)) {
    for (index, detail) in &batch.failed {
        if let Err(receipt_error) = ledger.record_failure(*index, "check", detail) {
            report(&format!(
                "{}: check receipt not saved: {receipt_error:#}",
                ledger.apps[*index].name
            ));
        }
    }
}

pub fn check(ledger: &Ledger, include_considering: bool) -> Result<Batch> {
    // `collect` already formats progress; print it as given.
    collect(ledger, include_considering, |ledger, index| {
        check_one(ledger, index, &std::sync::atomic::AtomicBool::new(false))
    }, |s| {
        println!("{s}")
    })
}

fn collect(
    ledger: &Ledger,
    include_considering: bool,
    check_one: impl Fn(&Ledger, usize) -> Result<Plan> + Sync,
    progress: impl Fn(&str) + Sync,
) -> Result<Batch> {
    collect_with_cancel(
        ledger,
        include_considering,
        check_one,
        progress,
        &std::sync::atomic::AtomicBool::new(false),
    )
}

pub(crate) fn collect_with_cancel(
    ledger: &Ledger,
    include_considering: bool,
    check_one: impl Fn(&Ledger, usize) -> Result<Plan> + Sync,
    progress: impl Fn(&str) + Sync,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<Batch> {
    ledger.check_unchanged()?;
    let mut batch = Batch {
        plans: Vec::new(),
        skipped: Vec::new(),
        errors: Vec::new(),
        failed: Vec::new(),
        considering_excluded: 0,
        archived_excluded: 0,
    };
    let mut eligible = Vec::new();
    for (index, app) in ledger.apps.iter().enumerate() {
        match app.disposition {
            Disposition::Archived => {
                batch.archived_excluded += 1;
                continue;
            }
            Disposition::Considering if !include_considering => {
                batch.considering_excluded += 1;
                continue;
            }
            _ => {}
        }
        let Some(recipe) = &app.recipe else {
            batch
                .skipped
                .push((app.name.clone(), "no update recipe".into()));
            continue;
        };
        let supported = (recipe.source == "github" && update::supported_installer(&recipe.installer))
            || (recipe.source == "codeberg" && update::supported_installer(&recipe.installer))
            || (recipe.source == "gitlab"
                && matches!(recipe.installer.as_str(), "appimage" | "appimage-appdir"))
            || (recipe.source == "cargo" && recipe.installer == "cargo-install")
            || (recipe.source == "bun" && recipe.installer == "bun-global")
            || (recipe.source == "flatpak" && recipe.installer == "flatpak");
        if !supported {
            batch.skipped.push((
                app.name.clone(),
                format!(
                    "unsupported recipe: {} / {}",
                    recipe.source, recipe.installer
                ),
            ));
            continue;
        }
        eligible.push(index);
    }
    let checked = crate::work::map_by(
        &eligible,
        cancel,
        |&index| {
            let recipe = ledger.apps[index].recipe.as_ref().unwrap();
            match recipe.source.as_str() {
                "cargo" | "bun" => format!(
                    "{}:{}",
                    recipe.source,
                    recipe.package.as_deref().unwrap_or("")
                ),
                "flatpak" => format!(
                    "flatpak:{}:{}",
                    recipe.remote.as_deref().unwrap_or(""),
                    recipe.package.as_deref().unwrap_or("")
                ),
                "github" | "gitlab" | "codeberg" => {
                    format!("{}:{}", recipe.source, recipe.repo)
                }
                _ => format!("{}:{}", recipe.source, recipe.repo),
            }
        },
        |&index| {
            progress(&format!("Checking {}…", ledger.apps[index].name));
            let result = check_one(ledger, index);
            progress(&format!("Checked {}", ledger.apps[index].name));
            result
        },
    );
    for (&index, result) in eligible.iter().zip(checked) {
        let Some(result) = result else {
            continue;
        };
        match result {
            Ok(plan) => batch.plans.push(plan),
            Err(error) => {
                let detail = format!("{error:#}");
                batch
                    .errors
                    .push((ledger.apps[index].name.clone(), detail.clone()));
                batch.failed.push((index, detail));
            }
        }
    }
    // Catch overlap even when one of the two recipes is already up to date.
    let mut targets: HashMap<String, Vec<String>> = HashMap::new();
    for plan in &batch.plans {
        for target in plan.targets() {
            targets.entry(target).or_default().push(plan.name().to_string());
        }
    }
    batch.plans.retain(|plan| {
        for target in plan.targets() {
            let apps = &targets[&target];
            if apps.len() > 1 {
                batch.errors.push((
                    plan.name().to_string(),
                    format!(
                        "destination {} is shared by {}; fix the recipes",
                        target,
                        apps.join(", ")
                    ),
                ));
                return false;
            }
        }
        true
    });
    ledger.check_unchanged()?;
    Ok(batch)
}

impl Batch {
    pub(crate) fn review(&self) -> String {
        let mut text = format!(
            "{} updates · {} current · {} skipped · {} check failures\n{} Considering excluded · {} Archived excluded\n",
            self.plans.iter().filter(|p| !p.up_to_date()).count(),
            self.plans.iter().filter(|p| p.up_to_date()).count(),
            self.skipped.len(),
            self.errors.len(),
            self.considering_excluded,
            self.archived_excluded
        );
        for plan in self.plans.iter().filter(|p| !p.up_to_date()) {
            text.push_str(&format!("\n{}\n", plan.summary()));
        }
        for (name, error) in &self.errors {
            text.push_str(&format!("\n{name}: {error}\n"));
        }
        let mut skipped = BTreeMap::new();
        for (_, reason) in &self.skipped {
            *skipped.entry(reason).or_insert(0) += 1;
        }
        for (reason, count) in skipped {
            text.push_str(&format!("\nSkipped {count}: {reason}"));
        }
        text
    }
    pub fn print(&self) {
        let updates = self.plans.iter().filter(|p| !p.up_to_date()).count();
        let current = self.plans.len() - updates;
        println!(
            "\n{updates} updates | {current} current | {} skipped | {} check failures",
            self.skipped.len(),
            self.errors.len()
        );
        println!(
            "{} Considering excluded | {} Archived excluded",
            self.considering_excluded, self.archived_excluded
        );
        for plan in &self.plans {
            println!(
                "{}: {} → {}{}",
                plan.name(),
                plan.old_version().unwrap_or("unknown"),
                plan.release(),
                if plan.up_to_date() { " (current)" } else { "" }
            );
        }
        let mut skipped_reasons = BTreeMap::new();
        for (_, why) in &self.skipped {
            *skipped_reasons.entry(why).or_insert(0) += 1;
        }
        for (why, count) in skipped_reasons {
            println!("Skipped {count}: {why}");
        }
        for (name, why) in &self.errors {
            println!("Check failed — {name}: {why}");
        }
        for plan in self.plans.iter().filter(|p| !p.up_to_date()) {
            println!("\n{}\n", plan.summary());
        }
    }
}

#[cfg(test)]
#[derive(Default)]
struct Results {
    succeeded: usize,
    failed: usize,
    not_run: usize,
}

#[cfg(test)]
fn apply_all(
    ledger: &mut Ledger,
    plans: &[Plan],
    mut install: impl FnMut(&mut Ledger, &Plan) -> Result<()>,
    report: impl Fn(&str),
) -> Results {
    let updates: Vec<_> = plans.iter().filter(|p| !p.up_to_date()).collect();
    let mut results = Results::default();
    for (index, plan) in updates.iter().enumerate() {
        if let Err(error) = ledger.check_unchanged() {
            report(&format!("Batch stopped: {error:#}"));
            results.not_run = updates.len() - index;
            break;
        }
        report(&format!(
            "\n[{}/{}] {}",
            index + 1,
            updates.len(),
            plan.name()
        ));
        match install(ledger, plan) {
            Ok(()) => {
                results.succeeded += 1;
            }
            Err(error) => {
                results.failed += 1;
                let detail = format!("{error:#}");
                report(&format!("{} failed: {detail}", plan.name()));
                if let Err(receipt_error) = ledger.record_failure(plan.index(), "update", &detail) {
                    report(&format!(
                        "Batch stopped: could not save failure receipt: {receipt_error:#}"
                    ));
                    results.not_run = updates.len() - index - 1;
                    break;
                }
            }
        }
    }
    results
}

pub fn interactive(ledger: &mut Ledger, include_considering: bool) -> Result<String> {
    println!(
        "Global update — {}",
        if include_considering {
            "Using + Considering"
        } else {
            "Using"
        }
    );
    let batch = check(ledger, include_considering)?;
    batch.print();
    record_check_failures(ledger, &batch, |s| println!("{s}"));
    let count = batch.plans.iter().filter(|p| !p.up_to_date()).count();
    if count == 0 {
        return Ok("No planned updates. See skipped apps and check failures above.".into());
    }
    print!("Apply these {count} updates? [y/N] ");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    if !["y", "yes"].contains(&answer.trim().to_ascii_lowercase().as_str()) {
        return Ok("Batch cancelled; nothing changed.".into());
    }
    crate::dialog::install(ledger, &batch.plans, &crate::dialog::Console)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ledger::expand_path, update::Asset};
    use anyhow::bail;
    use std::{fs, path::PathBuf};

    fn fixture() -> Result<(tempfile::TempDir, Ledger)> {
        let dir = tempfile::tempdir()?;
        let mut text = "schema_version = 1\n".to_string();
        for (name, disposition, recipe) in [
            ("using-a", "using", true),
            ("using-b", "using", true),
            ("trial", "considering", true),
            ("rejected", "archived", true),
            ("no-recipe", "using", false),
        ] {
            text.push_str(&format!("\n[[apps]]\nidentity = 'name:{name}'\nname = '{name}'\ncategory = 'Tools'\ndisposition = '{disposition}'\n"));
            if recipe {
                text.push_str(&format!("[apps.recipe]\nsource = 'github'\nrepo = 'example/{name}'\nasset = 'tool-linux-amd64'\ninstaller = 'binary-copy'\ndestination = '{}'\nos = 'linux'\narch = 'x86_64'\n", dir.path().join(name).display()));
            }
        }
        let path = dir.path().join("ledger.toml");
        fs::write(&path, text)?;
        Ok((dir, Ledger::open(&path)?))
    }

    #[test]
    fn failed_checks_are_recorded_and_capped_without_touching_plans() -> Result<()> {
        let (_dir, mut ledger) = fixture()?;
        let batch = collect(
            &ledger,
            false,
            |_, index| bail!("release lookup failed for {index}"),
            |_| {},
        )?;
        // Both Using apps with recipes failed; no-recipe is skipped, not failed.
        assert_eq!(batch.failed.len(), 2);
        assert!(batch.plans.is_empty());
        assert_eq!(batch.errors.len(), 2);
        record_check_failures(&mut ledger, &batch, |_| {});
        let reopened = Ledger::open(&ledger.path)?;
        let record = reopened.record(batch.failed[0].0);
        assert!(record.contains(r#"action = "check""#));
        assert!(record.contains("release lookup failed"));
        assert_eq!(record.matches("[[apps.failure_history]]").count(), 1);

        // Two more strikes on the same apps keep only the last two.
        let mut ledger = reopened;
        record_check_failures(&mut ledger, &batch, |_| {});
        record_check_failures(&mut ledger, &batch, |_| {});
        let record = Ledger::open(&ledger.path)?.record(batch.failed[0].0);
        assert!(
            (1..=Ledger::FAILURES_KEPT).contains(&record.matches("[[apps.failure_history]]").count()),
            "expected 1..={} kept receipts:\n{record}",
            Ledger::FAILURES_KEPT
        );
        Ok(())
    }

    fn plan(ledger: &Ledger, index: usize) -> Result<Plan> {
        let app = &ledger.apps[index];
        let recipe = app.recipe.clone().unwrap();
        Ok(Plan::Github(update::Plan {
            index,
            name: app.name.clone(),
            identity: app.identity.clone(),
            destination: expand_path(&recipe.destination),
            member: recipe.member.clone(),
            recipe,
            release: "v2".into(),
            asset: Asset {
                name: "tool-linux-amd64".into(),
                api_url: "unused".into(),
                size: 64,
                digest: None,
            },
            previous_hash: None,
            old_version: None,
            up_to_date: false,
            renamed_from: None,
        }))
    }

    #[test]
    fn batch_scope_skips_unconfigured_apps_and_never_checks_archives() -> Result<()> {
        let (_dir, ledger) = fixture()?;
        let batch = collect(&ledger, false, plan, |_| {})?;
        assert_eq!(
            batch
                .plans
                .iter()
                .map(Plan::name)
                .collect::<Vec<_>>(),
            ["using-a", "using-b"]
        );
        assert_eq!(batch.considering_excluded, 1);
        assert_eq!(batch.archived_excluded, 1);
        assert_eq!(batch.skipped.len(), 1);
        let batch = collect(&ledger, true, plan, |_| {})?;
        assert_eq!(batch.plans.len(), 3);
        assert!(!batch.plans.iter().any(|p| p.name() == "rejected"));
        Ok(())
    }

    #[test]
    fn lookup_failures_and_target_collisions_are_excluded_from_installation() -> Result<()> {
        let (_dir, ledger) = fixture()?;
        let batch = collect(
            &ledger,
            true,
            |ledger, index| {
                if index == 2 {
                    bail!("network unavailable");
                }
                let mut planned = plan(ledger, index)?;
                let Plan::Github(github) = &mut planned else { unreachable!() };
                github.destination = PathBuf::from("/tmp/shared-target");
                github.up_to_date = index == 1;
                Ok(planned)
            },
            |_| {},
        )?;
        assert!(batch.plans.is_empty());
        assert_eq!(batch.errors.len(), 3);
        Ok(())
    }

    #[test]
    fn failed_app_does_not_prevent_next_app_and_saves_its_failure_receipt() -> Result<()> {
        let (_dir, mut ledger) = fixture()?;
        let batch = collect(&ledger, false, plan, |_| {})?;
        let mut installed = Vec::new();
        let results = apply_all(
            &mut ledger,
            &batch.plans,
            |ledger, planned| {
                if planned.index() == 0 {
                    bail!("failed download");
                }
                installed.push(planned.name().to_string());
                ledger.decide(planned.index(), Disposition::Using, None)
            },
            |_| {},
        );
        assert_eq!(installed, ["using-b"]);
        assert_eq!(
            (results.succeeded, results.failed, results.not_run),
            (1, 1, 0)
        );
        assert!(ledger.record(0).contains("failed download"));
        Ok(())
    }

    #[test]
    fn external_ledger_change_stops_remaining_batch_without_clobbering_it() -> Result<()> {
        let (_dir, mut ledger) = fixture()?;
        let batch = collect(&ledger, false, plan, |_| {})?;
        let results = apply_all(
            &mut ledger,
            &batch.plans,
            |ledger, _| {
                fs::write(
                    &ledger.path,
                    format!("{}\n# external edit\n", fs::read_to_string(&ledger.path)?),
                )?;
                Ok(())
            },
            |_| {},
        );
        assert_eq!(
            (results.succeeded, results.failed, results.not_run),
            (1, 0, 1)
        );
        assert!(fs::read_to_string(&ledger.path)?.contains("# external edit"));
        Ok(())
    }
}
