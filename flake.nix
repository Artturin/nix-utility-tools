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
      #   checkPackages: workspace members whose tests and clippy run in this
      #                build; defaults to just the crate itself. Library crates
      #                that have no package of their own are listed here.
      #   extraArgs:   function from pkgs to extra attributes for naersk's
      #                buildPackage (e.g. env vars or native build inputs the
      #                crate needs)
      mkWorkspaceCrate =
        {
          name,
          description,
          mainProgram ? name,
          checkPackages ? [ name ],
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
            checkOptions = map (package: "--package=${package}") checkPackages;
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
              cargoTestOptions = opts: opts ++ checkOptions;

              checkInputs = [ rustPackages.clippy ];
              doCheck = true;
              cargoTestCommands =
                x:
                x
                ++ [
                  ''
                    cargo clippy ${lib.concatStringsSep " " checkOptions} --all-features --tests -- \
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
        difflog = {
          description = "Compare two Nix build logs, normalizing out boring differences";
          # nix-utility-common has no package of its own
          checkPackages = [
            "difflog"
            "nix-utility-common"
          ];
          extraArgs = pkgs: {
            nativeBuildInputs = [ pkgs.makeWrapper ];
            # the integration tests run git diff --no-index
            checkInputs = [
              pkgs.rustPackages.clippy
              pkgs.git
            ];
            # difflog runs git diff --no-index; nix itself is left to the user's setup
            postInstall = ''
              wrapProgram $out/bin/difflog --prefix PATH : ${lib.makeBinPath [ pkgs.git ]}
            '';
          };
        };
        diffoscope-nix = {
          description = "Run diffoscope on two Nix store paths, ignoring differences that are only due to differing store path hashes and GNU build IDs";
        };
        diff-nixpkgs-attr = {
          description = "Compare nixpkgs attributes against a base revision with difflog and diffoscope-nix";
          # the core library has no package of its own
          checkPackages = [
            "diff-nixpkgs-attr"
            "diff-nixpkgs-attr-core"
          ];
          extraArgs = pkgs: {
            nativeBuildInputs = [ pkgs.makeWrapper ];
            # the integration tests run git; jq lets the jq-parity test run
            checkInputs = [
              pkgs.rustPackages.clippy
              pkgs.git
              pkgs.jq
            ];
            # difflog and diffoscope-nix run in-process; what is left to run is
            # diffoscope, git, nix-eval-jobs and the pager. nix itself is left
            # to the user's setup.
            postInstall = ''
              wrapProgram $out/bin/diff-nixpkgs-attr --prefix PATH : ${
                lib.makeBinPath [
                  pkgs.diffoscope
                  pkgs.git
                  pkgs.less
                  pkgs.nix-eval-jobs
                ]
              }
            '';
          };
        };
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
