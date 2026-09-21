# Track

A personal application ledger in Rust and Ratatui. The spec is in
[apptrack.md](apptrack.md); remaining work is in [TASKBOARD.md](TASKBOARD.md).

```bash
cd /home/john/repos/apptrack && cargo run --offline -- --file apptrack.toml
```

For a disposable human smoke run with the ledger, downloads, and local-bin
destinations redirected to `/tmp`:

```bash
cd ~/repos/apptrack && cargo run --offline -- --smoke
```

The first slice opens a real, reviewable bootstrap ledger: 118 applications from
the local trial notes, all ten personal apps in `tauri.nix`, the three coding
agents, and explicit archive decisions in the master list. The remaining master
list migration is on the taskboard. AppTrack never parses those Markdown files.

Categories begin collapsed. Right expands Using and Considering immediately;
Archived has its own disclosure. Counts use your terminal's green/yellow/red;
the rest uses the terminal's default foreground/background.

Each app-name cell ends with a fixed two-character presence marker: `\/` when
all recorded installed paths currently exist, and `[]` when they do not. A
pathless Flatpak uses `\/` only after an AppTrack-managed installed receipt;
imported or otherwise unverified claims remain `[]`.

The header shows total `U | C | A` counts, with unlabelled category counts aligned
beneath them. The map fits in 35 columns with the current totals: enough for
`Files & Disks`. Longer names shorten with an ellipsis and expand as space allows;
the TOML keeps the full names. Below the minimum size, a resize message replaces
the view and editing pauses. Wider totals automatically increase the minimum.

Open the built app in a compact Kitty window:

```bash
kitty -T Track -o remember_window_size=no -o initial_window_width=35c -o initial_window_height=26c bash -c '/home/john/repos/apptrack/target/debug/apptrack --file /home/john/repos/apptrack/apptrack.toml; exec bash'
```

The category view needs at least eight rows; forms and help request enough height
to display their controls. Window sizing remains subject to the compositor.
This launcher leaves a Bash prompt in the Kitty window when Track exits.

| Key | Action |
| --- | --- |
| Up / Down, j / k | Move |
| Right / Left | Expand / collapse or return to category |
| Enter | Inspect the full record and filesystem observations |
| / | Fuzzy search names, descriptions, categories and tags, including archives |
| Enter / Esc in search | Browse results / clear filter |
| a / A | Unified add / complete-recipe dialog |
| U / C | Mark Using / Considering |
| x | Archive with a required reason; optionally remove an exact managed installation |
| o | Run the explicit launch recipe |
| u | Check release, show plan, confirm installation |
| n | Check Nixpkgs for the selected known app and record evidence |
| g / G | Global update: Using / Using plus Considering |
| r | Reload after hand-editing the TOML |
| ? | Show the full key guide |
| Esc / q | Back / quit (q closes a detail view first) |

When a background action reaches its result screen, Enter returns to Track and
Esc exits to Bash.

CLI diagnostics work without a terminal:

```bash
cargo run --offline -- ttt doctor
cargo run --offline -- ai-usagebar doctor
cargo run --offline -- list
```

Use `--file /path/to/apptrack.toml` or `APPTRACK_FILE` to choose another ledger.
The default is `./apptrack.toml`; nothing silently creates another database.
The accepted bootstrap lives here until a Syncthing location is chosen.

## Ledger contract

`identity` names a logical application, while `installed_paths` can contain several
artifacts. Repository identities take priority; unknown upstreams use `name:…`.
Disposition, installed state and outcome are independent. Omitted `installed` or
`version` means unknown. Download/configuration versions stay in `apps.evidence`;
they are not promoted to installed versions. Cargo receipts and installed package
metadata are named as version evidence where available.

A displayed `[[apps]]` record requires `identity`, `name`, `category`, and an
explicit `disposition` of `using`, `considering`, or `archived`. `description` is
the brief account of what the app is; `archived_because` is the short reason an
Archived app lost. `review` remains available for broader context. Archived
records do not require install, version, outcome, path, provenance, launch, or
recipe fields.

Incomplete research can live under ignored `[[inbox.nix]]` or
`[[inbox.appimage]]` tables. AppTrack preserves those tables and their comments
through its own saves but does not display or validate their fields. Moving an
entry into `[[apps]]` is the explicit display-on boundary. Inbox entries should
still record disposition so intent survives fold-in; missing inbox fields are an
agentic-entry-check concern, while invalid TOML syntax still prevents the ledger
from opening.

