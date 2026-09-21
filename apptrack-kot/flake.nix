{
  description = "apptrack-kot -- Android APK port of AppTrack (Kotlin/Gradle)";
  inputs = {
    config.url = "path:/home/john/repos/config";
    nixpkgs.follows = "config/nixpkgs";
    flake-utils.url = "github:numtide/flake-utils";
  };
  outputs = { self, nixpkgs, config, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          config = { allowUnfree = true; android_sdk.accept_license = true; };
        };
        androidSdk = "${config.lib.androidSdk pkgs}/libexec/android-sdk";
      in {
        devShells.default = pkgs.mkShell {
          packages = with pkgs; [ jdk17 ];
          shellHook = ''
            export JAVA_HOME="${pkgs.jdk17}"
            export ANDROID_HOME="${androidSdk}"
            export ANDROID_SDK_ROOT="${androidSdk}"
            export PATH="${androidSdk}/platform-tools:$PATH"
          '';
        };
      });
}