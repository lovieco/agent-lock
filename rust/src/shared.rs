//! Shared files: config files and manifests that several agents may edit at
//! once. A shared file's claim has co-holders instead of blocking: each edit
//! still runs alone (a write in flight blocks the others for its few ms), and
//! one commit by any holder takes everyone's changes in it.
//!
//! Built-in patterns, then `<top>/.agent-lock-shared` (committed, one glob per
//! line, `#` comments, `!pat` excludes; the last match wins). A pattern with
//! no `/` matches the file name anywhere; one with a `/` matches the path from
//! the top. `*` stays within a path segment, `**` crosses them.

use std::path::Path;

pub const FILE: &str = ".agent-lock-shared";

const DEFAULTS: &[&str] = &[
    // manifests and their lockfiles (installs serialize through the pkg lock)
    "package.json",
    "package-lock.json",
    "npm-shrinkwrap.json",
    "pnpm-lock.yaml",
    "pnpm-workspace.yaml",
    "yarn.lock",
    "bun.lock",
    "bun.lockb",
    "Cargo.toml",
    "Cargo.lock",
    "pyproject.toml",
    "uv.lock",
    "poetry.lock",
    "requirements*.txt",
    "go.mod",
    "go.sum",
    "Gemfile",
    "Gemfile.lock",
    // tool and editor config
    "tsconfig*.json",
    "jsconfig.json",
    "*.config.js",
    "*.config.cjs",
    "*.config.mjs",
    "*.config.ts",
    ".eslintrc*",
    ".prettierrc*",
    ".editorconfig",
    ".gitignore",
    ".gitattributes",
    ".env.example",
    "Makefile",
    // agent instructions
    "CLAUDE.md",
    "AGENTS.md",
    ".claude/settings.json",
];

#[derive(Clone, Debug, PartialEq)]
pub struct Shared {
    pats: Vec<(bool, String)>, // (excluded, pattern)
}

impl Default for Shared {
    fn default() -> Shared {
        Shared { pats: DEFAULTS.iter().map(|p| (false, p.to_string())).collect() }
    }
}

impl Shared {
    /// The built-in patterns plus the repo's `.agent-lock-shared`, if any.
    pub fn load(top: &Path) -> Shared {
        let mut s = Shared::default();
        if let Ok(text) = std::fs::read_to_string(top.join(FILE)) {
            s.add(&text);
        }
        s
    }

    fn add(&mut self, text: &str) {
        for l in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
            let (neg, p) = match l.strip_prefix('!') {
                Some(p) => (true, p),
                None => (false, l),
            };
            let p = p.trim_start_matches('/');
            if !p.is_empty() {
                self.pats.push((neg, p.to_string()));
            }
        }
    }

    /// `rel` (repo-relative, `/`-separated) is a shared file.
    pub fn is(&self, rel: &str) -> bool {
        let name = rel.rsplit('/').next().unwrap_or(rel);
        let mut hit = false;
        for (neg, p) in &self.pats {
            let m = if p.contains('/') { glob(p.as_bytes(), rel.as_bytes()) } else { glob(p.as_bytes(), name.as_bytes()) };
            if m {
                hit = !neg;
            }
        }
        hit
    }
}

/// `*` and `?` stay within a segment; `**` matches across `/`.
fn glob(p: &[u8], s: &[u8]) -> bool {
    match p.first() {
        None => s.is_empty(),
        Some(b'*') if p.get(1) == Some(&b'*') => {
            let rest = p[2..].strip_prefix(b"/").unwrap_or(&p[2..]);
            rest.is_empty()
                || (0..=s.len()).any(|i| (i == 0 || s[i - 1] == b'/') && glob(rest, &s[i..])) || glob(&p[2..], s)
        }
        Some(b'*') => (0..=s.len()).take_while(|&i| i == 0 || s[i - 1] != b'/').any(|i| glob(&p[1..], &s[i..])),
        Some(b'?') => s.first().is_some_and(|&c| c != b'/') && glob(&p[1..], &s[1..]),
        Some(&c) => s.first() == Some(&c) && glob(&p[1..], &s[1..]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_config_files_anywhere() {
        let s = Shared::default();
        for p in ["package.json", "web/package.json", "tsconfig.json", "tsconfig.build.json", "vite.config.ts", "a/b/.gitignore", ".claude/settings.json"] {
            assert!(s.is(p), "{p}");
        }
        for p in ["src/main.rs", "package.json.bak", "src/config.ts", "x/.claude/settings.json.local", "docs/settings.json"] {
            assert!(!s.is(p), "{p}");
        }
    }

    #[test]
    fn the_repo_file_adds_and_excludes() {
        let mut s = Shared::default();
        s.add("# routes are edited by everyone\nsrc/routes.ts\nconfig/**/*.yaml\n!CLAUDE.md\n\n/i18n/*.json\n");
        assert!(s.is("src/routes.ts") && !s.is("lib/src/routes.ts"), "a path pattern is anchored at the top");
        assert!(s.is("config/a.yaml") && s.is("config/x/y/b.yaml") && !s.is("config.yaml"));
        assert!(!s.is("CLAUDE.md"), "excluded");
        assert!(s.is("i18n/en.json") && !s.is("i18n/en/x.json"), "* stays in its segment");
    }

    #[test]
    fn globs() {
        assert!(glob(b"*.config.*", b"vite.config.ts"));
        assert!(!glob(b"*", b"a/b"));
        assert!(glob(b"**", b"a/b"));
        assert!(glob(b"a/**/c", b"a/c") && glob(b"a/**/c", b"a/x/y/c") && !glob(b"a/**/c", b"ab/c"));
        assert!(glob(b"?.md", b"a.md") && !glob(b"?.md", b"/.md"));
    }
}
