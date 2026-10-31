//! Builds the React control page and hands it to the Rust compiler.
//!
//! The page lives in `ui/` (React + Vite + Tailwind, built with pnpm). `src/api.rs`
//! embeds its files from `OUT_DIR`, so the binary stays self-contained and a plain
//! `cargo build` needs no manual UI step — this script runs pnpm itself whenever the
//! UI sources are newer than the built page. Without pnpm (and without a previously
//! built page) it embeds placeholders and warns.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

const UI_SOURCES: [&str; 5] = [
    "ui/src",
    "ui/index.html",
    "ui/package.json",
    "ui/pnpm-lock.yaml",
    "ui/vite.config.ts",
];

const INDEX_PLACEHOLDER: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>unsonido</title>
<style>
  body { font: 15px/1.5 -apple-system, sans-serif; display: grid; place-items: center;
         min-height: 100vh; margin: 0; color: #6b6b66; }
  code { background: #f0f0ee; padding: 2px 6px; border-radius: 6px; }
</style>
</head>
<body>
<div>
  <p><b>unsonido</b> — the web UI has not been built.</p>
  <p>Run <code>pnpm --dir ui build</code> (needs Node and pnpm) and rebuild the server.</p>
</div>
</body>
</html>"#;

const JS_PLACEHOLDER: &str =
    "// unsonido UI placeholder: run `pnpm --dir ui build` for the real page.\n";
const CSS_PLACEHOLDER: &str =
    "/* unsonido UI placeholder: run `pnpm --dir ui build` for the real page. */\n";

fn main() {
    for source in UI_SOURCES {
        println!("cargo:rerun-if-changed={source}");
    }
    // Manual `pnpm --dir ui build` outpaces us; track the built page so its
    // changes re-run this script (not recursive; the rest of dist always
    // changes together with index.html).
    println!("cargo:rerun-if-changed=ui/dist/index.html");

    if !page_is_fresh() {
        match pnpm() {
            Some(pnpm) => {
                ensure_node_modules(&pnpm);
                run(&pnpm, &["run", "build"]);
            }
            None => println!(
                "cargo:warning=unsonido: UI sources changed but pnpm was not found; \
                 embedding a placeholder page. Install Node and pnpm (`corepack enable pnpm`), \
                 or run `pnpm --dir ui build` manually."
            ),
        }
    }
    emit();
}

/// True when the built page is at least as new as every UI source file.
fn page_is_fresh() -> bool {
    let fresh = match built_at() {
        Some(built) => sources_newest() <= built,
        None => false,
    };
    fresh
}

fn built_at() -> Option<SystemTime> {
    fs::symlink_metadata(Path::new("ui/dist/index.html"))
        .and_then(|meta| meta.modified())
        .ok()
}

fn sources_newest() -> SystemTime {
    let mut newest = SystemTime::UNIX_EPOCH;
    for source in UI_SOURCES {
        newest_mtime(Path::new(source), &mut newest);
    }
    newest
}

fn newest_mtime(path: &Path, newest: &mut SystemTime) {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => {
            let entries = fs::read_dir(path).expect("readable ui directory");
            for entry in entries.flatten() {
                newest_mtime(&entry.path(), newest);
            }
        }
        Ok(meta) => {
            if let Ok(modified) = meta.modified()
                && modified > *newest
            {
                *newest = modified;
            }
        }
        Err(_) => {} // a source file was removed; whatever is left decides
    }
}

fn pnpm() -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    for dir in env::split_paths(&path) {
        let candidate = dir.join("pnpm");
        if is_executable(&candidate) {
            return Some(candidate);
        }
        let candidate = dir.join("pnpm.cmd");
        if is_executable(&candidate) && cfg!(windows) {
            return Some(candidate);
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match fs::metadata(path) {
        Ok(meta) => meta.is_file() && meta.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

fn ensure_node_modules(pnpm: &Path) {
    if Path::new("ui/node_modules/.pnpm").exists() {
        return;
    }
    run(pnpm, &["install", "--frozen-lockfile"]);
}

fn run(pnpm: &Path, args: &[&str]) {
    let outcome = Command::new(pnpm)
        .args(args)
        .current_dir("ui")
        .output()
        .unwrap_or_else(|err| panic!("cannot run pnpm: {err}"));
    if !outcome.status.success() {
        if !outcome.stdout.is_empty() {
            eprintln!("{}", String::from_utf8_lossy(&outcome.stdout));
        }
        if !outcome.stderr.is_empty() {
            eprintln!("{}", String::from_utf8_lossy(&outcome.stderr));
        }
        panic!(
            "UI build failed: pnpm {} exited with {}",
            args.join(" "),
            outcome.status
        );
    }
}

/// Copy the built page (or placeholders) into OUT_DIR, where api.rs embeds it.
fn emit() {
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set by cargo"));
    let target = out.join("unsonido-ui");
    fs::create_dir_all(target.join("assets")).expect("OUT_DIR is writable");

    let index = Path::new("ui/dist/index.html");
    match index.exists() {
        true => {
            fs::copy(index, target.join("index.html")).expect("copy index.html");
            for name in ["app.js", "app.css"] {
                fs::copy(
                    format!("ui/dist/assets/{name}"),
                    target.join("assets").join(name),
                )
                .expect("copy built asset");
            }
        }
        false => {
            fs::write(target.join("index.html"), INDEX_PLACEHOLDER).expect("write placeholder");
            fs::write(target.join("assets/app.js"), JS_PLACEHOLDER).expect("write placeholder");
            fs::write(target.join("assets/app.css"), CSS_PLACEHOLDER).expect("write placeholder");
        }
    }
}
