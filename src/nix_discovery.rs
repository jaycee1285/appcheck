use crate::ledger::{App, Ledger, expand_path, normalize_identity};
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use std::{
    collections::BTreeSet,
    fs,
    io::{IsTerminal, Write},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use toml_edit::{Array, Item, Table, value};

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub(crate) struct EvaluatedPackage {
    pub drv_path: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub pname: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub homepage: String,
    #[serde(default)]
    pub main_program: String,
    #[serde(default)]
    pub scope: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Evaluation {
    pub candidate: Option<EvaluatedPackage>,
    #[serde(default)]
    pub home: Vec<EvaluatedPackage>,
    #[serde(default)]
    pub system: Vec<EvaluatedPackage>,
}

#[derive(Debug, Deserialize)]
struct AttributeNames {
    #[serde(default)]
    stable: Vec<String>,
    #[serde(default)]
    unstable: Vec<String>,
}

struct DiscoveredCandidate {
    expression: String,
    evaluation: Evaluation,
}

#[derive(Clone, Debug, Deserialize)]
struct InboxNix {
    #[serde(default)]
    name: String,
    package: String,
    config_file: String,
    config_line: Option<usize>,
    config_expression: String,
    #[serde(default)]
    scope: String,
    #[serde(default)]
    installer: String,
}

#[derive(Default, Deserialize)]
struct Inbox {
    #[serde(default)]
    nix: Vec<InboxNix>,
}

#[derive(Default, Deserialize)]
struct RawLedger {
    #[serde(default)]
    inbox: Inbox,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Locator {
    app_index: Option<usize>,
    app_name: String,
    package: String,
    config_file: String,
    config_line: Option<usize>,
    config_expression: String,
    scope: String,
    installer: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Classification {
    CandidateUnavailable,
    AmbiguousIdentity,
    ExactPresent,
    OptionSupplied,
    MultiAuthorityReview,
    Absent,
}

fn nix_string(value: &str) -> String {
    serde_json::to_string(value).expect("JSON strings are valid Nix strings")
}

pub(crate) fn candidate_path(package: &str) -> Result<Vec<&str>> {
    let package = package.strip_prefix("pkgs.").unwrap_or(package);
    ensure!(!package.is_empty(), "Nix package expression is empty");
    let parts: Vec<_> = package.split('.').collect();
    ensure!(
        parts.iter().all(|part| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '+'))
        }),
        "First-slice Nix checks require an attribute path such as ripgrep or pkgs.unstable.ripgrep"
    );
    Ok(parts)
}

fn evaluation_expression(
    config_root: &str,
    configuration: &str,
    user: &str,
    package: &str,
) -> Result<String> {
    let path = candidate_path(package)?
        .into_iter()
        .map(nix_string)
        .collect::<Vec<_>>()
        .join(" ");
    Ok(format!(
        r#"let
  flake = builtins.getFlake {flake};
  machine = builtins.getAttr {configuration} flake.nixosConfigurations;
  cfg = machine.config;
  pkgs = machine.pkgs;
  resolve = root: names:
    if names == [] then {{ success = true; value = root; }}
    else
      let name = builtins.head names;
      in if builtins.hasAttr name root
        then resolve (builtins.getAttr name root) (builtins.tail names)
        else {{ success = false; value = null; }};
  summarize = scope: package:
    let
      rawHomepage = (package.meta or {{}}).homepage or "";
      homepage = if builtins.isList rawHomepage
        then builtins.concatStringsSep " " rawHomepage
        else if builtins.isString rawHomepage then rawHomepage else "";
    in {{
      drv_path = package.drvPath;
      name = package.name or "";
      pname = package.pname or "";
      version = package.version or "";
      main_program = (package.meta or {{}}).mainProgram or "";
      inherit homepage scope;
    }};
  attempted = resolve pkgs [ {path} ];
  candidate = if attempted.success then summarize "candidate" attempted.value else null;
  homePackages = (builtins.getAttr {user} cfg.home-manager.users).home.packages;
in {{
  inherit candidate;
  home = map (summarize "home-manager") homePackages;
  system = map (summarize "nixos") cfg.environment.systemPackages;
}}"#,
        flake = nix_string(&format!("path:{config_root}")),
        configuration = nix_string(configuration),
        user = nix_string(user),
    ))
}

fn nix_context() -> Result<(String, String, String)> {
    let config = std::env::var("APPTRACK_NIX_CONFIG").unwrap_or_else(|_| "~/repos/config".into());
    let config = fs::canonicalize(expand_path(&config))
        .context("Cannot resolve APPTRACK_NIX_CONFIG or ~/repos/config")?;
    let configuration =
        std::env::var("APPTRACK_NIX_CONFIGURATION").unwrap_or_else(|_| "Sed".into());
    let user = std::env::var("APPTRACK_NIX_USER")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "john".into());
    Ok((
        config
            .to_str()
            .context("Nix config path is not UTF-8")?
            .to_string(),
        configuration,
        user,
    ))
}

