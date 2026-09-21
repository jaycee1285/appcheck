{
  description = "AppTrack — personal application ledger (TUI) and the appcheck launcher";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
    in
    {
      packages = forAllSystems (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.callPackage ./package.nix { };
        });

      # Builds apptrack against the consumer's pkgs at nixos-rebuild time and adds
      # the 87x27 Kitty `appcheck` launcher. Config: imports = [ appcheck.homeManagerModules.default ];
      homeManagerModules.default = ./home-manager.nix;

      devShells = forAllSystems (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.mkShell {
            packages = with pkgs; [
              cargo
              rustc
              rust-analyzer
              clippy
              pkg-config
            ];
          };
        });
    };
}
