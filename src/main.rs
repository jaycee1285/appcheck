mod archive;
mod android;
mod appimage_tree;
mod batch;
mod bun_strategy;
mod cargo_strategy;
mod cargo_intake;
mod codeberg_strategy;
mod dialog;
mod doctor;
mod flatpak_strategy;
mod gitlab_strategy;
mod intake;
mod ledger;
mod nix_strategy;
mod nix_discovery;
mod nix_migrate;
mod removal;
mod ui;
mod ui_task;
mod update;
mod work;

use anyhow::{Context, Result, bail, ensure};
use ledger::{Ledger, expand_path};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

struct SmokeFixture {
    _dir: tempfile::TempDir,
    ledger: PathBuf,
    bin: PathBuf,
    cargo_root: PathBuf,
    bun_root: PathBuf,
    flatpak_user: PathBuf,
    _appdirs: PathBuf,
    downloads: PathBuf,
}

fn prepare_smoke(source: &Path) -> Result<SmokeFixture> {
    let source_text = fs::read_to_string(source)
        .with_context(|| format!("Cannot read smoke source ledger {}", source.display()))?;
    let source_ledger = Ledger::open(source)?;
    let dir = tempfile::Builder::new()
        .prefix("apptrack-smoke-")
        .tempdir_in("/tmp")?;
    let bin = dir.path().join("bin");
    let downloads = dir.path().join("downloads");
    let cargo_root = dir.path().join("cargo");
    let bun_root = dir.path().join("bun");
    let flatpak_user = dir.path().join("flatpak-user");
    let appdirs = dir.path().join("appdirs");
    let nix_config = dir.path().join("nix-config/home");
    fs::create_dir(&bin)?;
    fs::create_dir(&downloads)?;
    fs::create_dir(&cargo_root)?;
    fs::create_dir(&bun_root)?;
    fs::create_dir(bun_root.join("bin"))?;
    fs::create_dir(&flatpak_user)?;
    fs::create_dir(&appdirs)?;
    fs::create_dir_all(&nix_config)?;
    let bin_text = bin
        .to_str()
        .context("Smoke binary directory is not valid UTF-8")?;
    let ledger = dir.path().join("apptrack.toml");
    let cargo_text = cargo_root
        .to_str()
        .context("Smoke Cargo root is not valid UTF-8")?;
    let bun_text = bun_root
        .to_str()
        .context("Smoke Bun root is not valid UTF-8")?;
    let mut smoke_text = source_text
            .replace("~/.local/bin/", &format!("{bin_text}/"))
            .replace("~/.cargo/", &format!("{cargo_text}/"))
            .replace("root = \"~/.cargo\"", &format!("root = \"{cargo_text}\""))
            .replace("~/.bun/", &format!("{bun_text}/"))
            .replace("root = \"~/.bun\"", &format!("root = \"{bun_text}\""))
            .replace(
                "~/.local/share/apptrack/appdirs/",
                &format!("{}/", appdirs.display()),
            )
            .replace("installation = \"system\"", "installation = \"user\"");
    let mut copied_configs: BTreeMap<String, PathBuf> = BTreeMap::new();
    let reviewed_configs = source_ledger.apps.iter().flat_map(|app| {
        app.provenance
            .config_file
            .iter()
            .zip(app.provenance.config_expression.iter())
            .map(|(file, _)| file.as_str())
            .chain(app.installations.iter().filter_map(|item| {
                item.config_expression.as_ref()?;
                item.config_file.as_deref()
            }))
            .chain(
                app.nix_migration
                    .iter()
                    .filter(|receipt| receipt.active())
                    .map(|receipt| receipt.config_file.as_str()),
            )
    });
    for (index, config_file) in reviewed_configs.enumerate() {
        let copied_nix_file = if let Some(copied) = copied_configs.get(config_file) {
            copied.clone()
        } else {
            let source_file = expand_path(config_file);
            let file_name = source_file.file_name().context("Nix config file has no file name")?;
            let copied = nix_config.join(format!("{index}-{}", file_name.to_string_lossy()));
            fs::copy(&source_file, &copied).with_context(|| {
                format!("Cannot copy reviewed Nix package list {} into the smoke fixture", source_file.display())
            })?;
            copied_configs.insert(config_file.to_string(), copied.clone());
            copied
        };
        smoke_text = smoke_text.replace(
            &format!("config_file = {config_file:?}"),
            &format!("config_file = {:?}", copied_nix_file.to_string_lossy()),
        );
    }
    fs::write(&ledger, smoke_text)?;
    // Parse the copy before starting the TUI; the source ledger remains untouched.
    Ledger::open(&ledger)?;
    Ok(SmokeFixture {
        _dir: dir,
        ledger,
        bin,
        cargo_root,
        bun_root,
        flatpak_user,
        _appdirs: appdirs,
        downloads,
    })
}