fn nix_eval_json(
    label: &str,
    expression: &str,
    show_timer: bool,
    cancel: Option<&AtomicBool>,
) -> Result<Vec<u8>> {
    let started = Instant::now();
    let terminal = std::io::stderr().is_terminal();
    let stopped = Arc::new(AtomicBool::new(false));
    let timer_stop = Arc::clone(&stopped);
    if show_timer {
        eprint!("{label} (read-only) · 0s");
        std::io::stderr().flush().ok();
    }
    let timer_label = label.to_string();
    let timer = (show_timer && terminal).then(move || {
        thread::spawn(move || {
            while !timer_stop.load(Ordering::Acquire) {
                thread::sleep(Duration::from_secs(1));
                if timer_stop.load(Ordering::Acquire) {
                    break;
                }
                eprint!(
                    "\r{timer_label} (read-only) · {}s",
                    started.elapsed().as_secs()
                );
                std::io::stderr().flush().ok();
            }
        })
    });
    let output = crate::work::output(
        Command::new("nix").args([
            "eval",
            "--offline",
            "--json",
            "--impure",
            "--expr",
            expression,
        ]),
        true,
        cancel,
    );
    stopped.store(true, Ordering::Release);
    if show_timer {
        if let Some(timer) = timer {
            let _ = timer.join();
            eprint!("\r\x1b[2K");
        } else {
            eprintln!();
        }
        eprintln!("{label} complete · {}s", started.elapsed().as_secs());
    }
    let output = output
        .context("Cannot evaluate the configured NixOS and Home Manager package sets")?;
    if !output.status.success() {
        bail!(
            "Nix package-set evaluation failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

pub(crate) fn evaluate(
    package: &str,
    show_timer: bool,
    cancel: Option<&AtomicBool>,
) -> Result<Evaluation> {
    let (config, configuration, user) = nix_context()?;
    let expression = evaluation_expression(&config, &configuration, &user, package)?;
    let json = nix_eval_json(
        "Evaluating Nix package collision",
        &expression,
        show_timer,
        cancel,
    )?;
    serde_json::from_slice(&json).context("Nix returned invalid package-set JSON")
}

fn normalize_name(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn repository_tail(value: &str) -> Option<&str> {
    value
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .rsplit('/')
        .next()
        .filter(|tail| !tail.is_empty())
}

fn lookup_keys(app: &App) -> BTreeSet<String> {
    let mut keys = BTreeSet::from([normalize_name(&app.name)]);
    for value in [Some(app.identity.as_str()), app.provenance.repo.as_deref()] {
        if let Some(tail) = value.and_then(repository_tail) {
            keys.insert(normalize_name(tail));
        }
    }
    if let Some(recipe) = app.recipe.as_ref() {
        if let Some(tail) = repository_tail(&recipe.repo) {
            keys.insert(normalize_name(tail));
        }
    }
    keys.retain(|key| !key.is_empty());
    keys
}

fn known_nix_package(app: &App) -> Option<String> {
    if app.provenance.source == "nix" {
        return app.provenance.package.clone();
    }
    app.installations
        .iter()
        .find(|item| item.source == "nix")
        .and_then(|item| item.package.clone())
}

fn matching_attribute_paths(keys: &BTreeSet<String>, names: AttributeNames) -> Vec<String> {
    let mut candidates = Vec::new();
    for name in names.stable {
        if keys.contains(&normalize_name(&name)) {
            candidates.push(name);
        }
    }
    for name in names.unstable {
        if keys.contains(&normalize_name(&name)) {
            candidates.push(format!("pkgs.unstable.{name}"));
        }
    }
    candidates.sort();
    candidates.dedup();
    candidates
}

fn available_attribute_paths(
    app: &App,
    show_timer: bool,
    cancel: Option<&AtomicBool>,
) -> Result<Vec<String>> {
    if let Some(package) = known_nix_package(app) {
        return Ok(vec![package]);
    }
    let (config, configuration, _) = nix_context()?;
    let expression = format!(
        r#"let
  flake = builtins.getFlake {flake};
  machine = builtins.getAttr {configuration} flake.nixosConfigurations;
in {{
  stable = builtins.attrNames machine.pkgs;
  unstable = builtins.attrNames machine.pkgs.unstable;
}}"#,
        flake = nix_string(&format!("path:{config}")),
        configuration = nix_string(&configuration),
    );
    let json = nix_eval_json(
        "Scanning configured Nix attributes",
        &expression,
        show_timer,
        cancel,
    )?;
    let names: AttributeNames =
        serde_json::from_slice(&json).context("Nix returned invalid attribute-name JSON")?;
    Ok(matching_attribute_paths(&lookup_keys(app), names))
}

fn locators(ledger: &Ledger) -> Result<Vec<Locator>> {
    let mut found = Vec::new();
    for (index, app) in ledger.apps.iter().enumerate() {
        if app.provenance.source == "nix" {
            if let (Some(package), Some(file), Some(expression)) = (
                app.provenance.package.as_ref(),
                app.provenance.config_file.as_ref(),
                app.provenance.config_expression.as_ref(),
            ) {
                found.push(Locator {
                    app_index: Some(index),
                    app_name: app.name.clone(),
                    package: package.clone(),
                    config_file: file.clone(),
                    config_line: app.provenance.config_line,
                    config_expression: expression.clone(),
                    scope: "tracked".into(),
                    installer: app.provenance.installer.clone().unwrap_or_default(),
                });
            }
        }
        for installation in app
            .installations
            .iter()
            .filter(|item| item.source == "nix")
        {
            if let (Some(package), Some(file), Some(expression)) = (
                installation.package.as_ref(),
                installation.config_file.as_ref(),
                installation.config_expression.as_ref(),
            ) {
                found.push(Locator {
                    app_index: Some(index),
                    app_name: app.name.clone(),
                    package: package.clone(),
                    config_file: file.clone(),
                    config_line: installation.config_line,
                    config_expression: expression.clone(),
                    scope: "tracked multi-install".into(),
                    installer: installation.installer.clone().unwrap_or_default(),
                });
            }
        }
    }
    for (index, app) in ledger.apps.iter().enumerate() {
        if let Some(receipt) = app.nix_migration.as_ref().filter(|receipt| receipt.active()) {
            found.push(Locator {
                app_index: Some(index),
                app_name: app.name.clone(),
                package: receipt.config_expression.clone(),
                config_file: receipt.config_file.clone(),
                config_line: None,
                config_expression: receipt.config_expression.clone(),
                scope: "tracked.nix".into(),
                installer: "home-manager".into(),
            });
        }
    }
    let text = fs::read_to_string(&ledger.path)?;
    let raw: RawLedger = toml::from_str(&text).context("Cannot read Nix research inbox")?;
    found.extend(raw.inbox.nix.into_iter().map(|item| Locator {
        app_index: None,
        app_name: item.name,
        package: item.package,
        config_file: item.config_file,
        config_line: item.config_line,
        config_expression: item.config_expression,
        scope: item.scope,
        installer: item.installer,
    }));
    Ok(found)
}

fn same_package(left: &str, right: &str) -> bool {
    left == right
        || left.strip_prefix("pkgs.") == Some(right)
        || right.strip_prefix("pkgs.") == Some(left)
}

fn upstreams(app: &App) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for value in [Some(app.identity.as_str()), app.provenance.repo.as_deref()] {
        if let Some(value) = value.filter(|value| value.starts_with("http")) {
            found.insert(normalize_identity(value));
        }
    }
    if let Some(recipe) = app.recipe.as_ref().filter(|recipe| !recipe.repo.is_empty()) {
        let host = match recipe.source.as_str() {
            "github" => Some("github.com"),
            "gitlab" => Some("gitlab.com"),
            "codeberg" => Some("codeberg.org"),
            _ => None,
        };
        if let Some(host) = host {
            found.insert(normalize_identity(&format!(
                "https://{host}/{}",
                recipe.repo
            )));
        }
    }
    found
}

fn homepage_matches(app: &App, candidate: &EvaluatedPackage) -> bool {
    let homepage = candidate.homepage.split_whitespace().next().unwrap_or("");
    !homepage.is_empty() && upstreams(app).contains(&normalize_identity(homepage))
}

fn has_non_nix_authority(app: &App) -> bool {
    app.tags.iter().any(|tag| tag == "multi-install")
        || (!matches!(app.provenance.source.as_str(), "nix" | "unknown")
            && app.installed != Some(false))
        || app.installations.iter().any(|item| item.source != "nix")
}

fn classify(
    app_index: usize,
    app: &App,
    package: &str,
    evaluation: &Evaluation,
    all_locators: &[Locator],
) -> (Classification, Vec<EvaluatedPackage>, Vec<Locator>) {
    let Some(candidate) = evaluation.candidate.as_ref() else {
        return (Classification::CandidateUnavailable, vec![], vec![]);
    };
    let matching: Vec<_> = evaluation
        .home
        .iter()
        .chain(&evaluation.system)
        .filter(|observed| observed.drv_path == candidate.drv_path)
        .cloned()
        .collect();
    let matching_locators: Vec<_> = all_locators
        .iter()
        .filter(|locator| same_package(&locator.package, package))
        .cloned()
        .collect();
    let locator_claims_other_app = matching_locators
        .iter()
        .any(|locator| locator.app_index.is_some_and(|index| index != app_index));
    let reviewed_for_this_app = matching_locators
        .iter()
        .any(|locator| locator.app_index == Some(app_index));
    if (!homepage_matches(app, candidate) && !reviewed_for_this_app)
        || locator_claims_other_app
    {
        return (
            Classification::AmbiguousIdentity,
            matching,
            matching_locators,
        );
    }
    if matching.is_empty() {
        return if matching_locators.is_empty() {
            (Classification::Absent, matching, matching_locators)
        } else {
            (
                Classification::AmbiguousIdentity,
                matching,
                matching_locators,
            )
        };
    }
    if has_non_nix_authority(app) {
        return (
            Classification::MultiAuthorityReview,
            matching,
            matching_locators,
        );
    }
    if matching_locators.is_empty() {
        return (
            Classification::OptionSupplied,
            matching,
            matching_locators,
        );
    }
    (Classification::ExactPresent, matching, matching_locators)
}

fn location(locator: &Locator) -> String {
    let line = locator
        .config_line
        .map(|line| format!(":{line}"))
        .unwrap_or_default();
    format!(
        "{}{} · {} · {}{}",
        locator.config_file,
        line,
        locator.config_expression,
        locator.scope,
        if locator.installer.is_empty() {
            String::new()
        } else {
            format!(" · {}", locator.installer)
        }
    )
}

pub(crate) fn report(
    app: &App,
    package: &str,
    evaluation: &Evaluation,
    classification: Classification,
    observed: &[EvaluatedPackage],
    locators: &[Locator],
) -> String {
    let result = classification_text(classification);
    let mut lines = vec![
        format!("app: {}", app.name),
        format!("candidate: {package}"),
        format!("result: {result}"),
    ];
    if let Some(candidate) = evaluation.candidate.as_ref() {
        lines.push(format!(
            "resolved: {} {} · {}",
            if candidate.pname.is_empty() {
                &candidate.name
            } else {
                &candidate.pname
            },
            if candidate.version.is_empty() {
                "version unreported"
            } else {
                &candidate.version
            },
            candidate.homepage
        ));
    }
    if !observed.is_empty() {
        let scopes: BTreeSet<_> = observed.iter().map(|item| item.scope.as_str()).collect();
        lines.push(format!(
            "evaluated in: {}",
            scopes.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    for locator in locators {
        lines.push(format!(
            "declared: {} ({})",
            location(locator),
            locator.app_name
        ));
    }
    lines.push("authority: read-only classification; tracked.nix was not created or changed".into());
    lines.join("\n")
}

pub(crate) fn classification_text(classification: Classification) -> &'static str {
    match classification {
        Classification::CandidateUnavailable => {
            "candidate unavailable in the configured package set"
        }
        Classification::AmbiguousIdentity => "ambiguous; no config change is safe",
        Classification::ExactPresent => "exact package already present",
        Classification::OptionSupplied => {
            "present through evaluated modules/options; no standalone locator recorded"
        }
        Classification::MultiAuthorityReview => {
            "multi-authority/version review; do not add a duplicate"
        }
        Classification::Absent => "not present in the evaluated configuration",
    }
}

fn record_discovery(
    ledger: &mut Ledger,
    index: usize,
    result: &str,
    candidates: &[String],
    selected: Option<(&str, &EvaluatedPackage, Classification)>,
) -> Result<()> {
    let mut doc = ledger.document();
    let app = doc["apps"]
        .as_array_of_tables_mut()
        .context("apps must be an array of tables")?
        .get_mut(index)
        .context("Application disappeared while recording Nix check")?;
    let mut receipt = Table::new();
    receipt["result"] = value(result);
    receipt["checked_at_unix"] = value(crate::update::timestamp());
    receipt["config_root"] = value("~/repos/config");
    receipt["configuration"] = value(
        std::env::var("APPTRACK_NIX_CONFIGURATION").unwrap_or_else(|_| "Sed".into()),
    );
    let mut listed = Array::new();
    for candidate in candidates {
        listed.push(candidate.as_str());
    }
    receipt["candidates"] = value(listed);
    if let Some((expression, package, classification)) = selected {
        receipt["package"] = value(expression);
        receipt["version"] = value(&package.version);
        receipt["homepage"] = value(&package.homepage);
        receipt["drv_path"] = value(&package.drv_path);
        receipt["classification"] = value(classification_text(classification));
    }
    app["nix_check"] = Item::Table(receipt);
    let _lock = ledger.write_lock()?;
    ledger.save_locked(doc)
}

fn candidate_description(candidate: &DiscoveredCandidate) -> String {
    let package = candidate.evaluation.candidate.as_ref().unwrap();
    format!(
        "{} {} · {}",
        candidate.expression,
        if package.version.is_empty() {
            "version unreported"
        } else {
            &package.version
        },
        package.homepage
    )
}

pub(crate) struct Selected {
    pub expression: String,
    pub evaluation: Evaluation,
    pub classification: Classification,
    pub observed: Vec<EvaluatedPackage>,
    pub locators: Vec<Locator>,
    pub candidate_lines: Vec<String>,
}

pub(crate) enum Selection {
    Unavailable,
    Ambiguous(Vec<String>),
    One(Selected),
}

pub(crate) fn select_candidate(
    ledger: &Ledger,
    index: usize,
    show_timer: bool,
    cancel: Option<&AtomicBool>,
    progress: &dyn Fn(&str),
) -> Result<(App, Selection)> {
    let app = ledger
        .apps
        .get(index)
        .context("Application no longer exists")?
        .clone();
    progress("Scanning configured stable and unstable Nix attributes…");
    let expressions = available_attribute_paths(&app, show_timer, cancel)?;
    if expressions.is_empty() {
        return Ok((app, Selection::Unavailable));
    }
    let all_locators = locators(ledger)?;
    let mut discovered = Vec::new();
    for expression in expressions {
        progress(&format!(
            "Verifying Nix candidate {expression} against evaluated packages…"
        ));
        let evaluation = evaluate(&expression, show_timer, cancel)?;
        if evaluation.candidate.is_some() {
            discovered.push(DiscoveredCandidate {
                expression,
                evaluation,
            });
        }
    }
    let candidate_lines: Vec<_> = discovered.iter().map(candidate_description).collect();
    let mut viable = Vec::new();
    for (position, candidate) in discovered.iter().enumerate() {
        let (classification, observed, matching_locators) = classify(
            index,
            &app,
            &candidate.expression,
            &candidate.evaluation,
            &all_locators,
        );
        if !matches!(
            classification,
            Classification::CandidateUnavailable | Classification::AmbiguousIdentity
        ) {
            viable.push((position, classification, observed, matching_locators));
        }
    }
    viable.sort_by_key(|(position, _, _, _)| {
        discovered[*position]
            .expression
            .starts_with("pkgs.unstable.")
    });
    let mut seen = BTreeSet::new();
    viable.retain(|(position, _, _, _)| {
        seen.insert(
            discovered[*position]
                .evaluation
                .candidate
                .as_ref()
                .unwrap()
                .drv_path
                .clone(),
        )
    });
    if viable.len() != 1 {
        return Ok((app, Selection::Ambiguous(candidate_lines)));
    }
    let (position, classification, observed, matching_locators) = viable.pop().unwrap();
    let selected = discovered.into_iter().nth(position).unwrap();
    Ok((
        app,
        Selection::One(Selected {
            expression: selected.expression,
            evaluation: selected.evaluation,
            classification,
            observed,
            locators: matching_locators,
            candidate_lines,
        }),
    ))
}

fn discover_inner(
    ledger: &mut Ledger,
    index: usize,
    show_timer: bool,
    cancel: Option<&AtomicBool>,
    progress: &dyn Fn(&str),
) -> Result<String> {
    ledger.check_unchanged()?;
    let (app, selection) = select_candidate(ledger, index, show_timer, cancel, progress)?;
    let selected = match selection {
        Selection::Unavailable => {
            progress("Recording unavailable Nix check evidence…");
            record_discovery(ledger, index, "unavailable", &[], None)?;
            return Ok(format!(
                "app: {}\nresult: no normalized candidate attribute in the configured stable or unstable package sets\nledger: durable Nix check evidence recorded\nauthority: Nix config unchanged; tracked.nix was not created",
                app.name
            ));
        }
        Selection::Ambiguous(candidate_lines) => {
            progress("Recording ambiguous Nix check evidence…");
            record_discovery(ledger, index, "ambiguous", &candidate_lines, None)?;
            let candidates = if candidate_lines.is_empty() {
                "none passed upstream identity verification".into()
            } else {
                candidate_lines.join("\n  ")
            };
            return Ok(format!(
                "app: {}\nresult: ambiguous candidate lookup; no config change is safe\ncandidates:\n  {}\nledger: durable Nix check evidence recorded\nauthority: Nix config unchanged; tracked.nix was not created",
                app.name, candidates
            ));
        }
        Selection::One(selected) => selected,
    };
    let package = selected.evaluation.candidate.as_ref().unwrap();
    progress("Recording verified Nix check evidence…");
    record_discovery(
        ledger,
        index,
        "matched",
        &selected.candidate_lines,
        Some((&selected.expression, package, selected.classification)),
    )?;
    Ok(format!(
        "{}\nledger: durable Nix check evidence recorded; Nix config unchanged",
        report(
            &app,
            &selected.expression,
            &selected.evaluation,
            selected.classification,
            &selected.observed,
            &selected.locators,
        )
    ))
}

pub fn discover(ledger: &mut Ledger, index: usize) -> Result<String> {
    discover_inner(ledger, index, true, None, &|_| {})
}

pub fn discover_tui(
    ledger: &mut Ledger,
    index: usize,
    cancel: &AtomicBool,
    progress: impl Fn(&str),
) -> Result<String> {
    discover_inner(ledger, index, false, Some(cancel), &progress)
}

pub fn check(ledger: &Ledger, index: usize, package: &str) -> Result<String> {
    ledger.check_unchanged()?;
    let app = ledger
        .apps
        .get(index)
        .context("Application no longer exists")?;
    let evaluation = evaluate(package, true, None)?;
    let locators = locators(ledger)?;
    let (classification, observed, matching_locators) =
        classify(index, app, package, &evaluation, &locators);
    Ok(report(
        app,
        package,
        &evaluation,
        classification,
        &observed,
        &matching_locators,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(source: &str, tags: &[&str]) -> App {
        toml::from_str(&format!(
            r#"identity = "https://github.com/example/tool"
name = "tool"
category = "Tools"
disposition = "using"
installed = true
tags = {tags:?}
[provenance]
source = "{source}"
repo = "https://github.com/example/tool"
managed_by_apptrack = false
"#
        ))
        .unwrap()
    }

    fn package(scope: &str, drv: &str, version: &str) -> EvaluatedPackage {
        EvaluatedPackage {
            drv_path: drv.into(),
            name: format!("tool-{version}"),
            pname: "tool".into(),
            version: version.into(),
            homepage: "https://github.com/example/tool".into(),
            main_program: "tool".into(),
            scope: scope.into(),
        }
    }

    fn evaluation(present: bool) -> Evaluation {
        let candidate = package("candidate", "/nix/store/tool.drv", "2.0");
        Evaluation {
            candidate: Some(candidate.clone()),
            home: present
                .then_some(package("home-manager", "/nix/store/tool.drv", "2.0"))
                .into_iter()
                .collect(),
            system: vec![],
        }
    }

    fn locator(app_index: Option<usize>) -> Locator {
        Locator {
            app_index,
            app_name: "tool".into(),
            package: "tool".into(),
            config_file: "~/repos/config/home/tools.nix".into(),
            config_line: Some(4),
            config_expression: "tool".into(),
            scope: "home-manager".into(),
            installer: "home-manager".into(),
        }
    }

    #[test]
    fn classifier_covers_exact_option_multi_absent_and_ambiguous() {
        let nix = app("nix", &[]);
        assert_eq!(
            classify(0, &nix, "tool", &evaluation(true), &[locator(Some(0))]).0,
            Classification::ExactPresent
        );
        assert_eq!(
            classify(0, &nix, "tool", &evaluation(true), &[]).0,
            Classification::OptionSupplied
        );
        let overlay = app("bun", &["multi-install"]);
        assert_eq!(
            classify(
                0,
                &overlay,
                "tool",
                &evaluation(true),
                &[locator(Some(0))]
            )
            .0,
            Classification::MultiAuthorityReview
        );
        assert_eq!(
            classify(0, &nix, "tool", &evaluation(false), &[]).0,
            Classification::Absent
        );
        assert_eq!(
            classify(0, &nix, "tool", &evaluation(false), &[locator(Some(0))]).0,
            Classification::AmbiguousIdentity
        );
    }

    #[test]
    fn mismatched_homepage_and_other_app_locator_fail_closed() {
        let app = app("nix", &[]);
        let mut wrong = evaluation(true);
        wrong.candidate.as_mut().unwrap().homepage = "https://github.com/other/tool".into();
        assert_eq!(
            classify(0, &app, "tool", &wrong, &[]).0,
            Classification::AmbiguousIdentity
        );
        assert_eq!(
            classify(0, &app, "tool", &evaluation(true), &[locator(Some(1))]).0,
            Classification::AmbiguousIdentity
        );
    }

    #[test]
    fn reviewed_same_app_locator_can_bind_a_project_homepage() {
        let app = app("nix", &[]);
        let mut project_site = evaluation(true);
        project_site.candidate.as_mut().unwrap().homepage = "https://example.dev".into();
        assert_eq!(
            classify(
                0,
                &app,
                "tool",
                &project_site,
                &[locator(Some(0))]
            )
            .0,
            Classification::ExactPresent
        );
    }

    #[test]
    fn nix_expression_uses_only_validated_attribute_paths() -> Result<()> {
        let expression =
            evaluation_expression("/tmp/config", "Sed", "john", "pkgs.unstable.tool")?;
        assert!(expression.contains("[ \"unstable\" \"tool\" ]"));
        assert!(
            evaluation_expression(
                "/tmp/config",
                "Sed",
                "john",
                "pkgs.tool; builtins.abort \"oops\""
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn automatic_lookup_matches_only_normalized_exact_stable_and_unstable_names() {
        let keys = BTreeSet::from(["projectgraph".into()]);
        let found = matching_attribute_paths(
            &keys,
            AttributeNames {
                stable: vec!["project-graph".into(), "project-graph-extra".into()],
                unstable: vec!["project_graph".into(), "other".into()],
            },
        );
        assert_eq!(found, ["pkgs.unstable.project_graph", "project-graph"]);
    }

    #[test]
    fn active_migration_receipt_is_a_locator_and_removed_is_not() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("ledger.toml");
        fs::write(
            &path,
            r#"schema_version = 1
[[apps]]
identity = "https://github.com/example/tool"
name = "tool"
category = "Tools"
disposition = "using"
[apps.provenance]
source = "unknown"
managed_by_apptrack = false
[apps.nix_migration]
package = "tool"
config_file = "~/repos/config/home/tracked.nix"
config_expression = "tool"
drv_path = "/nix/store/tool.drv"
migrated_at_unix = 1789831809

[[apps]]
identity = "https://github.com/example/removed"
name = "removed"
category = "Tools"
disposition = "using"
[apps.provenance]
source = "unknown"
managed_by_apptrack = false
[apps.nix_migration]
package = "removed"
config_file = "~/repos/config/home/tracked.nix"
config_expression = "removed"
drv_path = "/nix/store/removed.drv"
migrated_at_unix = 1789831809
removed_at_unix = 1789832832
"#,
        )?;
        let ledger = Ledger::open(&path)?;
        let found = locators(&ledger)?;
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].app_index, Some(0));
        assert_eq!(found[0].package, "tool");
        assert_eq!(found[0].scope, "tracked.nix");
        let app = &ledger.apps[0];
        assert_eq!(
            classify(0, app, "tool", &evaluation(true), &found).0,
            Classification::ExactPresent
        );
        Ok(())
    }

    #[test]
    fn durable_check_receipt_preserves_the_ledger_and_grants_no_authority() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("ledger.toml");
        fs::write(
            &path,
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

[[inbox.nix]]
name = "other"
package = "other"
config_file = "~/repos/config/home/tools.nix"
config_expression = "other"
"#,
        )?;
        let mut ledger = Ledger::open(&path)?;
        let package = package("candidate", "/nix/store/tool.drv", "2.0");
        record_discovery(
            &mut ledger,
            0,
            "matched",
            &["tool 2.0 · https://github.com/example/tool".into()],
            Some(("tool", &package, Classification::Absent)),
        )?;
        let text = fs::read_to_string(path)?;
        assert!(text.contains("# keep this note"), "{text}");
        assert!(text.contains("[[inbox.nix]]"), "{text}");
        assert!(text.contains("[apps.nix_check]"), "{text}");
        assert!(text.contains("drv_path = \"/nix/store/tool.drv\""), "{text}");
        assert!(!text.contains("managed_by_apptrack = true"), "{text}");
        Ok(())
    }
}
