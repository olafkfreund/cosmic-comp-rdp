{
  description = "Compositor for the COSMIC desktop environment (with RemoteDesktop EIS support)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

    parts.url = "github:hercules-ci/flake-parts";
    parts.inputs.nixpkgs-lib.follows = "nixpkgs";

    crane.url = "github:ipetkov/crane";

    rust.url = "github:oxalica/rust-overlay";
    rust.inputs.nixpkgs.follows = "nixpkgs";

    nix-filter.url = "github:numtide/nix-filter";
  };

  outputs =
    inputs@{
      self,
      nixpkgs,
      parts,
      crane,
      rust,
      nix-filter,
      ...
    }:
    parts.lib.mkFlake { inherit inputs; } {
      systems = [
        "aarch64-linux"
        "x86_64-linux"
      ];

      flake = {
        nixosModules = {
          default = import ./nix/module.nix;
          cosmic-comp = import ./nix/module.nix;
        };

        homeManagerModules = {
          default = import ./nix/home-manager.nix;
          cosmic-comp = import ./nix/home-manager.nix;
        };

        overlays.default = final: prev: {
          cosmic-comp = self.packages.${prev.system}.default;
        };
      };

      perSystem =
        {
          self',
          lib,
          system,
          ...
        }:
        let
          pkgs = nixpkgs.legacyPackages.${system}.extend rust.overlays.default;
          rust-toolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
          craneLib = (crane.mkLib pkgs).overrideToolchain rust-toolchain;

          runtimeDeps = with pkgs; [
            libglvnd
            wayland
            xorg.libX11
            xorg.libXcursor
            xorg.libxcb
            xorg.libXi
            libxkbcommon
            vulkan-loader
            libei
          ];

          # Force-link graphics libraries that wgpu/iced dlopen at runtime.
          # Equivalent to what libcosmicAppHook does in nixos-cosmic.
          forceLinkedLibs = [
            "-lEGL"
            "-lwayland-client"
            "-lwayland-egl"
            "-lxkbcommon"
            "-lX11"
            "-lX11-xcb"
            "-lXcursor"
            "-lXi"
            "-lxcb"
            "-lvulkan"
          ];

          forceRustFlags = builtins.concatStringsSep " " (
            [ "-C link-arg=-Wl,--push-state,--no-as-needed" ]
            ++ map (l: "-C link-arg=${l}") forceLinkedLibs
            ++ [ "-C link-arg=-Wl,--pop-state" ]
          );

          craneArgs = {
            pname = "cosmic-comp";
            version = self.rev or "dirty";

            src = nix-filter.lib.filter {
              root = ./.;
              include = [
                ./src
                ./i18n.toml
                ./Cargo.toml
                ./Cargo.lock
                ./resources
                ./cosmic-comp-config
              ];
            };

            nativeBuildInputs = with pkgs; [
              pkg-config
              autoPatchelfHook
              cmake
            ];

            buildInputs = with pkgs; [
              wayland
              systemd
              seatd
              libxkbcommon
              libinput
              mesa
              fontconfig
              stdenv.cc.cc.lib
              pixman
              libdisplay-info
              libei
              libglvnd
              xorg.libX11
              xorg.libXcursor
              xorg.libXi
              xorg.libxcb
              vulkan-loader
            ];

            runtimeDependencies = runtimeDeps;

            CARGO_BUILD_RUSTFLAGS = forceRustFlags;
          };

          cargoArtifacts = craneLib.buildDepsOnly craneArgs;
          cosmic-comp = craneLib.buildPackage (craneArgs // { inherit cargoArtifacts; });
        in
        {
          apps.cosmic-comp = {
            type = "app";
            program = lib.getExe self'.packages.default;
          };

          checks.cosmic-comp = cosmic-comp;
          packages.default = cosmic-comp;

          devShells.default = craneLib.devShell {
            LD_LIBRARY_PATH = lib.makeLibraryPath (
              __concatMap (d: d.runtimeDependencies) (__attrValues self'.checks)
            );

            inputsFrom = [ cosmic-comp ];
          };
        };
    };
}