fn smoke(source: &Path) -> Result<()> {
    let fixture = prepare_smoke(source)?;
    eprintln!("Preparing isolated Flatpak smoke remote and appstream…");
    let remote = Command::new("flatpak")
        .args([
            "remote-add",
            "--user",
            "--if-not-exists",
            "--from",
            "flathub",
            "https://dl.flathub.org/repo/flathub.flatpakrepo",
        ])
        .env("FLATPAK_USER_DIR", &fixture.flatpak_user)
        .output()
        .context("Cannot initialize the temporary Flatpak smoke remote")?;
    ensure!(
        remote.status.success(),
        "Cannot initialize the temporary Flatpak smoke remote: {}",
        String::from_utf8_lossy(&remote.stderr).trim()
    );
    let appstream = Command::new("flatpak")
        .args([
            "update",
            "--user",
            "--appstream",
            "--noninteractive",
            "flathub",
        ])
        .env("FLATPAK_USER_DIR", &fixture.flatpak_user)
        .output()
        .context("Cannot fetch Flatpak appstream for the temporary smoke remote")?;
    ensure!(
        appstream.status.success(),
        "Cannot fetch Flatpak appstream for the temporary smoke remote: {}",
        String::from_utf8_lossy(&appstream.stderr).trim()
    );
    let status = Command::new(std::env::current_exe()?)
        .arg("--file")
        .arg(&fixture.ledger)
        .env("APPTRACK_BIN_DIR", &fixture.bin)
        .env("APPTRACK_DOWNLOADS", &fixture.downloads)
        .env("CARGO_INSTALL_ROOT", &fixture.cargo_root)
        .env("BUN_INSTALL", &fixture.bun_root)
        .env("BUN_INSTALL_BIN", fixture.bun_root.join("bin"))
        .env("FLATPAK_USER_DIR", &fixture.flatpak_user)
        .status()
        .context("Cannot start the AppTrack smoke TUI")?;
    ensure!(status.success(), "Smoke TUI exited with {status}");
    Ok(())
}

