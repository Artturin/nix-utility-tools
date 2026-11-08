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
      #   platforms:   meta.platforms, unrestricted when left out
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
          platforms ? null,
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
              }
              // lib.optionalAttrs (platforms != null) { inherit platforms; };
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

      # The graphical front end needs GTK, so it is only exposed on Linux.
      linuxCrates = lib.mapAttrs (name: args: mkWorkspaceCrate (args // { inherit name; })) {
        nix-utility-gui = {
          description = "GTK front end for comparing nixpkgs attributes with difflog and diffoscope-nix";
          platforms = lib.platforms.linux;
          extraArgs = pkgs: {
            nativeBuildInputs = [
              pkgs.pkg-config
              pkgs.wrapGAppsHook4
            ];
            buildInputs = [
              pkgs.gtk4
              pkgs.libadwaita
              pkgs.gtksourceview5
              # schemas for the file chooser
              pkgs.gsettings-desktop-schemas
            ];
            # the tests are widget-free and need no display
            preFixup = ''
              gappsWrapperArgs+=(--prefix PATH : ${
                lib.makeBinPath [
                  pkgs.diffoscope
                  pkgs.git
                  pkgs.nix-eval-jobs
                ]
              })
            '';
            postInstall = ''
              install -Dm644 ${./crates/nix-utility-gui/data/io.github.artturin.NixUtilityGui.desktop} \
                $out/share/applications/io.github.artturin.NixUtilityGui.desktop
            '';
          };
        };
      };

      packagesFor =
        pkgs:
        lib.mapAttrs (_: mkCrate: mkCrate pkgs) (
          crates // lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux linuxCrates
        );

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
        // packagesFor pkgs;

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
              pkg-config
            ];
            buildInputs = [
              gtk4
              libadwaita
              gtksourceview5
            ];
            RUST_SRC_PATH = rustPlatform.rustLibSrc;
            # GTK looks for the GSettings schemas on XDG_DATA_DIRS
            shellHook = ''
              export XDG_DATA_DIRS="$XDG_DATA_DIRS:${gsettings-desktop-schemas}/share/gsettings-schemas/${gsettings-desktop-schemas.name}:${gtk4}/share/gsettings-schemas/${gtk4.name}"
            '';
          };

        formatter = pkgs.nixfmt-tree;
      }
    )
    // {
      overlays.default =
        final: prev:
        lib.mapAttrs (_: mkCrate: mkCrate prev) (
          crates // lib.optionalAttrs prev.stdenv.hostPlatform.isLinux linuxCrates
        );
    };
}
