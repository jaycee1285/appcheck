use crate::{batch, bun_strategy, cargo_strategy, flatpak_strategy, ledger::{Disposition, Ledger}, update};
use anyhow::Result;
use std::{
    fs,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
};

static NEVER_CANCEL: AtomicBool = AtomicBool::new(false);

pub trait Dialog: Sync {
    fn message(&self, text: String);
    fn prompt(&self, label: &str, default: &str) -> Result<Option<String>>;
    fn choose(
        &self,
        label: &str,
        names: &[String],
        default: Option<usize>,
    ) -> Result<Option<usize>>;
    fn cancel_flag(&self) -> &AtomicBool {
        &NEVER_CANCEL
    }
    fn cancelled(&self) -> bool {
        self.cancel_flag().load(Ordering::Acquire)
    }
}

pub struct Console;
impl Dialog for Console {
    fn message(&self, text: String) {
        println!("{text}");
    }
    fn prompt(&self, label: &str, default: &str) -> Result<Option<String>> {
        crate::intake::prompt(label, default)
    }
    fn choose(
        &self,
        label: &str,
        names: &[String],
        default: Option<usize>,
    ) -> Result<Option<usize>> {
        crate::intake::choose(label, names, default)
    }
}

pub enum Event {
    Message(String),
    Prompt {
        label: String,
        default: String,
        choices: Vec<String>,
        reply: mpsc::Sender<Option<String>>,
    },
    Done {
        ledger: Ledger,
        result: Result<String, String>,
    },
}

pub struct Bridge {
    pub tx: mpsc::Sender<Event>,
    pub cancel: Arc<AtomicBool>,
}
impl Bridge {
    fn ask(&self, label: &str, default: &str, choices: Vec<String>) -> Result<Option<String>> {
        if self.cancelled() {
            return Ok(None);
        }
        let (tx, rx) = mpsc::channel();
        self.tx.send(Event::Prompt {
            label: label.into(),
            default: default.into(),
            choices,
            reply: tx,
        })?;
        Ok(rx.recv().unwrap_or(None))
    }
}
impl Dialog for Bridge {
    fn message(&self, text: String) {
        let _ = self.tx.send(Event::Message(text));
    }
    fn prompt(&self, label: &str, default: &str) -> Result<Option<String>> {
        self.ask(label, default, vec![])
    }
    fn choose(
        &self,
        label: &str,
        names: &[String],
        default: Option<usize>,
    ) -> Result<Option<usize>> {
        Ok(self
            .ask(
                label,
                &default.map(|n| n.to_string()).unwrap_or_default(),
                names.to_vec(),
            )?
            .and_then(|s| s.parse().ok()))
    }
    fn cancel_flag(&self) -> &AtomicBool {
        &self.cancel
    }
}

pub fn updates(
    ledger: &mut Ledger,
    app: Option<usize>,
    include_considering: bool,
    dialog: &dyn Dialog,
) -> Result<String> {
    let plans = if let Some(index) = app {
        dialog.message(format!("Checking {}…", ledger.apps[index].name));
        let mut plan = batch::check_one(ledger, index, dialog.cancel_flag())?;
        loop {
            if dialog.cancelled() {
                return Ok("Cancelled; nothing installed.".into());
            }
            dialog.message(plan.summary());
            let can_clean = match &plan {
                batch::Plan::Github(plan) => plan
                    .destination
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| update::clean_filename(n) != n),
                batch::Plan::Gitlab(plan) => plan
                    .destination
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| update::clean_filename(n) != n),
                batch::Plan::Codeberg(plan) => plan
                    .destination
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| update::clean_filename(n) != n),
                batch::Plan::Cargo(_) | batch::Plan::Bun(_) | batch::Plan::Flatpak(_) => false,
            };
            if plan.up_to_date() && !can_clean {
                return Ok("Already up to date.".into());
            }
            let mut options = vec!["Install".into(), "Cancel".into()];
            if can_clean {
                options.insert(1, "Clean command name".into());
            }
            match dialog.choose("Review update", &options, Some(0))? {
                Some(0) => break vec![plan],
                Some(1) if can_clean => {
                    match &mut plan {
                        batch::Plan::Github(plan)
                        | batch::Plan::Gitlab(plan)
                        | batch::Plan::Codeberg(plan) => {
                            plan.clean_destination()?;
                        }
                        _ => unreachable!(),
                    }
                }
                _ => return Ok("Cancelled; nothing installed.".into()),
            }
        }
    } else {
        dialog.message(
            if include_considering {
                "Checking Using + Considering…"
            } else {
                "Checking Using…"
            }
            .into(),
        );
        let batch = batch::collect_with_cancel(
            ledger,
            include_considering,
            |l, i| batch::check_one(l, i, dialog.cancel_flag()),
            |s| dialog.message(s.into()),
            dialog.cancel_flag(),
        )?;
        if dialog.cancelled() {
            return Ok("Checks cancelled; nothing installed.".into());
        }
        batch::record_check_failures(ledger, &batch, |s| dialog.message(s.into()));
        dialog.message(batch.review());
        if !batch.plans.iter().any(|p| !p.up_to_date()) {
            return Ok(
                "No planned updates. Review current, skipped and failed checks above.".into(),
            );
        }
        if dialog.choose(
            "Review batch",
            &["Install all planned updates".into(), "Cancel".into()],
            Some(0),
        )? != Some(0)
        {
            return Ok("Cancelled; nothing installed.".into());
        }
        batch.plans
    };
    install(ledger, &plans, dialog)
}

