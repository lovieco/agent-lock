//! Package manifests and the lockfiles that pair with them: the package shims
//! claim both before a dependency change, and `agent-lock commit` commits a
//! manifest together with its lockfile.

use std::path::{Path, PathBuf};

/// manifest → the lockfiles committed with it.
pub const PAIRS: &[(&str, &[&str])] = &[
    ("package.json", &["package-lock.json", "npm-shrinkwrap.json", "pnpm-lock.yaml", "yarn.lock", "bun.lock", "bun.lockb"]),
    ("pyproject.toml", &["uv.lock", "poetry.lock"]),
    ("Cargo.toml", &["Cargo.lock"]),
    ("go.mod", &["go.sum"]),
    ("Gemfile", &["Gemfile.lock"]),
];

/// Manifests whose lockfile may sit at a workspace root above them.
pub const UPWARD: &[&str] = &["package.json", "Cargo.toml", "pyproject.toml"];

/// A package manager's manifest and the lockfiles it writes.
pub fn manager(name: &str) -> Option<(&'static str, &'static [&'static str])> {
    Some(match name {
        "npm" => ("package.json", &["package-lock.json", "npm-shrinkwrap.json"]),
        "pnpm" => ("package.json", &["pnpm-lock.yaml"]),
        "yarn" => ("package.json", &["yarn.lock"]),
        "bun" => ("package.json", &["bun.lock", "bun.lockb"]),
        "uv" => ("pyproject.toml", &["uv.lock"]),
        "poetry" => ("pyproject.toml", &["poetry.lock"]),
        "cargo" => ("Cargo.toml", &["Cargo.lock"]),
        "go" => ("go.mod", &["go.sum"]),
        "bundle" | "bundler" => ("Gemfile", &["Gemfile.lock"]),
        _ => return None,
    })
}

fn present(p: &Path) -> bool {
    p.symlink_metadata().is_ok()
}

/// The lockfiles of `manifest`: the existing ones next to it, else upward to
/// `top` for workspace-style manifests (the nearest wins). With a `manager`
/// only its own lockfiles count, and when none exists yet the one it will
/// create is predicted at the outermost manifest (the workspace root).
pub fn lockfiles_for(manifest: &Path, top: &Path, manager: Option<&str>) -> Vec<PathBuf> {
    let Some(base) = manifest.file_name().and_then(|n| n.to_str()) else { return Vec::new() };
    let names = match manager {
        Some(m) => match self::manager(m) {
            Some((file, locks)) if file == base => locks,
            _ => return Vec::new(),
        },
        None => match PAIRS.iter().find(|(m, _)| *m == base) {
            Some((_, locks)) => *locks,
            None => return Vec::new(),
        },
    };
    let Some(start) = manifest.parent() else { return Vec::new() };
    let (mut d, mut root) = (start, start);
    while d.starts_with(top) {
        let found: Vec<PathBuf> = names.iter().map(|n| d.join(n)).filter(|p| present(p)).collect();
        if !found.is_empty() {
            return found;
        }
        if d != start && present(&d.join(base)) {
            root = d;
        }
        match d.parent() {
            Some(up) if UPWARD.contains(&base) && d != top => d = up,
            _ => break,
        }
    }
    if manager.is_some() { vec![root.join(names[0])] } else { Vec::new() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lockfiles_pair_next_to_or_above_the_manifest() {
        let top = std::env::temp_dir().join(format!("al-lf-{}-{}", crate::sys::pid(), crate::sys::now_ms()));
        for d in ["pkgs/a", "crates/x", "m", "w/p/q"] {
            std::fs::create_dir_all(top.join(d)).unwrap();
        }
        for f in ["pnpm-lock.yaml", "pkgs/a/package.json", "Cargo.lock", "crates/x/Cargo.toml", "go.sum", "go.mod"] {
            std::fs::write(top.join(f), "").unwrap();
        }
        let lf = |m: &str, mgr: Option<&str>| lockfiles_for(&top.join(m), &top, mgr);
        assert_eq!(lf("pkgs/a/package.json", None), vec![top.join("pnpm-lock.yaml")]);
        assert_eq!(lf("crates/x/Cargo.toml", None), vec![top.join("Cargo.lock")]);
        assert_eq!(lf("go.mod", None), vec![top.join("go.sum")]);
        assert!(lf("m/go.mod", None).is_empty(), "go.sum is never searched upward");
        assert!(lf("README.md", None).is_empty());
        assert!(lockfiles_for(&top.join("pkgs/a/package.json"), &top.join("pkgs/a"), None).is_empty(), "not above the top");
        // a manager sees only its own lockfiles, and predicts one at the outermost manifest
        assert_eq!(lf("pkgs/a/package.json", Some("pnpm")), vec![top.join("pnpm-lock.yaml")]);
        assert_eq!(lf("pkgs/a/package.json", Some("npm")), vec![top.join("pkgs/a/package-lock.json")], "no root manifest yet");
        std::fs::write(top.join("package.json"), "").unwrap();
        assert_eq!(lf("pkgs/a/package.json", Some("npm")), vec![top.join("package-lock.json")]);
        assert_eq!(lf("m/go.mod", Some("go")), vec![top.join("m/go.sum")]);
        assert!(lf("crates/x/Cargo.toml", Some("npm")).is_empty(), "another manager's manifest");
        std::fs::write(top.join("w/Cargo.toml"), "").unwrap();
        std::fs::write(top.join("w/p/q/Cargo.toml"), "").unwrap();
        std::fs::remove_file(top.join("Cargo.lock")).unwrap();
        assert_eq!(lf("w/p/q/Cargo.toml", Some("cargo")), vec![top.join("w/Cargo.lock")], "the workspace root's");
        std::fs::write(top.join("pkgs/a/package-lock.json"), "").unwrap();
        assert_eq!(lf("pkgs/a/package.json", None), vec![top.join("pkgs/a/package-lock.json")], "the nearest wins");
        assert_eq!(manager("bundler"), manager("bundle"));
        assert_eq!(manager("make"), None);
        let _ = std::fs::remove_dir_all(&top);
    }
}
