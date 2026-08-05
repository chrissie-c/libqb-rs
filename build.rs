use std::env;
use std::path::PathBuf;

// Replace the cbindgen-generated full struct qb_hdb body with a forward
// declaration.  qbhdb.h provides the complete struct with the public field
// names needed by QB_HDB_DECLARE.  A forward declaration is sufficient here
// because all qb_hdb_* signatures only reference the type through a pointer.
fn guard_hdb_struct(content: String) -> String {
    let full_def = concat!(
        "typedef struct qb_hdb {\n",
        "  uint32_t _handle_count;\n",
        "  uint32_t _pad0;\n",
        "  void *_inner;\n",
        "  uint32_t _iterator_c;\n",
        "  uint32_t _pad1;\n",
        "  uintptr_t _destructor;\n",
        "  uint32_t _first_run;\n",
        "  uint32_t _pad2;\n",
        "} qb_hdb;",
    );
    let fwd_decl = "struct qb_hdb;\ntypedef struct qb_hdb qb_hdb;";
    content.replace(full_def, fwd_decl)
}

fn main() {
    let crate_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let out_path = PathBuf::from(&crate_dir).join("include").join("qb").join("qb.h");

    cbindgen::Builder::new()
        .with_crate(&crate_dir)
        .with_config(cbindgen::Config::from_file("cbindgen.toml").unwrap_or_default())
        .generate()
        .expect("Unable to generate C bindings")
        .write_to_file(&out_path);

    // Post-process: remove the bare `iovec` typedef and use `struct iovec` in
    // function signatures to match the original libqb ABI and avoid -Wshadow
    // warnings when callers name local variables `iovec`.
    let content = std::fs::read_to_string(&out_path).unwrap();
    let fixed = content
        .replace("typedef struct iovec iovec;\n", "")
        // Replace bare `iovec` type references but not ones already prefixed with `struct`.
        // Use word-boundary-aware replacements: longer patterns first.
        .replace("const iovec *", "const struct iovec *")
        .replace(", iovec *", ", struct iovec *");

    // Replace the cbindgen-generated struct qb_hdb body with a forward declaration.
    // qbhdb.h provides the complete definition with the public field names that
    // QB_HDB_DECLARE requires, without conflicting with this file regardless of
    // include order.  The forward declaration is sufficient here because all
    // qb_hdb_* function signatures only use the type through a pointer.
    let fixed = guard_hdb_struct(fixed);

    std::fs::write(&out_path, fixed).unwrap();

    // Compile C shims (va_list functions not expressible in stable Rust).
    // Use --whole-archive so the linker includes all symbols even if nothing
    // in the Rust code references them (qb_log_real_, qb_log_from_external_source, etc.).
    cc::Build::new()
        .file("src/log_va_shims.c")
        .compile("log_va_shims");

    let out_dir = env::var("OUT_DIR").unwrap();
    // Force inclusion of all C shim symbols (variadic fns not expressible in Rust).
    // --whole-archive includes all archive members; --no-gc-sections keeps them alive.
    println!("cargo:rustc-link-arg=-Wl,--whole-archive,{}/liblog_va_shims.a,--no-whole-archive", out_dir);
    println!("cargo:rustc-link-arg=-Wl,--no-gc-sections");

    // Override the Rust cdylib version script by supplying our own that includes
    // all Rust #[no_mangle] symbols PLUS the C shim symbols.
    // We add a supplementary version script that marks the C shims as global.
    // lld merges multiple --version-script inputs: later "global:" declarations
    // take precedence over earlier "local: *" wildcards.
    let extra_vs = format!("{}/shim_version_script.map", out_dir);
    std::fs::write(&extra_vs,
        "{ global:\n\
         qb_log_real_;\n\
         qb_log_real_va_;\n\
         qb_log_from_external_source;\n\
         qb_log_from_external_source_va;\n\
         qb_log_from_external_source_va2;\n\
         qb_log_from_external_source_msg_id;\n\
         qb_log_custom_open;\n\
         qb_vsnprintf_serialize;\n\
         qb_vsnprintf_deserialize;\n\
         local: *; };\n"
    ).unwrap();
    println!("cargo:rustc-link-arg=-Wl,--version-script={}", extra_vs);

    println!("cargo:rerun-if-changed=src/");
    println!("cargo:rerun-if-changed=cbindgen.toml");
}
