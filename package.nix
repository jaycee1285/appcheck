{
  lib,
  rustPlatform,
  pkg-config,
}:

rustPlatform.buildRustPackage {
  pname = "apptrack";
  version = "0.1.0";

  src = lib.fileset.toSource {
    root = ./.;
    fileset = lib.fileset.unions [
      ./Cargo.toml
      ./Cargo.lock
      ./src
    ];
  };

  cargoLock.lockFile = ./Cargo.lock;
  nativeBuildInputs = [ pkg-config ];

  # This one integration assertion deliberately reads John's live Home Manager
  # files. The ordinary repo gate runs it; Nix's isolated builder cannot see
  # /home/john/repos/config, so retain every other test here.
  checkFlags = [
    "--skip"
    "nix_strategy::tests::every_reviewed_bootstrap_nix_expression_matches_live_home_config"
  ];

  meta = {
    description = "Personal application ledger for decisions, provenance, and updates";
    license = lib.licenses.mit;
    mainProgram = "apptrack";
    platforms = lib.platforms.linux;
  };
}