Edits retain comments, spacing, and unknown fields using `toml_edit`. Saves use a
temporary file in the ledger directory and atomic replacement, with a save lock
and an external-change check. A stale write leaves your external edits intact;
reload with `r` and retry. The `.toml.lock` file can remain on disk; the OS lock is
released when the save finishes. This does not replace Syncthing conflict handling.

Adding new records starts them in Considering with installation unknown. Both `a`
and `A` open the same form: Enter sets up a GitHub recipe; Ctrl-S saves only the
record. With no upstream URL, Enter also saves a record-only entry.
Hand-edit additional verified provenance, paths, outcome or optional tags in TOML.
One explicit launch recipe looks like:

```toml
[apps.launch]
program = "~/.local/bin/ttt-linux-amd64"
args = []
gui = false
```

The launch command is an executable plus an argument array, not a shell string.
Terminal apps take over the terminal and return after Enter; GUI launches return
immediately. GUI child output is suppressed, and startup is not proof of health.
No inferred launch command is used for an unknown app.

`doctor` checks only recorded paths: missing files, non-files and absent execute
bits. It does not run binaries or claim to validate dynamic libraries, installed
versions, or application behavior. It prints the complete record, including any
additional recipe/history fields, for human or agent diagnosis.

## Add from GitHub

Press `a` or `A`. Tab to the upstream field and paste a GitHub repository URL
(or `owner/repo`), then Enter. Name can be blank for GitHub intake. Category is
prefilled from your current position and locked: focus that field, press `→`,
then choose from the existing categories, with `<custom>` as the final entry.
Typing/Backspace does not alter the locked field; Esc leaves its value unchanged.
The CLI version of the flow is also available directly from Bash:

```bash
cargo run --offline -- add https://github.com/TysonLabs/lazyide
cargo run --offline -- add https://github.com/paradise-runner/toast
```

For a new app, an omitted name or description is filled from GitHub. If the repo is already
tracked without a recipe, Track completes that record instead; existing metadata,
decisions, notes and installation evidence stay intact. Already-configured records
are left alone. Archived records must be moved to Considering first.

Choose a release asset; the sole filename match for this machine is offered as a
default, but nothing is selected without Enter. Selection downloads the asset for
inspection. For archives, choose from the contained regular ELF executables that
match the machine. Confirm a clean command name and terminal/GUI launch mode.
No archive contents or application code are executed during inspection.

The final review offers Save recipe only, Save and install, or Cancel. Use arrows
to select and Enter to confirm; no save/install action is preselected. Saving a new record and recipe
is atomic. Installing uses the inspected download and the existing verification,
rollback and receipt flow. An installation failure leaves the saved recipe for
retry; it does not mark the app installed. Existing installation state is retained.

For a previously installed app such as toast, save-only keeps the old launch path.
The installation plan shows its move to the clean name; a successful receipt
switches the launch, retaining the old executable and recorded path. Custom launch
wrappers require explicit configuration instead of an inferred rewrite.

Esc cancels a TUI prompt or requests that background work stop. CLI prompts use
`q`/EOF instead. Cancellation before saving leaves the ledger/executable unchanged;
normal prompt cancellation removes the temporary inspection download. Inspection
errors retain it at the displayed path. The default destination
directory is `~/.local/bin`; `APPTRACK_BIN_DIR` overrides it for new recipes and
must already exist. `APPTRACK_DOWNLOADS` controls download staging as usual.

Add-dialog inputs are GitHub repositories with direct Linux ELF binaries,
single-binary tar.gz/tar.xz/ZIP archives, or Type-2 AppImages. Use Ctrl-S in Add
for record-only entries when no supported release exists. Cargo, Bun, and Flatpak
recipes are currently hand-edited; local-file intake remains outside this slice.

## Release installation

`u` on an application opens the release planner inside Ratatui. Asset/member
selection, clean-name options, confirmation, progress and results stay themed.
Use PgUp/PgDn to inspect long plans. At the result screen, Enter returns to Track;
Esc exits to Bash. The same applies to `g` / `G`. Terminal applications launched
with `o` still intentionally take over the terminal; CLI commands still use text.

```bash
cargo run --offline -- ttt check    # Read-only release plan
cargo run --offline -- ttt update   # Plan, confirm once, install
```

A direct Linux ELF binary is selected by an exact GitHub release asset name:

```toml
[apps.recipe]
source = "github"
repo = "eugenioenko/ttt"
asset = "ttt-linux-amd64"
installer = "binary-copy"
clean_filename = true
destination = "~/.local/bin/ttt-linux-amd64"
os = "linux"
arch = "x86_64"
```

