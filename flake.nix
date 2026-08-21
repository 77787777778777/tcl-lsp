{
  description = "tcl-lsp — a language server for Tcl and Tk";

  inputs = {
    # A channel branch, deliberately, not `master`. Channel branches only advance
    # once Hydra's blocking jobset has passed, so their build outputs are in the
    # binary cache; `master` is the integration branch and routinely leaves you
    # compiling LLVM and rustc locally.
    nixpkgs.url = "github:NixOS/nixpkgs?rev=f13ff45afd1bb73e640eaa08a7066dbed07e3238";
    utils.url = "github:numtide/flake-utils?rev=11707dc2f618dd54ca8739b309ec4fc024de578b";
    fenix = {
      url = "github:nix-community/fenix?rev=36ef6893e18fb070e33a1d17cd5296382c9519d4";
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
              # Tk's own library is correct Tcl/Tk by construction, so it is the
              # oracle for the option validator: anything flagged there is our bug.
              TCL_LSP_TK_LIBRARY = "${tk}/lib/tk${lib.versions.majorMinor tk.version}";
            };

            nativeCheckInputs = [
              pkgs.tclPackages.tcllib
              tk
            ];

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

        # The Emacs client. The server's store path is substituted in as the
        # *fallback*, so the package works with no configuration at all; the
        # client still prefers whatever `tcl-lsp` a project's direnv environment
        # puts on PATH, which is what makes a dev shell override this.
        emacs-tcl-lsp = pkgs.emacsPackages.trivialBuild {
          pname = "tcl-lsp";
          version = (lib.importTOML ./Cargo.toml).workspace.package.version;
          src = ./editors/emacs;

          # lsp-mode is the only one needed to byte-compile cleanly. lsp-ui and
          # eglot are loaded with `with-eval-after-load`, so they stay optional
          # at runtime and are not required here.
          packageRequires = [ pkgs.emacsPackages.lsp-mode ];

          postPatch = ''
            substituteInPlace tcl-lsp.el \
              --replace-fail \
                '(defconst tcl-lsp-bundled-server-path nil' \
                '(defconst tcl-lsp-bundled-server-path "${lib.getExe tcl-lsp}"'
          '';

          # The test file drives a real server; it is exercised by `checks.emacs`,
          # not shipped to users.
          preBuild = "rm -f tcl-lsp-tests.el";

          meta = {
            description = "Emacs client for the tcl-lsp language server";
            homepage = "https://github.com/pillowtrucker/tcl-lsp";
            license = with lib.licenses; [
              mit
              asl20
            ];
          };
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
          inherit
            tcl-lsp
            tcl-lsp-tcl9
            cmddb86
            cmddb90
            emacs-tcl-lsp
            ;
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
            # The server itself, so a direnv-enabled editor opening this repo
            # finds `tcl-lsp` on PATH and the client's discovery path is
            # exercised the same way a user's would be. Note this is the *built*
            # server, not your working tree: while hacking on the Rust, point
            # the editor at ./target/debug/tcl-lsp instead.
            tcl-lsp
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
          cmddb-current = pkgs.runCommand "tcl-cmddb-current" { } ''
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

          # The Emacs client: byte-compiled with warnings fatal, checkdoc'd, and
          # driven against the real server. lsp-mode is present so its half of
          # the suite runs rather than skipping.
          emacs =
            let
              emacsWithDeps = pkgs.emacs.pkgs.withPackages (epkgs: [
                epkgs.lsp-mode
                epkgs.lsp-ui
              ]);
            in
            pkgs.runCommand "tcl-lsp-emacs-check"
              {
                nativeBuildInputs = [
                  emacsWithDeps
                  tcl-lsp
                ];
              }
              ''
                export HOME=$TMPDIR
                cp ${./editors/emacs}/*.el .
                chmod u+w ./*.el

                echo "--- byte-compile (warnings are errors) ---"
                emacs -Q --batch -L . \
                  --eval '(setq byte-compile-error-on-warn t)' \
                  -f batch-byte-compile tcl-lsp.el

                echo "--- checkdoc ---"
                # `checkdoc-file' reports through `warn' and still exits 0, so
                # the flag it sets is what has to be tested.
                emacs -Q --batch -L . --eval '
                  (progn
                    (require (quote checkdoc))
                    (checkdoc-file "tcl-lsp.el")
                    (checkdoc-file "tcl-lsp-tests.el")
                    (when checkdoc-pending-errors (kill-emacs 1)))'

                echo "--- ert ---"
                emacs -Q --batch -L . -l tcl-lsp-tests.el \
                  -f ert-run-tests-batch-and-exit 2>&1 | tee ert.log

                # A skipped integration test would silently hide a broken client,
                # so require that the ones needing a server and lsp-mode ran.
                grep -q "0 unexpected" ert.log
                if grep -qE "SKIPPED +tcl-lsp-test-(server-starts|lsp-client)" ert.log; then
                  echo "FAIL: a test that must run was skipped"
                  exit 1
                fi
                touch $out
              '';

          # The home-manager module, evaluated against a stub of the two
          # home-manager options it touches. This cannot prove an activation
          # works, but it does catch the ways an unused module rots: a renamed
          # package attribute, a type error, or `enable` failing to gate.
          # Stubbing beats adding a home-manager flake input for one module.
          hm-module =
            let
              stub =
                { lib, ... }:
                {
                  options = {
                    home.packages = lib.mkOption {
                      type = lib.types.listOf lib.types.package;
                      default = [ ];
                    };
                    programs.emacs.enable = lib.mkEnableOption "emacs";
                    programs.emacs.extraPackages = lib.mkOption {
                      type = lib.types.functionTo (lib.types.listOf lib.types.package);
                      default = _: [ ];
                    };
                  };
                };
              evalHm =
                extra:
                (lib.evalModules {
                  modules = [
                    stub
                    self.homeManagerModules.default
                    { _module.args.pkgs = pkgs; }
                    extra
                  ];
                }).config;
              paths = map (p: p.outPath);

              disabled = evalHm { programs.tcl-lsp.enable = false; };
              withEmacs = evalHm {
                programs.tcl-lsp.enable = true;
                programs.emacs.enable = true;
              };
              # `emacs.enable` defaults to `programs.emacs.enable`, so this also
              # tests that the default is wired to the right option.
              withoutEmacs = evalHm {
                programs.tcl-lsp.enable = true;
                programs.emacs.enable = false;
              };
            in
            assert lib.assertMsg (disabled.home.packages == [ ]) "hm module installs the server when disabled";
            assert lib.assertMsg (
              paths withEmacs.home.packages == paths [ tcl-lsp ]
            ) "hm module does not install the server when enabled";
            assert lib.assertMsg (
              paths (withEmacs.programs.emacs.extraPackages pkgs.emacsPackages) == paths [
                emacs-tcl-lsp
                pkgs.emacsPackages.lsp-mode
              ]
            ) "hm module does not install the Emacs client alongside lsp-mode";
            assert lib.assertMsg (
              withoutEmacs.programs.emacs.extraPackages pkgs.emacsPackages == [ ]
            ) "hm module installs the Emacs client without Emacs";
            pkgs.runCommand "tcl-lsp-hm-module" { } "touch $out";

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

      # Best-effort, and flagged as such in the README: the author does not use
      # home-manager, so this is verified to *evaluate* (`checks.hm-module`) but
      # has never been run in a real activation.
      #
      # There is deliberately no `epkgs.tcl-lsp` overlay attribute to go with
      # this. An overlay entry would have to build the Emacs client against the
      # consumer's nixpkgs, and the client bakes in a `tcl-lsp` store path — so
      # it would silently bake in a *different* server build than this flake
      # pins. Referring to `packages.emacs-tcl-lsp` keeps the two in step.
      homeManagerModules.default =
        {
          config,
          lib,
          pkgs,
          ...
        }:
        let
          cfg = config.programs.tcl-lsp;
          ours = self.packages.${pkgs.stdenv.hostPlatform.system};
        in
        {
          options.programs.tcl-lsp = {
            enable = lib.mkEnableOption "the tcl-lsp language server for Tcl and Tk";

            package = lib.mkOption {
              type = lib.types.package;
              default = ours.tcl-lsp;
              defaultText = lib.literalExpression "tcl-lsp.packages.\${system}.tcl-lsp";
              description = ''
                The server. Set to `tcl-lsp-tcl9` to link against Tcl/Tk 9.0.
                Note this selects the *runtime* only; which command set your code
                is analysed against is the `tclVersion` setting, independently.
              '';
            };

            emacs.enable = lib.mkOption {
              type = lib.types.bool;
              default = config.programs.emacs.enable;
              defaultText = lib.literalExpression "config.programs.emacs.enable";
              description = ''
                Install the Emacs client into `programs.emacs.extraPackages`,
                along with lsp-mode. The client prefers a `tcl-lsp` found on
                `exec-path` — so a project's direnv environment still wins — and
                falls back to {option}`programs.tcl-lsp.package`.
              '';
            };
          };

          config = lib.mkIf cfg.enable {
            home.packages = [ cfg.package ];

            programs.emacs.extraPackages = lib.mkIf cfg.emacs.enable (epkgs: [
              ours.emacs-tcl-lsp
              epkgs.lsp-mode
            ]);
          };
        };
    };
}
