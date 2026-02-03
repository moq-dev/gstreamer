{
  description = "Media over QUIC - Gstreamer plugin";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    crane.url = "github:ipetkov/crane";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, crane, rust-overlay}: 
      flake-utils.lib.eachDefaultSystem (
        system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ (import rust-overlay) ];
          };
          rust-toolchain = pkgs.rust-bin.stable.latest.default.override {
            extensions = [
              "rust-src"
              "rust-analyzer"
            ];
          };
         craneLib = (crane.mkLib pkgs).overrideToolchain rust-toolchain;

          # Helper function to get crate info from Cargo.toml
          crateInfo = cargoTomlPath: craneLib.crateNameFromCargoToml { cargoToml = cargoTomlPath; };
          
          gstreamerLib = with pkgs ; [
            gst_all_1.gstreamer.out
            gst_all_1.gst-plugins-base
            gst_all_1.gst-plugins-good
            gst_all_1.gst-plugins-bad
          ];
          gstreamerSearch = with pkgs; [
            gst_all_1.gst-plugins-ugly
            gst_all_1.gst-plugins-rs
          ] ++ gstreamerLib;

          shell-deps = with pkgs; [
            rust-toolchain
            just
            pkg-config
            glib
            cargo-sort
            cargo-shear
            cargo-edit
            gst_all_1.gstreamer
          ];

          gstPluginPath = nixpkgs.lib.makeSearchPath "lib/gstreamer-1.0" gstreamerSearch + ":" + nixpkgs.lib.makeLibraryPath gstreamerLib;
        in {
          devShell = pkgs.mkShell {
            packages = shell-deps;

            env = {
              GST_PLUGIN_PATH_1_0 = gstPluginPath;
              #PKG_CONFIG_PATH = pkgConfigPath;
              GST_REGISTRY = "./gst-registry.bin";
            };

            # if you want zsh instead of bash, uncomment this
            #shellHook = ''
            #  exec zsh
            #'';
          };
          packages = {
            hang-gst = craneLib.buildPackage (
              crateInfo ./Cargo.toml
              // {
                src = craneLib.cleanCargoSource ./.;
                nativeBuildInputs = with pkgs; [
                  pkg-config
                  glib
                  glib.dev
                  gst_all_1.gstreamer.dev
                ];
                cargoExtraArgs = "-p hang-gst";
              }
            );
            default = self.packages.${system}.hang-gst;
          };
        });
}