`clean_filename = true` makes the plan target `ttt` automatically. Release asset
selection keeps its exact platform-specific filename. Cleanup removes complete
trailing platform tokens, preserving names such as `docx2md` and version digits.
After installation succeeds, Track saves the clean destination for future updates
and switches a direct launch recipe that pointed to the old name. Existing clean
targets are refused. The old artifact remains recorded; naming cleanup does not
grant permission to delete it. Failed/cancelled installs leave the recipe unchanged.

### Archive binaries

`tar.gz`, `tar.xz` and `zip` recipes install one explicitly named Linux ELF binary.
Keepkit is the first configured archive recipe:

```toml
[apps.recipe]
source = "github"
repo = "stanlyzoolo/keepkit"
asset = "keepkit_{tag}_linux_x86_64.tar.gz"
installer = "tar.gz"
member = "keepkit"
destination = "~/.local/bin/keepkit"
os = "linux"
arch = "x86_64"
```

The optional `{tag}` placeholder expands to the full release tag, including `v`;
`{version}` removes one leading `v`. Intake proposes a placeholder only for a
delimiter-bounded release token, preserving application-name digits.
It still must resolve to exactly one asset; globs and guessed architecture matches
are not supported. The plan shows the resolved archive, member and destination.
`member` can name a nested path such as `bin/tool`, but must match exactly.

Track verifies the downloaded archive before extracting only the named regular
file into a fixed staging path. No archive directories, links, permissions, scripts,
or other payload files are installed. Missing/duplicate members, links, malformed
archives and wrong ELF architecture fail before replacement. ZIPs with duplicate
filenames are refused. The extracted binary is limited to 512 MiB; tar scanning is
limited to 2 GiB of expanded data. ZIP supports stored and deflate-compressed files.

Receipts record `asset_sha256` for the download and `sha256` for the installed
binary, along with the exact member. Both are checked when deciding whether the
installation is current. Bundles needing companion files are outside this slice.

Track uses `gh` and its existing authentication. Missing/ambiguous assets, wrong
platform recipes and symlink destinations are rejected. Destination directories
must already exist. Archived apps cannot update until moved back to Considering.
The release's exact asset ID is captured in the plan; no latest lookup happens
after confirmation. This follows GitHub's latest stable release selection.

Downloads stage in a unique directory under `~/Downloads` (`APPTRACK_DOWNLOADS`
overrides it). Size and ELF architecture are checked, along with the published
SHA-256 when available. If no digest is published, the plan says so. Preparation
finishes before replacement; failed preparations retain staging for inspection.
Successful staging and the temporary previous copy are removed automatically.

Track saves the installed version, hash, asset and exact managed path, retains the
old provenance under `[[apps.provenance_history]]`, and records `apps.last_action`.
Disposition, outcome and review remain independent. Installation success does not
claim the application runs correctly. If saving the receipt fails, Track restores
the prior executable. This handles ordinary errors; executable and ledger writes
are separate filesystem operations, not an atomic pair across process/power loss.

Unchanged files matching their Track receipt and the latest published checksum
report up to date. Without a published checksum, Track offers a verified-size and
architecture-checked download even when the release tag is unchanged.
An external change after planning rejects that plan. Imported paths alone still
grant no removal authority. Archive reviews persist without uninstalling.

Archiving a direct forge binary installed by AppTrack offers removal with “keep
installed” selected by default. Track requires the provenance receipt's one exact
managed path and installed SHA-256, refuses links or changed files, and renames the
executable into same-filesystem staging before saving the absence receipt. A
receipt failure restores the staged executable. Older imported paths are never
silently folded into that removal authority.

## Global updates

`g` checks Using applications; `G` includes Considering. Archived is always excluded.

```bash
cargo run --offline -- check
cargo run --offline -- check --include-considering
cargo run --offline -- update --include-considering
```

Track checks all eligible recipes, reports current/skipped/failed checks, and shows
the exact release, asset and destination for each planned update before asking once
for the batch. Missing or unsupported recipes are skipped and grouped by reason.
Conflicting destination paths are excluded. An individual install failure is recorded
and the batch continues; an external ledger change stops remaining installations.
The final summary distinguishes successful, failed and unattempted installations.
Each installation is committed independently; a later failure does not roll back
earlier successful apps. Declining the initial batch confirmation happens before
any download or installation.

