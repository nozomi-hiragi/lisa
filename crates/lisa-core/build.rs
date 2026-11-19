//! リポジトリの `catalogs/*.toml` を組み込みの型として埋め込む。

use std::env;
use std::fs;
use std::io::Write;
use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let catalogs_dir = manifest_dir.join("../../catalogs");
    println!("cargo:rerun-if-changed={}", catalogs_dir.display());

    let mut files: Vec<(String, PathBuf)> = Vec::new();
    if let Ok(entries) = fs::read_dir(&catalogs_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("toml")
                && !path.is_dir()
                && let Some(name) = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .map(str::to_string)
            {
                let abs = fs::canonicalize(&path).unwrap_or(path);
                println!("cargo:rerun-if-changed={}", abs.display());
                files.push((name, abs));
            }
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));

    let out_path = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("builtin_catalogs.rs");
    let mut out = fs::File::create(&out_path).unwrap();
    writeln!(out, "pub static BUILTIN: &[(&str, &str)] = &[").unwrap();
    for (name, path) in files {
        let path_str = path.to_string_lossy();
        writeln!(out, "    ({name:?}, include_str!({path_str:?})),").unwrap();
    }
    writeln!(out, "];").unwrap();
}
