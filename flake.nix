{
  description = "eufy C220 / T8W11 standalone firmware - pinned environment and tasks";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
    }:
    let
      lib = nixpkgs.lib;
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];

      # The versions the flashed firmware was built with. rustc compiles the
      # Rust, rust-lld links it, and clang is the linker driver plus the C
      # compiler for ring, so a rebuild is only byte-identical when both match.
      rustVersion = "1.98.1";
      llvm = "llvmPackages_22";

      # Target of both camera binaries. The engine is the same triple built with
      # -C target-feature=-crt-static, which native/arm-engine-linker.sh owns;
      # this only has to make the target's std available.
      target = "armv7-unknown-linux-musleabihf";

      # Debian and Ubuntu build clang with -fstack-protector-strong on by default
      # and nixpkgs does not, so it is named here rather than inherited: without
      # it ring's C is compiled without stack canaries, 2,304 bytes smaller and
      # different from the firmware that was flashed.
      cflagsVar = "CFLAGS_armv7_unknown_linux_musleabihf";
      armCflags = "-fstack-protector-strong";

      # One package list for the devshell and for the `nix run` tasks, so the
      # environment a task sees cannot drift from the one you get by hand.
      toolchain =
        pkgs:
        with pkgs;
        [
          # rust-overlay rather than pkgs.rustc because native/arm-linker.sh
          # locates rust-lld at $SYSROOT/lib/rustlib/$HOST/bin/gcc-ld/ld.lld,
          # which the official toolchain layout provides and a distribution
          # build need not.
          (rust-bin.stable.${rustVersion}.default.override {
            # Only the target is added; rust-src is deliberately absent. With it
            # installed, rustc rewrites the /rustc/<hash> std paths in panic
            # locations to the local source tree, which bakes the toolchain's
            # installation directory into the artifact: measured 2,002,224 bytes
            # with it against 1,999,360 without. Absent, the paths keep the
            # stable official /rustc/<hash> form, so the build does not depend on
            # where it ran.
            targets = [ target ];
          })
          # Unwrapped on purpose: nixpkgs' `clang` is a cc-wrapper that adds host
          # hardening flags (-fzero-call-used-regs=used-gpr) which the ARM target
          # rejects, and it warns itself that a wrapped compiler is not meant to
          # be used with --target. native/arm-cc.sh and native/arm-linker.sh call
          # `clang` directly and supply the triple.
          pkgs.${llvm}.clang-unwrapped
          gcc # the host cc that cargo test builds ring with
          binutils # size -A, used by native/check.sh

          # Host tools the build and deploy scripts call by name.
          squashfs-tools # mksquashfs/unsquashfs for the /home image
          flashrom # CH341A chip-level recovery
          openssl # certificates and fingerprints, as the docs use them
          qemu # user-mode ARM, for the hardware-free media engine
          file # build-rust-media-engine.sh reports the produced ELF with it
          gawk # arm-linker.sh reads the host triple with awk
          git
          which

          # Size and dependency hygiene; the /home partition is 4,915,200 bytes,
          # so footprint is a build gate rather than a curiosity.
          cargo-bloat
          cargo-machete
          cargo-binutils
        ];

      pkgsFor =
        system:
        import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };

      shellFor =
        system:
        let
          pkgs = pkgsFor system;
        in
        pkgs.mkShell {
          packages = toolchain pkgs;
          env.${cflagsVar} = armCflags;

          shellHook = ''
            # Report the toolchain, and what the absent vendor bundle blocks.
            # native/camera-rootfs holds proprietary libraries that this
            # repository does not redistribute (README.md, licence section), so a
            # fresh clone can build the firmware and run the tests but cannot
            # build the engine or an image. Say so here instead of leaving the
            # first failure to explain it.
            if [ -n "''${PS1:-}" ]; then
              repo_root=$(git rev-parse --show-toplevel 2>/dev/null || echo "$PWD")
              echo "devshell: $(rustc --version), $(clang --version | head -n1)"
              echo "devshell: tasks are also \`nix run .\` without entering this shell"
              if [ -e "$repo_root/native/camera-rootfs/rootfs/lib/libc.so.0" ]; then
                echo "devshell: camera-rootfs present - the engine and package-c220.sh can build"
              else
                echo "devshell: native/camera-rootfs absent (vendor libraries, not redistributable)"
                echo "devshell:   available  cargo test --workspace"
                echo "devshell:              cargo build --release --target ${target} -p c220-firmware"
                echo "devshell:   needs one  native/build-rust-media-engine.sh, native/package-c220.sh"
              fi
            fi
          '';
        };

      # The tasks. Each one runs the repository's own script where one exists, so
      # the scripts stay the implementation and this stays the way in.
      appsFor =
        system:
        let
          pkgs = pkgsFor system;

          mkApp =
            name: description: body:
            let
              script = pkgs.writeShellApplication {
                inherit name;
                runtimeInputs = toolchain pkgs;
                text = ''
                  export ${cflagsVar}=${lib.escapeShellArg armCflags}
                  repo_root=$(git rev-parse --show-toplevel 2>/dev/null) || {
                    echo "${name}: run this from inside the repository" >&2
                    exit 2
                  }
                  cd "$repo_root"
                  ${body}
                '';
              };
            in
            {
              type = "app";
              program = "${script}/bin/${name}";
              meta = { inherit description; };
            };
        in
        {
          default = mkApp "tasks" "list the tasks" ''
            echo "nix run .#<task> from anywhere in the repository:"
            echo ""
            echo "  check     formatting, lints, tests, ARM release build, size budget"
            echo "  build     the ARM firmware binary"
            echo "  size      section sizes of that binary"
            echo "  bloat     what is taking the space, per crate and per function"
            echo "  engine    the media engine (needs native/camera-rootfs)"
            echo "  package   the /home overlay and squashfs image (needs BASE_HOME)"
            echo "  verify    the post-write gate, against a camera"
            echo ""
            echo "nix develop gives the same toolchain as a shell."
          '';

          check = mkApp "check" "formatting, lints, tests, ARM build, size budget" ''exec ./native/check.sh "$@"'';
          build = mkApp "build" "the ARM firmware binary" ''exec cargo build --release --target ${target} -p c220-firmware "$@"'';
          size = mkApp "size" "section sizes of the built firmware" ''
            cargo build --release --target ${target} -p c220-firmware
            exec size -A target/${target}/release/c220-firmware
          '';
          bloat = mkApp "bloat" "what is taking the space, per crate" ''
            cargo build --release --target ${target} -p c220-firmware
            exec cargo bloat --release --target ${target} -p c220-firmware --crates "$@"
          '';
          engine = mkApp "engine" "the media engine, needs native/camera-rootfs" ''exec ./native/build-rust-media-engine.sh "$@"'';
          package = mkApp "package" "the /home overlay and image, needs BASE_HOME" ''
            if [ -z "''${BASE_HOME:-}" ]; then
              echo "set BASE_HOME to the factory /home tree" >&2
              echo "native/factory-bootstrap/harvest.sh collects it from the camera" >&2
              exit 2
            fi
            exec env BASE_HOME="$BASE_HOME" INSTALL_STARTUP_HOOK=1 \
              SQUASHFS_OUT=dist/c220-home.squashfs ./native/package-c220.sh "$@"
          '';
          verify = mkApp "verify" "the post-write gate, against a camera" ''exec ./native/verify-c220-deployment.sh "$@"'';
        };
    in
    {
      devShells = lib.genAttrs systems (system: {
        default = shellFor system;
      });
      apps = lib.genAttrs systems appsFor;
    };
}