pub fn install_one(ledger: &mut Ledger, index: usize, dialog: &dyn Dialog) -> Result<String> {
    let result = updates(ledger, Some(index), false, dialog)?;
    if result.starts_with("Cancelled") || dialog.cancelled() {
        return Ok(result);
    }
    if ledger.apps[index].disposition == Disposition::Considering
        && (ledger.apps[index].provenance.managed_by_apptrack || ledger.apps[index].installed == Some(true))
        && dialog.choose("Move installed app to Using?", &["Move to Using".into(), "Keep Considering".into()], Some(0))? == Some(0)
    {
        ledger.decide(index, Disposition::Using, None)?;
        return Ok(format!("{result}\nMoved {} to Using.", ledger.apps[index].name));
    }
    Ok(result)
}

/// Each repository's user downloads and installs independently; commits are
/// consumed on this single writer thread as they complete.
pub fn install(ledger: &mut Ledger, plans: &[batch::Plan], dialog: &dyn Dialog) -> Result<String> {
    let downloads = crate::ledger::expand_path(
        &std::env::var("APPTRACK_DOWNLOADS").unwrap_or_else(|_| "~/Downloads".into()),
    );
    let forge: Vec<_> = plans.iter().filter_map(|plan| match plan {
        batch::Plan::Github(plan) if !plan.up_to_date => Some(plan.clone()),
        batch::Plan::Gitlab(plan) if !plan.up_to_date => Some(plan.clone()),
        batch::Plan::Codeberg(plan) if !plan.up_to_date => Some(plan.clone()),
        _ => None,
    }).collect();
    let mut counts = if forge.is_empty() {
        Counts::default()
    } else {
        install_with(
            ledger,
            &forge,
            dialog,
            &downloads,
            |plan, path| update::download_cancellable(plan, path, dialog.cancel_flag()),
            |ledger, plan, path| {
                update::apply_in(
                    ledger,
                    plan,
                    &downloads,
                    &|s| dialog.message(format!("{} · {s}", plan.name)),
                    |_, target| {
                        fs::copy(path, target)?;
                        Ok(())
                    },
                )
            },
        )?
    };
    let cargo: Vec<_> = plans.iter().filter_map(|plan| match plan {
        batch::Plan::Cargo(plan) if !plan.up_to_date => Some(plan),
        _ => None,
    }).collect();
    for (position, plan) in cargo.iter().enumerate() {
        if dialog.cancelled() {
            counts.not_run += cargo.len() - position;
            break;
        }
        match cargo_strategy::apply(ledger, plan, dialog.cancel_flag(), |phase| {
            dialog.message(format!("{} · {phase}", plan.name))
        }) {
            Ok(()) => counts.succeeded += 1,
            Err(error) => {
                counts.failed += 1;
                let detail = format!("{error:#}");
                dialog.message(format!("{} failed: {detail}", plan.name));
                ledger.record_failure(plan.index, "update", &detail)?;
            }
        }
    }
    let bun: Vec<_> = plans
        .iter()
        .filter_map(|plan| match plan {
            batch::Plan::Bun(plan) if !plan.up_to_date => Some(plan),
            _ => None,
        })
        .collect();
    for (position, plan) in bun.iter().enumerate() {
        if dialog.cancelled() {
            counts.not_run += bun.len() - position;
            break;
        }
        match bun_strategy::apply(ledger, plan, dialog.cancel_flag(), |phase| {
            dialog.message(format!("{} · {phase}", plan.name))
        }) {
            Ok(()) => counts.succeeded += 1,
            Err(error) => {
                counts.failed += 1;
                let detail = format!("{error:#}");
                dialog.message(format!("{} failed: {detail}", plan.name));
                ledger.record_failure(plan.index, "update", &detail)?;
            }
        }
    }
    let flatpak: Vec<_> = plans
        .iter()
        .filter_map(|plan| match plan {
            batch::Plan::Flatpak(plan) if !plan.up_to_date => Some(plan),
            _ => None,
        })
        .collect();
    for (position, plan) in flatpak.iter().enumerate() {
        if dialog.cancelled() {
            counts.not_run += flatpak.len() - position;
            break;
        }
        match flatpak_strategy::apply(ledger, plan, dialog.cancel_flag(), |phase| {
            dialog.message(format!("{} · {phase}", plan.name))
        }) {
            Ok(()) => counts.succeeded += 1,
            Err(error) => {
                counts.failed += 1;
                let detail = format!("{error:#}");
                dialog.message(format!("{} failed: {detail}", plan.name));
                ledger.record_failure(plan.index, "update", &detail)?;
            }
        }
    }
    Ok(counts.report())
}

