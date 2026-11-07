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

      workspaceVersion = (lib.importTOML ./Cargo.toml).workspace.package.version;

      # Builds the workspace member `name` from crates/${name}.
      # Returns a function from pkgs to the package, so it can be used for both
      # `packages` and the overlay.
      #   description: meta.description
      #   mainProgram: meta.mainProgram, defaults to name
      #   extraArgs:   function from pkgs to extra attributes for naersk's
      #                buildPackage (e.g. env vars or native build inputs the
      #                crate needs)
      mkWorkspaceCrate =
        {
          name,
          description,
          mainProgram ? name,
          extraArgs ? _: { },
        }:
        pkgs:
        pkgs.callPackage (
          {
            callPackage,
            rustPackages,
          }:
          let
            naersk-lib = callPackage naersk { };
            packageOption = "--package=${name}";
          in
          naersk-lib.buildPackage (
            {
              inherit name;
              version = workspaceVersion;
              src = lib.fileset.toSource {
                root = ./.;
                fileset = lib.fileset.unions [
                  ./Cargo.toml
                  ./Cargo.lock
                  ./crates
                ];
              };

              # naersk only reads the name and version from a root package, not a workspace
              cargoBuildOptions = opts: opts ++ [ packageOption ];
              cargoTestOptions = opts: opts ++ [ packageOption ];

              checkInputs = [ rustPackages.clippy ];
              doCheck = true;
              cargoTestCommands =
                x:
                x
                ++ [
                  ''
                    cargo clippy ${packageOption} --all-features --tests -- \
                                    -D warnings''
                ];

              meta = {
                inherit description mainProgram;
              };
            }
            // extraArgs pkgs
          )
        ) { };

      # One entry per workspace member under crates/. Adding a crate there and an
      # entry here is all that's needed to expose it as a package and in the overlay.
      crates = lib.mapAttrs (name: args: mkWorkspaceCrate (args // { inherit name; })) {
        nix-log-diff-analyzer = {
          description = "Ask an LLM (local or OpenAI-compatible remote) whether a diff of two sorted Nix build logs is noise or consequential";
          extraArgs = pkgs: {
            # reqwest loads system CA certificates even for plain http clients
            SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
          };
        };
      };

      # The crate that `nix build`/`nix run` use without an attribute
      defaultCrate = "nix-log-diff-analyzer";
    in
    utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
      in
      {
        packages = {
          default = self.packages.${system}.${defaultCrate};
        }
        // lib.mapAttrs (_: mkCrate: mkCrate pkgs) crates;

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
      overlays.default = final: prev: lib.mapAttrs (_: mkCrate: mkCrate prev) crates;
    };
}