fn main() -> Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut path = std::env::var("APPTRACK_FILE").unwrap_or_else(|_| "apptrack.toml".into());
    if let Some(i) = args.iter().position(|a| a == "--file") {
        if i + 1 >= args.len() {
            bail!("--file needs a TOML path");
        }
        path = args.remove(i + 1);
        args.remove(i);
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "Track — personal application ledger\n\napptrack [--file PATH]                 Open TUI\napptrack [--file PATH] --smoke         Open TUI with ledger and install targets isolated under /tmp\napptrack [--file PATH] <app> doctor    Diagnose one app\napptrack [--file PATH] <app> check     Read-only release plan\napptrack [--file PATH] <app> update    Plan, confirm, install\napptrack [--file PATH] <app> nix-check\n                                           Discover candidate and record evidence\napptrack [--file PATH] <app> nix-check PACKAGE\n                                           Classify one package without saving\napptrack [--file PATH] <app> nix-migrate\n                                           Confirm and add one verified absent package to tracked.nix\napptrack [--file PATH] <app> nix-remove\n                                           Confirm and remove a migrated declaration from tracked.nix\napptrack [--file PATH] check          Global plan; installs nothing, records failed checks (Using)\napptrack [--file PATH] update         Confirm global update (Using)\n  --include-considering               Also include Considering in global operations\napptrack [--file PATH] list            Category counts\n\nDirect Go installs: out of scope · Nix migration: landed\nAPPTRACK_FILE overrides ./apptrack.toml; APPTRACK_DOWNLOADS overrides ~/Downloads staging.\nIn the TUI: arrows browse, Enter details, / search, a add, U using, C considering, x archive, o launch, u update, n Nix lookup, m Nix migrate, g global update, G include Considering, c source check, r release check, R reload, ? help, q quit."
        );
        println!(
            "\napptrack [--file PATH] add [URL]       Add/configure from GitHub\napptrack [--file PATH] <app> source    Review release and Cargo routes\napptrack [--file PATH] <app> install   Install one reviewed recipe; offer Using promotion\na/A: Add; e: edit selected record; c: check source routes; r: check release artifacts; R: reload; i: install selected. Ctrl-S saves record only.\nAPPTRACK_BIN_DIR overrides ~/.local/bin for new intake recipes."
        );
        return Ok(());
    }
    if args == ["--version"] {
        println!("apptrack {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args == ["--smoke"] || args == ["smoke"] {
        return smoke(&expand_path(&path));
    }
    let mut ledger = Ledger::open(&expand_path(&path))?;
    match args.as_slice() {
        [] => ui::run(&mut ledger),
        [command] if command == "add" => {
            println!("{}", intake::interactive(&mut ledger, None, "")?);
            Ok(())
        }
        [command, url] if command == "add" => {
            println!("{}", intake::interactive(&mut ledger, Some(url), "")?);
            Ok(())
        }
        [command] if command == "check" || command == "update" => {
            if command == "check" {
                let batch = batch::check(&ledger, false)?;
                batch.print();
                batch::record_check_failures(&mut ledger, &batch, |s| println!("{s}"));
            } else {
                println!("{}", batch::interactive(&mut ledger, false)?);
            }
            Ok(())
        }
        [command, flag]
            if (command == "check" || command == "update") && flag == "--include-considering" =>
        {
            if command == "check" {
                let batch = batch::check(&ledger, true)?;
                batch.print();
                batch::record_check_failures(&mut ledger, &batch, |s| println!("{s}"));
            } else {
                println!("{}", batch::interactive(&mut ledger, true)?);
            }
            Ok(())
        }
        [command] if command == "list" => {
            for category in ui::categories(&ledger) {
                let counts = ui::counts(&ledger, &category);
                println!(
                    "{category:28} U {:>3}   C {:>3}   A {:>3}",
                    counts[0], counts[1], counts[2]
                );
            }
            Ok(())
        }
        [query, command] if command == "doctor" => {
            let i = ledger.find(query)?;
            println!("{}", doctor::report(&ledger.apps[i], &ledger.record(i)));
            Ok(())
        }
        [query, command] if command == "source" => {
            let index = ledger.find(query)?;
            println!("{}", intake::configure_source(&mut ledger, index, &dialog::Console)?);
            Ok(())
        }
        [query, command] if command == "nix-check" => {
            let index = ledger.find(query)?;
            println!("{}", nix_discovery::discover(&mut ledger, index)?);
            Ok(())
        }
        [query, command, package] if command == "nix-check" => {
            let index = ledger.find(query)?;
            println!("{}", nix_discovery::check(&ledger, index, package)?);
            Ok(())
        }
        [query, command] if command == "nix-migrate" => {
            let index = ledger.find(query)?;
            println!(
                "{}",
                nix_migrate::migrate(&mut ledger, index, &dialog::Console, true, None)?
            );
            Ok(())
        }
        [query, command] if command == "nix-remove" => {
            let index = ledger.find(query)?;
            println!("{}", nix_migrate::unmigrate(&mut ledger, index, &dialog::Console)?);
            Ok(())
        }
        [query, command] if command == "check" => {
            let index = ledger.find(query)?;
            println!(
                "{}",
                batch::check_one(
                    &ledger,
                    index,
                    &std::sync::atomic::AtomicBool::new(false)
                )?
                .summary()
            );
            Ok(())
        }
        [query, command] if command == "install" || command == "i" => {
            let index = ledger.find(query)?;
            println!("{}", dialog::install_one(&mut ledger, index, &dialog::Console)?);
            Ok(())
        }
        [query, command] if command == "update" => {
            let index = ledger.find(query)?;
            println!(
                "{}",
                dialog::updates(&mut ledger, Some(index), false, &dialog::Console)?
            );
            Ok(())
        }
        _ => bail!("Unknown command. Run apptrack --help"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoke_fixture_redirects_local_cargo_bun_and_flatpak_without_touching_source() -> Result<()> {
        let source_dir = tempfile::tempdir()?;
        let source = source_dir.path().join("ledger.toml");
        let text = r#"schema_version = 1
[[apps]]
identity = "name:tool"
name = "tool"
category = "Tools"
disposition = "considering"
installed_paths = ["~/.local/bin/tool"]
[apps.launch]
program = "~/.local/bin/tool"
args = []
gui = false
[apps.recipe]
source = "cargo"
installer = "cargo-install"
package = "tool"
registry = "crates-io"
root = "~/.cargo"
bins = ["tool"]

[[apps]]
identity = "npm:@scope/bun-tool"
name = "bun-tool"
category = "Tools"
disposition = "considering"
[apps.recipe]
source = "bun"
installer = "bun-global"
package = "@scope/bun-tool"
registry = "https://registry.npmjs.org"
root = "~/.bun"
bins = ["bun-tool"]

[[apps]]
identity = "flatpak:org.example.Tool"
name = "flat-tool"
category = "Tools"
disposition = "considering"
[apps.recipe]
source = "flatpak"
installer = "flatpak"
package = "org.example.Tool"
remote = "flathub"
remote_url = "https://dl.flathub.org/repo/"
installation = "system"
arch = "x86_64"
branch = "stable"

[[apps]]
identity = "https://gitlab.com/example/tree-tool"
name = "tree-tool"
category = "Tools"
disposition = "considering"
[apps.launch]
program = "appimage-run"
args = ["-w", "~/.local/share/apptrack/appdirs/tree-tool"]
gui = true
[apps.recipe]
source = "gitlab"
repo = "example/tree-tool"
asset = "tree-tool-{version}.AppImage"
installer = "appimage-appdir"
destination = "~/.local/share/apptrack/appdirs/tree-tool"
os = "linux"
arch = "x86_64"
"#;
        fs::write(&source, text)?;

        let fixture = prepare_smoke(&source)?;
        let copied = fs::read_to_string(&fixture.ledger)?;
        assert_eq!(fs::read_to_string(&source)?, text);
        assert!(!copied.contains("~/.local/bin/"));
        assert!(!copied.contains("~/.cargo"));
        assert!(!copied.contains("~/.bun"));
        assert!(!copied.contains("installation = \"system\""));
        assert!(!copied.contains("~/.local/share/apptrack/appdirs/"));
        assert!(copied.contains("installation = \"user\""));
        assert!(copied.contains(&format!("{}/tool", fixture.bin.display())));
        assert!(copied.contains(&format!("root = \"{}\"", fixture.cargo_root.display())));
        assert!(copied.contains(&format!("root = \"{}\"", fixture.bun_root.display())));
        assert!(fixture.bin.is_dir());
        assert!(fixture.cargo_root.is_dir());
        assert!(fixture.bun_root.is_dir());
        assert!(fixture.bun_root.join("bin").is_dir());
        assert!(fixture.flatpak_user.is_dir());
        assert!(fixture._appdirs.is_dir());
        assert!(copied.contains(&format!("{}/tree-tool", fixture._appdirs.display())));
        assert!(fixture.downloads.is_dir());
        Ok(())
    }

    #[test]
    fn smoke_fixture_copies_reviewed_nix_config_instead_of_editing_source() -> Result<()> {
        let source_dir = tempfile::tempdir()?;
        let config = source_dir.path().join("packages.nix");
        let config_text = "{ pkgs, ... }:\n{\n  home.packages = [\n    pkgs.tool\n  ];\n}\n";
        fs::write(&config, config_text)?;
        let source = source_dir.path().join("ledger.toml");
        fs::write(
            &source,
            format!(
                r#"schema_version = 1
[[apps]]
identity = "nix:tool"
name = "tool"
category = "Tools"
disposition = "considering"
[apps.provenance]
source = "nix"
installer = "home-manager"
package = "pkgs.tool"
config_file = {config:?}
config_expression = "pkgs.tool"
managed_by_apptrack = false
"#,
                config = config.to_string_lossy(),
            ),
        )?;
        let fixture = prepare_smoke(&source)?;
        let ledger = Ledger::open(&fixture.ledger)?;
        let copied = expand_path(ledger.apps[0].provenance.config_file.as_deref().unwrap());
        assert_ne!(copied, config);
        assert!(copied.starts_with(fixture.ledger.parent().unwrap()));
        assert_eq!(fs::read_to_string(copied)?, config_text);
        assert_eq!(fs::read_to_string(config)?, config_text);
        Ok(())
    }
}
