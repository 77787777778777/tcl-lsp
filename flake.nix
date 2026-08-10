{
  description = "tcl-lsp — a language server for Tcl and Tk";

  inputs = {
    # A channel branch, deliberately, not `master`. Channel branches only advance
    # once Hydra's blocking jobset has passed, so their build outputs are in the
    # binary cache; `master` is the integration branch and routinely leaves you
    # compiling LLVM and rustc locally.
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    utils.url = "github:numtide/flake-utils";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      utils,
      fenix,
    }:
    let
      # Fixes nagelfar, which is unusable as packaged in nixpkgs: its installPhase
      # copies only `nagelfar.tcl` and drops every `syntaxdb*.tcl`, so the tool exits
      # immediately with "No syntax database file found". nagelfar locates its
      # databases relative to its own script, so installing script and databases
      # together into one directory and exec'ing it through a wrapper is enough.
      nagelfarOverlay = final: prev: {
        nagelfar-full = prev.nagelfar.overrideAttrs (old: {
          pname = "nagelfar-full";

          nativeBuildInputs = (old.nativeBuildInputs or [ ]) ++ [ final.makeWrapper ];

          installPhase = ''
            runHook preInstall

            install -Dm644 -t $out/share/nagelfar syntaxdb*.tcl
            install -Dm644 nagelfar.tcl $out/share/nagelfar/nagelfar.tcl

            makeWrapper ${final.tcl}/bin/tclsh $out/bin/nagelfar \
              --add-flags $out/share/nagelfar/nagelfar.tcl

            runHook postInstall
          '';

          meta = (old.meta or { }) // {
            description = "Static syntax checker for Tcl, with its syntax databases installed";
          };
        });
      };
    in
    utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [
            fenix.overlays.default
            nagelfarOverlay
          ];
        };

        inherit (pkgs) lib;

        # Stable, for reproducibility. Nothing here needs nightly.
        rustToolchain = fenix.packages.${system}.stable.withComponents [
          "rustc"
          "cargo"
          "rust-std"
          "rust-src"
          "clippy"
          "rustfmt"
        ];
        rustPlatform = pkgs.makeRustPlatform {
          cargo = rustToolchain;
          rustc = rustToolchain;
        };

        # bindgen resolves `#include <tcl.h>` through libclang, which does not inherit
        # the gcc stdenv's header search path. It needs two things spelled out:
        # clang's own resource headers (stddef.h and friends), and the libc headers
        # (tcl.h includes stdio.h). The clang version is read at eval time so a clang
        # bump cannot rot the path.
        clangMajor = lib.versions.major pkgs.llvmPackages.llvm.version;
        clangResourceInclude = "${pkgs.llvmPackages.libclang.lib}/lib/clang/${clangMajor}/include";
        bindgenClangArgs = lib.concatStringsSep " " [
          "-I${clangResourceInclude}"
          "-I${lib.getDev pkgs.stdenv.cc.libc}/include"
        ];

        # Builds the server against a specific Tcl/Tk pair.
        #
        # `crates/tcl-tclsys/build.rs` probes Tcl with pkg-config and derives the
        # generation from its version, so selecting 8.6 vs 9.0 is entirely a matter
        # of which `tcl.pc` is on PKG_CONFIG_PATH — no feature flags to keep in sync.
        mkTclLsp =
          {
            tcl,
            tk,
            suffix ? "",
          }:
          rustPlatform.buildRustPackage {
            pname = "tcl-lsp${suffix}";
            version = (lib.importTOML ./Cargo.toml).workspace.package.version;
            src = lib.cleanSource ./.;
            cargoLock.lockFile = ./Cargo.lock;

            nativeBuildInputs = [
              pkgs.pkg-config
              pkgs.makeBinaryWrapper
              pkgs.llvmPackages.libclang
            ];
            buildInputs = [ tcl ];

            env = {
              PKG_CONFIG_PATH = "${tcl}/lib/pkgconfig";
              LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
              BINDGEN_EXTRA_CLANG_ARGS = bindgenClangArgs;
              # Every crate that transitively links libtcl needs its directory on the
              # rpath, including the test binaries. Cargo scopes a build script's
              # `rustc-link-arg` to its own package, so this has to be set globally.
              RUSTFLAGS = "-C link-arg=-Wl,-rpath,${tcl}/lib";
              # tcllib is 600+ files of valid, real-world Tcl. Because it is valid by
              # construction, any parse error the corpus test reports is our bug.
              TCL_LSP_CORPUS = "${pkgs.tclPackages.tcllib}/lib/tcllib${pkgs.tclPackages.tcllib.version}";
            };

            nativeCheckInputs = [ pkgs.tclPackages.tcllib ];

            # Bake the Tcl runtime and the optional external tools as *defaults*, so
            # the server works with no PATH setup while a user's environment can still
            # override any of them.
            postInstall = ''
              wrapProgram $out/bin/tcl-lsp \
                --set-default TCL_LIBRARY "${tcl}/lib/tcl${lib.versions.majorMinor tcl.version}" \
                --set-default TK_LIBRARY "${tk}/lib/tk${lib.versions.majorMinor tk.version}" \
                --set-default TCLLIBPATH "${pkgs.tclPackages.tcllib}/lib/tcllib${pkgs.tclPackages.tcllib.version}" \
                --set-default TCL_LSP_TCLSH "${tcl}/bin/tclsh" \
                --set-default TCL_LSP_NAGELFAR "${pkgs.nagelfar-full}/bin/nagelfar" \
                --set-default TCL_LSP_NAGELFAR_DB "${pkgs.nagelfar-full}/share/nagelfar/${
                  if lib.versionAtLeast tcl.version "9.0" then "syntaxdb90.tcl" else "syntaxdb86.tcl"
                }" \
                --set-default TCL_LSP_TCLINT "${pkgs.tclint}/bin/tclint" \
                --set-default TCL_LSP_TCLFMT "${pkgs.tclint}/bin/tclfmt"
            '';

            meta = {
              description = "A language server for Tcl and Tk";
              homepage = "https://github.com/pillowtrucker/tcl-lsp";
              license = with lib.licenses; [
                mit
                asl20
              ];
              mainProgram = "tcl-lsp";
              platforms = lib.platforms.unix;
            };
          };

        # Regenerates the committed command database from a given Tcl/Tk pair's man
        # pages. The result is committed to the repo so `cargo build` needs neither
        # Tcl nor Nix; `checks.cmddb-current` guards against it drifting.
        mkCmdDb =
          {
            tcl,
            tk,
            version,
          }:
          pkgs.runCommand "tcl-cmddb-${version}"
            {
              nativeBuildInputs = [
                tcl-lsp-tools
                pkgs.gzip
              ];
            }
            ''
              mkdir -p $out
              tcl-cmddb --version ${version} \
                --tcl-man ${tcl.man}/share/man/mann \
                --tk-man ${tk.man}/share/man/mann \
                --out $out/tcl${builtins.replaceStrings [ "." ] [ "" ] version}.json
            '';

        # The generator, built without the server's wrapper.
        tcl-lsp-tools = mkTclLsp {
          tcl = pkgs.tcl-8_6;
          tk = pkgs.tk-8_6;
          suffix = "-tools";
        };

        cmddb86 = mkCmdDb {
          tcl = pkgs.tcl-8_6;
          tk = pkgs.tk-8_6;
          version = "8.6";
        };
        cmddb90 = mkCmdDb {
          tcl = pkgs.tcl-9_0;
          tk = pkgs.tk-9_0;
          version = "9.0";
        };

        tcl-lsp = mkTclLsp {
          tcl = pkgs.tcl-8_6;
          tk = pkgs.tk-8_6;
        };
        tcl-lsp-tcl9 = mkTclLsp {
          tcl = pkgs.tcl-9_0;
          tk = pkgs.tk-9_0;
          suffix = "-tcl9";
        };
      in
      {
        packages = {
          default = tcl-lsp;
          inherit tcl-lsp tcl-lsp-tcl9 cmddb86 cmddb90;
          inherit (pkgs) nagelfar-full;
        };

        apps = {
          default = {
            type = "app";
            program = lib.getExe tcl-lsp;
          };
          # `nix run .#regen-cmddb` refreshes the committed databases after a
          # nixpkgs bump moves Tcl or Tk.
          regen-cmddb = {
            type = "app";
            program = lib.getExe (
              pkgs.writeShellScriptBin "regen-cmddb" ''
                set -eu
                dest="''${1:-crates/tcl-analysis/data}"
                mkdir -p "$dest"
                cp -f ${cmddb86}/tcl86.json "$dest/tcl86.json"
                cp -f ${cmddb90}/tcl90.json "$dest/tcl90.json"
                chmod u+w "$dest"/tcl86.json "$dest"/tcl90.json
                echo "refreshed $dest/tcl86.json and $dest/tcl90.json"
              ''
            );
          };
        };

        devShells.default = pkgs.mkShell {
          inputsFrom = [ tcl-lsp ];

          packages = [
            rustToolchain
            pkgs.rust-analyzer
            pkgs.cargo-watch
            # The Tcl side: a runtime to test against, plus the external analysers.
            pkgs.tcl-8_6
            pkgs.tk-8_6
            pkgs.tclPackages.tcllib
            pkgs.nagelfar-full
            pkgs.tclint
            pkgs.nixfmt-rfc-style
          ];

          env = {
            PKG_CONFIG_PATH = "${pkgs.tcl-8_6}/lib/pkgconfig";
            LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
            BINDGEN_EXTRA_CLANG_ARGS = bindgenClangArgs;
            RUSTFLAGS = "-C link-arg=-Wl,-rpath,${pkgs.tcl-8_6}/lib";
            TCL_LIBRARY = "${pkgs.tcl-8_6}/lib/tcl8.6";
            TK_LIBRARY = "${pkgs.tk-8_6}/lib/tk8.6";
            TCL_LSP_NAGELFAR = "${pkgs.nagelfar-full}/bin/nagelfar";
            TCL_LSP_NAGELFAR_DB = "${pkgs.nagelfar-full}/share/nagelfar/syntaxdb86.tcl";
            TCL_LSP_TCLINT = "${pkgs.tclint}/bin/tclint";
            TCL_LSP_TCLFMT = "${pkgs.tclint}/bin/tclfmt";
            # Corpus for the parser tests: 600+ real Tcl files, free and already pinned.
            TCL_LSP_CORPUS = "${pkgs.tclPackages.tcllib}/lib/tcllib${pkgs.tclPackages.tcllib.version}";
          };

          shellHook = ''
            echo "tcl-lsp dev shell"
            echo "  rust : $(rustc --version)"
            echo "  tcl  : $(echo 'puts [info patchlevel]' | tclsh)"
            echo "  tools: nagelfar, tclint, tclfmt"
          '';
        };

        checks = {
          build = tcl-lsp;
          build-tcl9 = tcl-lsp-tcl9;

          clippy = tcl-lsp.overrideAttrs (old: {
            pname = "tcl-lsp-clippy";
            nativeBuildInputs = (old.nativeBuildInputs or [ ]) ++ [ rustToolchain ];
            buildPhase = "cargo clippy --workspace --all-targets -- -D warnings";
            installPhase = "touch $out";
            doCheck = false;
          });

          fmt =
            pkgs.runCommand "tcl-lsp-fmt"
              {
                nativeBuildInputs = [ rustToolchain ];
              }
              ''
                export HOME=$TMPDIR
                cp -r ${lib.cleanSource ./.}/. src
                chmod -R u+w src
                cd src
                cargo fmt --all --check
                touch $out
              '';

          # Drives the real binary over stdio JSON-RPC, the way an editor does.
          # Written in Tcl on purpose: no Rust harness sits between the test and the
          # wire, so the framing itself is under test.
          e2e =
            pkgs.runCommand "tcl-lsp-e2e"
              {
                nativeBuildInputs = [
                  tcl-lsp
                  pkgs.tcl-8_6
                  pkgs.nagelfar-full
                  pkgs.tclint
                ];
                # The wrapper already bakes these, but setting them explicitly makes
                # the check fail loudly if the external-analyser path regresses,
                # rather than silently skipping it.
                TCL_LSP_NAGELFAR = "${pkgs.nagelfar-full}/bin/nagelfar";
                TCL_LSP_NAGELFAR_DB = "${pkgs.nagelfar-full}/share/nagelfar/syntaxdb86.tcl";
                TCL_LSP_TCLINT = "${pkgs.tclint}/bin/tclint";
                TCL_LSP_TCLFMT = "${pkgs.tclint}/bin/tclfmt";
              }
              ''
                export HOME=$TMPDIR
                cd $TMPDIR
                tclsh ${./tests/lsp_smoke.tcl} ${lib.getExe tcl-lsp} | tee result.log
                grep -q "ALL CHECKS PASSED" result.log
                grep -q "nagelfar diagnostics surface" result.log
                touch $out
              '';

          # The committed command database must match what the pinned Tcl/Tk man
          # pages actually say. Without this, a nixpkgs bump silently leaves the
          # server documenting a version it no longer builds against.
          cmddb-current =
            pkgs.runCommand "tcl-cmddb-current" { }
              ''
                if ! diff -q ${cmddb86}/tcl86.json ${./crates/tcl-analysis/data/tcl86.json}; then
                  echo "FAIL: crates/tcl-analysis/data/tcl86.json is stale."
                  echo "Run: nix run .#regen-cmddb"
                  exit 1
                fi
                if ! diff -q ${cmddb90}/tcl90.json ${./crates/tcl-analysis/data/tcl90.json}; then
                  echo "FAIL: crates/tcl-analysis/data/tcl90.json is stale."
                  echo "Run: nix run .#regen-cmddb"
                  exit 1
                fi
                touch $out
              '';

          # nagelfar is useless without its databases; assert the overlay fixed it.
          nagelfar-usable =
            pkgs.runCommand "nagelfar-usable"
              {
                nativeBuildInputs = [ pkgs.nagelfar-full ];
              }
              ''
                printf 'proc greet {name} {\n  puts "hi $nam"\n}\n' > bad.tcl
                # Note: not named `out` — that is Nix's output path variable.
                report=$(nagelfar bad.tcl 2>&1) || true
                echo "$report"
                if echo "$report" | grep -q 'No syntax database'; then
                  echo "FAIL: nagelfar still cannot find its syntax database"; exit 1
                fi
                if ! echo "$report" | grep -q 'Unknown variable'; then
                  echo "FAIL: nagelfar did not report the expected diagnostic"; exit 1
                fi
                touch $out
              '';
        };

        formatter = pkgs.nixfmt-rfc-style;
      }
    )
    // {
      overlays.default = nagelfarOverlay;
    };
}
