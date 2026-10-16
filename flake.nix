{
  description = "rusty-bucket: a read-only S3/Ceph RGW FUSE filesystem (Rust port of goofys)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    crane.url = "github:ipetkov/crane";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
      crane,
      flake-utils,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };

        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

        craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

        # Only rebuild when Rust sources / manifests change.
        src = craneLib.cleanCargoSource ./.;

        # Native dependencies needed to compile the crate.
        buildDeps = with pkgs; [
          cmake
          perl
          pkg-config
        ];

        # Runtime dependencies (fusermount3 lives in the fuse3 package).
        runtimeDeps = with pkgs; [ fuse3 ];

        commonArgs = {
          inherit src;
          strictDeps = true;
          nativeBuildInputs = buildDeps;
          buildInputs = [ pkgs.openssl ];
        };

        # Cache compiled dependencies separately from the crate itself.
        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        rusty-bucket = craneLib.buildPackage (
          commonArgs
          // {
            inherit cargoArtifacts;
            nativeBuildInputs = buildDeps ++ [ pkgs.makeWrapper ];
            doCheck = true;

            CARGO_PROFILE = "debug";

            # Make `fusermount3` available to the mount helper at runtime.
            postInstall = ''
              wrapProgram $out/bin/rusty-bucket \
                --prefix PATH : ${pkgs.lib.makeBinPath runtimeDeps}
            '';

            meta = with pkgs.lib; {
              description = "Read-only FUSE filesystem for S3-compatible object stores (Ceph RGW)";
              license = licenses.asl20;
              mainProgram = "rusty-bucket";
              platforms = platforms.linux;
            };
          }
        );

        # Provisions a throwaway Ceph RGW container and runs the conformance
        # suite against it. Needs Docker/Podman and /dev/fuse on the host.
        ceph-test = pkgs.writeShellApplication {
          name = "rusty-bucket-ceph-test";
          runtimeInputs = [
            rustToolchain
            pkgs.docker-client
            pkgs.curl
            pkgs.git
            pkgs.coreutils
            pkgs.gnugrep
            pkgs.fuse3
          ]
          ++ buildDeps
          ++ [ pkgs.stdenv.cc ];
          text = ''
            exec ${./scripts/ceph-test.sh} "$@"
          '';
        };
      in
      {
        packages.default = rusty-bucket;
        packages.rusty-bucket = rusty-bucket;
        packages.ceph-test = ceph-test;

        devShells.default = pkgs.mkShell {
          packages = [
            rustToolchain
            pkgs.cargo-nextest
          ]
          ++ buildDeps
          ++ runtimeDeps
          ++ (with pkgs; [
            awscli2
            curl
            docker-client
            openssl
          ]);
        };

        checks = {
          build = rusty-bucket;
          fmt = craneLib.cargoFmt {
            inherit src;
          };
          clippy = craneLib.cargoClippy (
            commonArgs
            // {
              inherit cargoArtifacts;
              cargoClippyExtraArgs = "--all-targets --all-features -- --deny warnings";
            }
          );
          test = craneLib.cargoTest (
            commonArgs
            // {
              inherit cargoArtifacts;
            }
          );
        };

        apps.default = {
          type = "app";
          program = "${rusty-bucket}/bin/rusty-bucket";
          meta = {
            description = "Mount an S3-compatible bucket read-only";
          };
        };

        apps.ceph-test = {
          type = "app";
          program = "${ceph-test}/bin/rusty-bucket-ceph-test";
          meta = {
            description = "Run the read-only conformance suite against a throwaway Ceph RGW";
          };
        };

        formatter = pkgs.nixfmt;
      }
    );
}
