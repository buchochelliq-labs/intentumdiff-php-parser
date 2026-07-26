fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let target = std::env::var("TARGET").unwrap_or_default();
    if target == "wasm32-wasip2" {
        println!("cargo:rustc-link-lib=static=tree_sitter_php");
        println!("cargo:rustc-link-search=native={}/lib", manifest);
        println!("cargo:rerun-if-changed=lib/libtree_sitter_php.a");
        return;
    }

    let mut cfg = cc::Build::new();
    cfg.std("c11")
        .include("php/src")
        .include("php_only/src");
    if std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default() == "msvc" {
        cfg.flag("-utf-8");
    }
    cfg
        .file("php/src/parser.c")
        .file("php/src/scanner.c")
        .file("php_only/src/parser.c")
        .file("php_only/src/scanner.c")
        .compile("tree_sitter_php");
}
