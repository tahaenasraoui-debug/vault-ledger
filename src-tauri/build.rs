fn main() {
    let forbidden_env_prefixes = [
        "DEP_CURL",
        "CARGO_FEATURE_HTTP",
        "CARGO_PKG_REQWEST",
        "CARGO_PKG_UREQ",
        "CARGO_PKG_HYPER",
    ];

    for (key, _value) in std::env::vars() {
        for forbidden in &forbidden_env_prefixes {
            if key.starts_with(forbidden) {
                panic!(
                    "AIR-GAP VIOLATION: Forbidden network dependency indicator '{}' detected at build time.",
                    key
                );
            }
        }
    }

    tauri_build::build()
}
