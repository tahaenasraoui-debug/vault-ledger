use std::fs;
use std::path::Path;

const FORBIDDEN_DIRECT_CRATES: &[&str] = &[
    "reqwest",
    "reqwest-middleware",
    "curl",
    "curl-sys",
    "hyper",
    "hyper-util",
    "ureq",
    "isahc",
    "surf",
    "attohttpc",
    "awc",
    "tauri-plugin-http",
    "hf-hub",
];

const FORBIDDEN_ENV_PREFIXES: &[&str] = &["DEP_CURL", "CARGO_FEATURE_HTTP"];

const BUNDLED_MODEL_RESOURCES: &[&str] = &[
    "resources/models/gemma-2b-it-q4_k_m.gguf",
    "resources/models/tokenizer.json",
    "resources/ocr/text-detection.rten",
    "resources/ocr/text-recognition.rten",
];

fn enforce_airgap_manifest(manifest_path: &Path) {
    let manifest = fs::read_to_string(manifest_path).unwrap_or_else(|err| {
        panic!(
            "AIR-GAP CHECK FAILURE: unable to read '{}': {}",
            manifest_path.display(),
            err
        )
    });

    let mut in_dependency_section = false;

    for (line_idx, raw_line) in manifest.lines().enumerate() {
        let line = raw_line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }

        if line.starts_with('[') && line.ends_with(']') {
            let header = &line[1..line.len() - 1].trim();

            for suffix in ["dependencies.", "build-dependencies.", "dev-dependencies."] {
                if let Some(rest) = header.split(suffix).nth(1) {
                    let crate_name = rest
                        .split('.')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .trim_matches('"');
                    if FORBIDDEN_DIRECT_CRATES.contains(&crate_name) {
                        panic!(
                            "AIR-GAP VIOLATION: Forbidden network crate '{}' declared in {} (line {}).",
                            crate_name,
                            manifest_path.display(),
                            line_idx + 1
                        );
                    }
                }
            }

            in_dependency_section = *header == "dependencies"
                || *header == "build-dependencies"
                || *header == "dev-dependencies"
                || header.ends_with(".dependencies")
                || header.ends_with(".build-dependencies")
                || header.ends_with(".dev-dependencies");
            continue;
        }

        if in_dependency_section {
            if let Some((raw_key, raw_val)) = line.split_once('=') {
                let key = raw_key.trim().trim_matches('"');
                if FORBIDDEN_DIRECT_CRATES.contains(&key) {
                    panic!(
                        "AIR-GAP VIOLATION: Forbidden network crate '{}' declared in {} (line {}).",
                        key,
                        manifest_path.display(),
                        line_idx + 1
                    );
                }

                if key == "tokenizers" && raw_val.contains("\"http\"") {
                    panic!(
                        "AIR-GAP VIOLATION: 'tokenizers' must not enable the 'http' feature ({}:{}).",
                        manifest_path.display(),
                        line_idx + 1
                    );
                }
            }
        }
    }
}

fn main() {
    println!("cargo:rerun-if-changed=Cargo.toml");

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    let manifest_root = Path::new(&manifest_dir);
    enforce_airgap_manifest(&manifest_root.join("Cargo.toml"));

    for (key, _value) in std::env::vars() {
        for forbidden in FORBIDDEN_ENV_PREFIXES {
            if key.starts_with(forbidden) {
                panic!(
                    "AIR-GAP VIOLATION: Forbidden network build indicator '{}' detected at compile time.",
                    key
                );
            }
        }
    }

    let mut missing_resources = Vec::new();
    for rel_path in BUNDLED_MODEL_RESOURCES {
        println!("cargo:rerun-if-changed={}", rel_path);
        if !manifest_root.join(rel_path).exists() {
            missing_resources.push(*rel_path);
        }
    }

    // Allow debug builds (`cargo check`, `cargo test`, `cargo clippy`) to succeed without
    // downloading multi-GB gitignored model weights, while release bundles still enforce them.
    let profile = std::env::var("PROFILE").unwrap_or_default();
    if profile == "debug"
        && !missing_resources.is_empty()
        && std::env::var_os("TAURI_CONFIG").is_none()
    {
        println!(
            "cargo:warning=Skipping Tauri resource bundling in debug build because local model files are missing: {}. See README.md to install them before running the desktop app.",
            missing_resources.join(", ")
        );
        std::env::set_var("TAURI_CONFIG", r#"{"bundle":{"resources":[]}}"#);
    }

    tauri_build::build()
}
