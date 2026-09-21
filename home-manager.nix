{ pkgs, ... }:

let
  apptrack = pkgs.callPackage ./package.nix { };
  appcheck = pkgs.writeShellScriptBin "appcheck" ''
    ledger="''${APPTRACK_FILE:-$HOME/.config/apptrack/config.toml}"
    if [ ! -f "$ledger" ]; then
      echo "AppTrack ledger not found: $ledger" >&2
      exit 1
    fi

    exec ${pkgs.kitty}/bin/kitty \
      -T Track \
      -o remember_window_size=no \
      -o initial_window_width=87c \
      -o initial_window_height=27c \
      ${pkgs.bashInteractive}/bin/bash -lc \
      '"$1" --file "$2"; exec ${pkgs.bashInteractive}/bin/bash' \
      appcheck ${apptrack}/bin/apptrack "$ledger"
  '';
in
{
  home.packages = [
    apptrack
    appcheck
  ];
}