#[derive(Default)]
struct Counts {
    succeeded: usize,
    failed: usize,
    not_run: usize,
}

impl Counts {
    fn report(&self) -> String {
        format!("{} installed · {} failed · {} not run", self.succeeded, self.failed, self.not_run)
    }
}

fn install_with(
    ledger: &mut Ledger,
    plans: &[update::Plan],
    dialog: &dyn Dialog,
    downloads: &std::path::Path,
    download: impl Fn(&update::Plan, &std::path::Path) -> Result<()> + Sync,
    mut commit: impl FnMut(&mut Ledger, &update::Plan, &std::path::Path) -> Result<()>,
) -> Result<Counts> {
    let plans: Vec<_> = plans.iter().filter(|p| !p.up_to_date).collect();
    fs::create_dir_all(&downloads)?;
    // One user per repository, each running its own downloads to completion.
    // A repository that finishes early installs early; nothing waits on the
    // slowest download. Commits are consumed here, on the single writer thread,
    // so network workers still never touch the ledger.
    let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
    for (i, plan) in plans.iter().enumerate() {
        let repo = format!("{}:{}", plan.recipe.source, plan.recipe.repo);
        match groups.iter_mut().find(|(existing, _)| *existing == repo) {
            Some((_, indices)) => indices.push(i),
            None => groups.push((repo, vec![i])),
        }
    }
    let mut succeeded = 0;
    let mut failed = 0;
    let mut done = vec![false; plans.len()];
    let (tx, rx) = std::sync::mpsc::channel();
    let plans_ref = &plans;
    let download_ref = &download;
    let downloads_ref = &downloads;
    std::thread::scope(|scope| -> Result<()> {
        for (_, indices) in &groups {
            let tx = tx.clone();
            scope.spawn(move || {
                for &i in indices {
                    if dialog.cancelled() {
                        return;
                    }
                    let plan = plans_ref[i];
                    let staged = (|| -> Result<tempfile::TempDir> {
                        let staging = tempfile::Builder::new()
                            .prefix("apptrack-batch-")
                            .tempdir_in(downloads_ref)?;
                        dialog.message(format!("Downloading {}…", plan.name));
                        let path = staging.path().join(&plan.asset.name);
                        match download_ref(plan, &path)
                            .and_then(|_| update::verify_download(&path, plan).map(|_| ()))
                        {
                            Ok(()) => {
                                dialog.message(format!("Downloaded {}", plan.name));
                                Ok(staging)
                            }
                            Err(error) => Err(error.context(format!(
                                "Download retained at {}",
                                staging.keep().display()
                            ))),
                        }
                    })();
                    if tx.send((i, staged)).is_err() {
                        return;
                    }
                }
            });
        }
        drop(tx);
        // Install in completion order, one at a time.
        for (i, staged) in rx {
            done[i] = true;
            let plan = plans[i];
            if dialog.cancelled() {
                continue;
            }
            ledger.check_unchanged()?;
            let result = match staged {
                Ok(staging) => commit(ledger, plan, &staging.path().join(&plan.asset.name)),
                Err(error) => Err(error),
            };
            match result {
                Ok(()) => succeeded += 1,
                Err(error) => {
                    failed += 1;
                    dialog.message(format!("{} failed: {error:#}", plan.name));
                    ledger.record_failure(plan.index, "update", &format!("{error:#}"))?;
                }
            }
        }
        Ok(())
    })?;
    // Anything neither installed nor failed was cancelled before its turn.
    let not_run = plans.len() - succeeded - failed;
    Ok(Counts { succeeded, failed, not_run })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    struct Quiet(AtomicBool);
    impl Dialog for Quiet {
        fn message(&self, _: String) {}
        fn prompt(&self, _: &str, _: &str) -> Result<Option<String>> {
            Ok(None)
        }
        fn choose(&self, _: &str, _: &[String], _: Option<usize>) -> Result<Option<usize>> {
            Ok(None)
        }
        fn cancel_flag(&self) -> &AtomicBool {
            &self.0
        }
    }
    fn fixture() -> Result<(tempfile::TempDir, Ledger, Vec<update::Plan>)> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("ledger.toml");
        fs::write(&path, "schema_version = 1\napps = []\n")?;
        let mut ledger = Ledger::open(&path)?;
        for i in 0..4 {
            ledger.add(
                &format!("tool{i}"),
                "Tools",
                &format!("https://github.com/example/tool{i}"),
                "",
                "",
            )?;
        }
        let mut plans = vec![];
        for (i, app) in ledger.apps.iter().enumerate() {
            let mut app = app.clone();
            app.recipe = Some(update::Recipe {
                source: "github".into(),
                repo: format!("example/tool{i}"),
                asset: "binary".into(),
                installer: "binary-copy".into(),
                destination: dir.path().join(format!("tool{i}")).to_string_lossy().into(),
                os: "linux".into(),
                arch: std::env::consts::ARCH.into(),
                clean_filename: false,
                member: None,
                ..update::Recipe::default()
            });
            plans.push(update::make_plan(
                &app,
                i,
                update::Release {
                    tag_name: "v1".into(),
                    is_prerelease: false,
                    assets: vec![update::Asset {
                        name: "binary".into(),
                        api_url: format!(
                            "https://api.github.com/repos/example/tool{i}/releases/assets/1"
                        ),
                        size: 64,
                        digest: None,
                    }],
                },
            )?);
        }
        Ok((dir, ledger, plans))
    }
    /// A repository that finishes early installs early: nothing waits on the
    /// slowest download. Commits still happen one at a time, on one thread, and
    /// a failed download does not stop the others.
    #[test]
    fn downloads_commit_as_they_finish_serially_and_failures_continue() -> Result<()> {
        let (dir, mut ledger, plans) = fixture()?;
        let committed_any = AtomicBool::new(false);
        let pipelined = AtomicBool::new(false);
        let in_commit = AtomicUsize::new(0);
        let overlaps = AtomicUsize::new(0);
        let mut commits = vec![];
        let report = install_with(
            &mut ledger,
            &plans,
            &Quiet(AtomicBool::new(false)),
            &dir.path().join("downloads"),
            |plan, path| {
                if plan.index == 1 {
                    anyhow::bail!("synthetic download failure");
                }
                if plan.index == 3 {
                    // Hold the slowest repository open until another has
                    // installed. Under a global barrier this never becomes true.
                    let start = std::time::Instant::now();
                    while !committed_any.load(Ordering::Acquire)
                        && start.elapsed() < std::time::Duration::from_secs(2)
                    {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    pipelined.store(committed_any.load(Ordering::Acquire), Ordering::Release);
                }
                fs::write(path, [0; 64])?;
                Ok(())
            },
            |ledger, plan, path| {
                if in_commit.fetch_add(1, Ordering::SeqCst) > 0 {
                    overlaps.fetch_add(1, Ordering::SeqCst);
                }
                assert_eq!(fs::metadata(path)?.len(), 64);
                commits.push(plan.index);
                let result = ledger.decide(plan.index, crate::ledger::Disposition::Using, None);
                committed_any.store(true, Ordering::Release);
                in_commit.fetch_sub(1, Ordering::SeqCst);
                result
            },
        )?;
        assert!(
            pipelined.load(Ordering::Acquire),
            "a slow repository blocked every install; the global barrier is back"
        );
        assert_eq!(overlaps.load(Ordering::SeqCst), 0, "commits overlapped");
        commits.sort();
        assert_eq!(commits, [0, 2, 3]);
        assert_eq!(report.report(), "3 installed · 1 failed · 0 not run");
        assert!(ledger.record(1).contains("synthetic download failure"));
        Ok(())
    }
    #[test]
    fn cancel_during_download_does_not_enter_commit_loop() -> Result<()> {
        let (dir, mut ledger, plans) = fixture()?;
        let before = fs::read(&ledger.path)?;
        let dialog = Quiet(AtomicBool::new(false));
        let report = install_with(
            &mut ledger,
            &plans,
            &dialog,
            &dir.path().join("downloads"),
            |_, path| {
                dialog.0.store(true, Ordering::Release);
                fs::write(path, [0; 64])?;
                Ok(())
            },
            |_, _, _| panic!("cancelled download must not commit"),
        )?;
        assert_eq!(report.report(), "0 installed · 0 failed · 4 not run");
        assert_eq!(fs::read(&ledger.path)?, before);
        Ok(())
    }
}