Release lookups and downloads run concurrently across distinct repositories with
no global worker cap; users of the same repository serialize. After confirmation,
each verified download enters the single-writer install/receipt loop as it finishes,
without waiting for the slowest repository. No network worker mutates the ledger.
The UI receives progress events without blocking keyboard input. Esc prevents
remaining same-repository work from starting and cancels/reaps in-flight `gh`
processes; a commit already underway finishes before stopping. Completed
saves/installs are retained. Each GitHub operation has a 120-second deadline.
Per-ledger file locks and stale-write checks remain in force; this is not a lock
shared by unrelated ledger files targeting the same executable.

TTT and Keepkit have operational recipes. While both remain Considering, use `G`
to include them; `g` may report only skipped apps.

### Cargo registry packages

Cargo recipes use exact package-manager identity rather than a GitHub repository:

```toml
[apps.recipe]
source = "cargo"
installer = "cargo-install"
package = "ai-usagebar"
registry = "crates-io"
root = "~/.cargo"
bins = ["ai-usagebar", "ai-usagebar-tui"]
```

`u`, `g`, and `G` ask Cargo for the current registry version and read the selected
root's Cargo installation receipt. The review names the registry, package, exact
version, root, and every binary before confirmation. Installation passes that
reviewed version back to `cargo install`, then requires Cargo's receipt and every
configured binary to agree before AppTrack saves its own receipt and hashes.

AppTrack snapshots the selected binaries plus `.crates.toml` and `.crates2.json`
before Cargo mutates them. A command, verification, or ledger-save failure restores
those files. `--smoke` rewrites Cargo roots into the temporary fixture, so Cargo
testing cannot touch the live installation. The bootstrap contains exact recipes
for croft, ai-usagebar, ghr, and officemd; they remain unmanaged until a confirmed
AppTrack install succeeds.

Archiving an AppTrack-managed Cargo package offers exact `cargo uninstall` with
“keep installed” selected by default. Authority requires the recorded crates.io
package, root, version, complete Cargo bin receipt, command paths, and hashes all
to agree. The same binaries and Cargo metadata files are snapshotted first and
restored if uninstall, absence verification, or the AppTrack receipt fails.

### Bun global packages

Bun recipes identify the exact npm registry, package, Bun installation root, and
commands:

```toml
[apps.recipe]
source = "bun"
installer = "bun-global"
package = "@openai/codex"
registry = "https://registry.npmjs.org"
root = "~/.bun"
bins = ["codex"]
```

The plan resolves one registry version with `bun info` and reads the installed
package's `package.json`; neither operation relies on the mutable global lockfile.
After confirmation, the exact reviewed `package@version` is passed to Bun. Track
then requires matching package metadata, the declared bin mapping, command
symlinks that resolve inside that package, and one hash per command before saving
its receipt. A launch path inherited from the imported installed path moves to
Bun's managed `root/bin` command only after success.

`--smoke` supplies a temporary `BUN_INSTALL`, so the package tree, lockfile, and
command symlinks cannot touch `~/.bun`. Codex and pi have exact bootstrap recipes;
their imported installations remain unmanaged until a confirmed Track install.

Archiving an AppTrack-managed Bun package offers exact `bun remove --global`
with “keep installed” selected by default. Authority requires the recorded npm
package, registry, root, version, complete bin mapping, contained command
symlinks, paths, and hashes all to agree. If removal mutates the installation but
absence verification or the AppTrack receipt fails, AppTrack reinstalls the
exact recorded version and verifies the original command hashes.

### Flatpak applications

Flatpak recipes preserve the complete application authority rather than only an
application ID:

```toml
[apps.recipe]
source = "flatpak"
installer = "flatpak"
package = "io.github.suchnsuch.Tangent"
remote = "flathub"
remote_url = "https://dl.flathub.org/repo/"
installation = "system"
arch = "x86_64"
branch = "stable"
```

The plan verifies that the named remote maps to the recorded repository URL, then
resolves the exact `app/ID/ARCH/BRANCH` ref and OSTree commit. It separately reads
the chosen user or system installation and refuses an origin mismatch. Confirmation
updates an existing ref to the reviewed commit or installs an absent ref, verifies
the resulting origin and commit, and only then records the receipt and a
`flatpak run` launch command. Verification or receipt failure restores the previous
commit, or removes a newly installed ref without deleting its application data.

`--smoke` rewrites system scope to a temporary user installation under `/tmp`,
adds an isolated Flathub remote, and fetches its appstream version metadata before
opening the TUI. The bootstrap Tangent
record carries the first exact recipe; its observed system installation remains
unmanaged until a confirmed live install.

