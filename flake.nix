{
  description = "pierport";

  inputs = {
    nixpkgs.url = "nixpkgs/nixos-unstable";
    parts = {
      url = "github:hercules-ci/flake-parts";
      inputs.nixpkgs-lib.follows = "nixpkgs";
    };
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = inputs@{self, nixpkgs, parts, rust-overlay }: parts.lib.mkFlake { inherit inputs; } {
    systems = [ "x86_64-linux" "aarch64-darwin" ];

    flake = {};

    perSystem = { pkgs, system, ... }: {
      devShells.default = let
        pkgs = import nixpkgs { inherit system; overlays = [(import rust-overlay)]; };
      in
        pkgs.mkShell {
          nativeBuildInputs = with pkgs; [
            # Rust toolchain
            (rust-bin.stable."1.76.0".default.override {
              extensions = [ "rust-analyzer" ];
            })
          ];

          # Set a few default environment variables. Locale archive is needed
          # to make locales work due to Nix' weird decision to split it out.
          LOCALE_ARCHIVE = if pkgs.lib.strings.hasSuffix "linux" system then "${pkgs.glibcLocales}/lib/locale/locale-archive" else null;
        };
    };
  };
}
