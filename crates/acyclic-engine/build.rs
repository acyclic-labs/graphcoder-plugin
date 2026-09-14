//! Reads the repo-level `product.toml` and exposes its keys to the crate as
//! compile-time environment variables (see `src/product.rs`).

use std::path::Path;

fn main() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let path = manifest.join("../../product.toml");
    println!("cargo:rerun-if-changed={}", path.display());
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    let table: toml::Table = text
        .parse()
        .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()));
    let get = |key: &str| -> String {
        table
            .get(key)
            .and_then(toml::Value::as_str)
            .unwrap_or_else(|| panic!("product.toml: missing string key `{key}`"))
            .to_owned()
    };
    let name = get("name");
    assert!(
        !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
        "product.toml: `name` must be lowercase ASCII letters, digits, or '-' (it becomes a command, a dotfile, and an env-var prefix)"
    );
    println!("cargo:rustc-env=PRODUCT_NAME={name}");
    println!(
        "cargo:rustc-env=PRODUCT_ENV_PREFIX={}",
        name.to_ascii_uppercase().replace('-', "_")
    );
    println!("cargo:rustc-env=PRODUCT_NPM_PACKAGE={}", get("npm_package"));
    println!("cargo:rustc-env=PRODUCT_GITHUB_REPO={}", get("github_repo"));
}
