//! Git operations end to end. git.rs is a private module of the binary, so
//! its behavior is covered by the unit tests in src/git.rs (real temp repos,
//! hermetic git, simulated kills). These drive the same flows through the
//! CLI contract (`agent-lock sync|push`, exit 0 ok / 1 refused).
mod common;
use common::*;

fn upstream_change(r: &Repo, file: &str, text: &str) -> String {
    let up = r.base.join("up");
    if !up.exists() {
        clone(r, "up");
    }
    git(r, &up, &["pull", "-q", "--ff-only", "origin", "main"]);
    write(&up, file, text);
    git(r, &up, &["add", "-A"]);
    git(r, &up, &["commit", "-q", "-m", &format!("up: {file}")]);
    git(r, &up, &["push", "-q", "origin", "main"]);
    git(r, &up, &["rev-parse", "HEAD"])
}

#[test]
fn sync_and_push_through_the_cli() {
    let r = make_repo(true, &[("a.txt", "a\n")]);
    let pid = std::process::id();
    let u = upstream_change(&r, "u.txt", "u\n");
    write(&r.dir, "mine.txt", "dirty, uncommitted\n");
    let s = cli(&r, &r.dir, &["sync"], Some(SA), pid);
    assert_eq!(s.status, 0, "{}", s.out());
    assert_eq!(git(&r, &r.dir, &["rev-parse", "HEAD"]), u);
    assert_eq!(std::fs::read_to_string(r.dir.join("mine.txt")).unwrap(), "dirty, uncommitted\n");
    // a local commit racing an upstream one: push syncs and retries
    write(&r.dir, "a.txt", "a2\n");
    git(&r, &r.dir, &["commit", "-q", "-am", "local"]);
    upstream_change(&r, "v.txt", "v\n");
    let p = cli(&r, &r.dir, &["push"], Some(SA), pid);
    assert_eq!(p.status, 0, "{}", p.out());
    let origin = r.origin.clone().unwrap();
    assert_eq!(git(&r, &origin, &["rev-parse", "main"]), git(&r, &r.dir, &["rev-parse", "HEAD"]));
    assert!(state(&r).get("locks").get("git").is_null() && state(&r).get("locks").get("sync").is_null());
}

#[test]
fn sync_refuses_a_conflict_through_the_cli() {
    let r = make_repo(true, &[("a.txt", "a\n")]);
    let pid = std::process::id();
    upstream_change(&r, "a.txt", "theirs\n");
    write(&r.dir, "a.txt", "mine\n");
    git(&r, &r.dir, &["commit", "-q", "-am", "mine"]);
    let before = git(&r, &r.dir, &["rev-parse", "HEAD"]);
    let s = cli(&r, &r.dir, &["sync"], Some(SA), pid);
    assert_eq!(s.status, 1, "{}", s.out());
    assert!(s.out().contains("a.txt"), "{}", s.out());
    assert_eq!(git(&r, &r.dir, &["rev-parse", "HEAD"]), before);
}