Flatpak's `Version` metadata is stored as the readable ledger version while the
full OSTree commit remains in provenance as the exact authority. Archiving a
Flatpak installed by AppTrack saves the reason first, then offers removal with
“keep installed” selected by default. Removal rechecks the receipt's origin and
commit, uninstalls only its full scoped ref, verifies absence, and records success.
Imported or otherwise unmanaged Flatpaks are archived without an uninstall offer.

### Nix configuration declarations

Nix checks keep declaration and realization separate. A reviewed declaration may
be absent while its executable remains in the current profile; the map shows `iP`
until the human runs their normal rebuild and AppTrack later observes convergence.
AppTrack never runs NixOS or Home Manager realization commands.

Config-edit authority is restricted to reviewed files below
`~/repos/config/home`. It covers the literal `home.packages` shapes in current use:
ordinary multiline lists, `with pkgs` multiline lists, and one-line single-item
lists. Entries may therefore be bare package names, qualified `pkgs.*` attributes,
flake-input expressions, or local `apps.*` attributes, but each must still match
one unique exact expression. The receipt names the exact config file, expression,
and Home Manager installer. AppTrack shows that source line, defaults to keeping
it, atomically edits only after confirmation, and restores it if saving the
AppTrack receipt fails. System modules, unknown, ambiguous, moved, definition-only,
or otherwise changed forms remain record-only archives. `--smoke` copies every
reviewed config file into its `/tmp` fixture before edits are offered.

Apps tagged `multi-install` preserve Nix as a recovery floor while separately
recording the faster-moving installation actually selected on PATH. Their
`[[apps.installations]]` entries retain source-specific versions, paths, packages,
and an independent Nix config locator. Doctor reports the greatest installed
numeric version as effective and treats an older Nix copy as expected rather than
as drift. Removing a reviewed Nix declaration does not claim the selected Bun or
native overlay absent.

V2 begins with a read-only collision classifier:

```bash
apptrack fibbo nix-check
apptrack codex nix-check codex
apptrack project-graph nix-check pkgs.unstable.project-graph
```

Without a package argument, AppTrack checks an existing reviewed Nix locator or
scans the configured stable and unstable attribute names for exact normalized
matches to the known app/repository name. It verifies candidate identity, records
durable `[apps.nix_check]` evidence, and never edits Nix configuration. Supplying
an explicit package performs the same collision classification without saving a
receipt.

Both forms compare the resolved derivation with the fully evaluated Home Manager
and system package lists, then reconcile that result with tracked locators and
`[[inbox.nix]]`. They report exact existing declarations, module/option-supplied
packages, multi-authority review, absence, unavailable candidates and ambiguous
identity. Neither command can create or edit `tracked.nix`.

Direct `go install` remains out of scope. Go applications distributed as supported
forge binaries or archives still use the ordinary forge lifecycle.

### AppImages

Forge AppImage recipes use `installer = "appimage"` with one exact, versioned
asset filename. GitHub uses its release asset API; GitLab uses the stable release
permalink and one exact asset link; Codeberg uses Forgejo's stable-release API
and one exact asset URL. `{version_underscores}` covers upstream filenames that
spell a tag such as `v0.7.1` as `v0_7_1`. Track verifies the reported size and any
published checksum, 64-bit Linux ELF
architecture, and Type-2 `AI 02` magic without launching the application. A
successful plain-AppImage install records the artifact hash and
`appimage_type = 2`, makes the managed file executable, and uses the same atomic
replacement and rollback guarantees as direct binaries.

On NixOS, AppImage launch recipes call `appimage-run` with the managed artifact;
AppTrack expands a leading `~/` in launch arguments before spawning it.

Archiving removes only an unchanged AppTrack-managed Type-2 AppImage after
confirmation. Imported AppImages grant no removal authority. Extracted AppDir
trees and NixOS library overrides are a separate compatibility layer and are not
silently treated as one regular file.

`installer = "appimage-appdir"` instead installs the extracted tree into a
managed AppDir and launches it with `appimage-run -w`. The receipt hashes every
relative path, file mode, file body, directory, and symlink. Optional
`disable_libs` entries are limited to `wayland`, `gl`, `gtk`, `webkit`, and
`fonts`; each invokes the reviewed reversible helper against the prepared tree.
Updates replace the tree recoverably, and removal requires the complete tree hash
and recorded library-family choices to remain unchanged.

## Human check

The map, decisions, delivered bootstrap, TTT installation, global updates, clean
filenames, archive installers, unified background dialogs, first Nix config edit,
and the GitHub/GitLab AppImage lifecycles are accepted. Codeberg Fibbo is the
current forge-adapter check; see [TASKBOARD.md](TASKBOARD.md).
