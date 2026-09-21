#!/usr/bin/env bash
set -euo pipefail
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$ROOT_DIR"
if [[ "${1:-}" != "--inside-nix" ]]; then
  exec nix develop "path:$ROOT_DIR" -c ./release-apk.sh --inside-nix
fi
export JAVA_HOME="$(dirname "$(dirname "$(readlink -f "$(command -v java)")")")"
ANDROID_SDK_OUT="$(nix build --no-link --print-out-paths "path:$HOME/repos/config#android-sdk")"
export ANDROID_HOME="$ANDROID_SDK_OUT/libexec/android-sdk"
export ANDROID_SDK_ROOT="$ANDROID_HOME"
printf 'sdk.dir=%s\n' "$ANDROID_HOME" > local.properties
./gradlew testDebugUnitTest assembleDebug --console=plain
APK_PATH="$ROOT_DIR/app/build/outputs/apk/debug/app-debug.apk"
APKSIGNER="$(find -L "$ANDROID_HOME/build-tools" -name apksigner | sort -V | tail -1)"
[[ -x "$APKSIGNER" ]] || { echo 'Android SDK apksigner unavailable'; exit 1; }
"$APKSIGNER" verify --verbose "$APK_PATH"
mkdir -p "$HOME/syncthing/apptrack-kot"
cp "$APK_PATH" "$HOME/syncthing/apptrack-kot/apptrack-kot.apk"
printf 'APK ready: %s/syncthing/apptrack-kot/apptrack-kot.apk\n' "$HOME"