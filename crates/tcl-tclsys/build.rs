//! Locate libtcl and generate bindings for its parser API.
//!
//! Bindings are *generated*, never hand-written, because the ABI genuinely differs
//! between the versions we support: in Tcl 8.6 `Tcl_Token::size` and
//! `Tcl_Token::numComponents` are `int`, while in Tcl 9.0 they are `Tcl_Size`
//! (`ptrdiff_t`). Hand-rolled `#[repr(C)]` structs would silently misread one of them.

use std::{env, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=TCL_INCLUDE_DIR");
    println!("cargo:rerun-if-env-changed=TCL_LIB_DIR");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");

    let (include_dirs, version) = probe_tcl();

    let major: u32 = version
        .split('.')
        .next()
        .and_then(|m| m.parse().ok())
        .unwrap_or(8);

    // Lets downstream code branch on the target Tcl generation.
    println!("cargo:rustc-check-cfg=cfg(tcl9)");
    if major >= 9 {
        println!("cargo:rustc-cfg=tcl9");
    }
    println!("cargo:version={version}");

    let mut builder = bindgen::Builder::default()
        .header_contents("wrapper.h", "#include <tcl.h>\n")
        // Only the parser surface. Binding all of tcl.h would drag in the entire
        // interpreter API and its stub-table machinery for no benefit.
        .allowlist_function("Tcl_ParseCommand")
        .allowlist_function("Tcl_ParseExpr")
        .allowlist_function("Tcl_ParseBraces")
        .allowlist_function("Tcl_ParseQuotedString")
        .allowlist_function("Tcl_ParseVarName")
        .allowlist_function("Tcl_FreeParse")
        .allowlist_function("Tcl_CommandComplete")
        .allowlist_type("Tcl_Parse")
        .allowlist_type("Tcl_Token")
        .allowlist_type("Tcl_Interp")
        .allowlist_var("TCL_TOKEN_.*")
        .allowlist_var("TCL_OK")
        .allowlist_var("TCL_ERROR")
        .layout_tests(false)
        .generate_comments(false)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()));

    for dir in &include_dirs {
        builder = builder.clang_arg(format!("-I{}", dir.display()));
    }

    let bindings = builder.generate().expect("failed to generate tcl bindings");

    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR unset"));
    bindings
        .write_to_file(out.join("bindings.rs"))
        .expect("failed to write bindings.rs");
}

/// Returns the include directories and the Tcl version string.
///
/// Prefers explicit `TCL_INCLUDE_DIR`/`TCL_LIB_DIR` (what the Nix build sets, so the
/// 8.6 and 9.0 package variants are unambiguous), and otherwise falls back to
/// pkg-config, which resolves `tcl.pc` on a normal system.
fn probe_tcl() -> (Vec<PathBuf>, String) {
    if let Ok(include) = env::var("TCL_INCLUDE_DIR") {
        if let Ok(libdir) = env::var("TCL_LIB_DIR") {
            println!("cargo:rustc-link-search=native={libdir}");
        }
        let version = env::var("TCL_VERSION").unwrap_or_else(|_| "8.6".to_string());
        let stub = env::var("TCL_LINK_LIB").unwrap_or_else(|_| {
            let major_minor: String = version.split('.').take(2).collect::<Vec<_>>().join(".");
            format!("tcl{major_minor}")
        });
        println!("cargo:rustc-link-lib={stub}");
        return (vec![PathBuf::from(include)], version);
    }

    let lib = pkg_config::Config::new()
        .atleast_version("8.6")
        .probe("tcl")
        .expect(
            "could not find Tcl via pkg-config. \
             Set TCL_INCLUDE_DIR/TCL_LIB_DIR, or install Tcl development files. \
             Inside this repo, `nix develop` provides them.",
        );

    // Bake libtcl's directory into this crate's own binaries, so `cargo test -p
    // tcl-tclsys` works without LD_LIBRARY_PATH. Note this does not propagate to
    // other packages in the workspace — Cargo scopes `rustc-link-arg` to the
    // declaring package — so the Nix build additionally sets RUSTFLAGS.
    for path in &lib.link_paths {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{}", path.display());
    }

    (lib.include_paths.clone(), lib.version)
}
