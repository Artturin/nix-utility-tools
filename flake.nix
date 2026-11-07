{
  description = "Tools to review nixpkgs packages by diffing package outputs and build logs";

  inputs = {
    naersk = {
      url = "github:nix-community/naersk/master";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      utils,
      naersk,
    }:
    let
      inherit (nixpkgs) lib;
      analyzerLambda =
        pkgs:
        pkgs.callPackage (
          {
            callPackage,
            rustPackages,
            cacert,
          }:
          let
            naersk-lib = callPackage naersk { };
          in
          naersk-lib.buildPackage {
            name = "nix-log-diff-analyzer";
            version = (lib.importTOML ./Cargo.toml).workspace.package.version;
            src = lib.fileset.toSource {
              root = ./.;
              fileset = lib.fileset.unions [
                ./Cargo.toml
                ./Cargo.lock
                ./crates
              ];
            };

            # naersk only reads the name and version from a root package, not a workspace
            cargoBuildOptions = opts: opts ++ [ "--package=nix-log-diff-analyzer" ];
            cargoTestOptions = opts: opts ++ [ "--package=nix-log-diff-analyzer" ];

            checkInputs = [ rustPackages.clippy ];
            # reqwest loads system CA certificates even for plain http clients
            SSL_CERT_FILE = "${cacert}/etc/ssl/certs/ca-bundle.crt";
            doCheck = true;
            cargoTestCommands =
              x:
              x
              ++ [
                ''
                  cargo clippy --all --all-features --tests -- \
                                  -D warnings''
              ];

            meta = {
              description = "Ask an LLM (local or OpenAI-compatible remote) whether a diff of two sorted Nix build logs is noise or consequential";
              mainProgram = "nix-log-diff-analyzer";
            };
          }
        ) { };
    in
    utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
      in
      {
        packages = {
          default = self.packages.${system}.nix-log-diff-analyzer;
          nix-log-diff-analyzer = analyzerLambda pkgs;
        };

        apps.default = utils.lib.mkApp {
          drv = self.packages.${system}.default;
        };

        devShells.default =
          with pkgs;
          mkShell {
            nativeBuildInputs = [
              cargo
              cargo-edit
              rustc
              rustfmt
              rustPackages.clippy
            ];
            RUST_SRC_PATH = rustPlatform.rustLibSrc;
          };

        formatter = pkgs.nixfmt-tree;
      }
    )
    // {
      overlays.default = (
        final: prev: {
          nix-log-diff-analyzer = analyzerLambda prev;
        }
      );
    };
}
