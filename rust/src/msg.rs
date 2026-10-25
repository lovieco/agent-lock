//! Every agent-facing sentence: short, specific, actionable. Prose names a
//! file by its repo-relative path; every command meant to be copied uses the
//! absolute, shell-quoted path, carries `--as <id>` for a subagent and
//! `-C <top>` when the agent's shell is not in that repo.

use crate::ids;
use crate::sys::plural;
pub use crate::sys::quote as q;
use std::path::{Path, PathBuf};

const T: &str = "[agent-lock] ";

pub fn qp(p: &Path) -> String {
    q(&p.to_string_lossy())
}

/// `5s`, `3m`, `2h 5m`.
pub fn ago(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s % 3600 < 60 => format!("{}h", s / 3600),
        s => format!("{}h {}m", s / 3600, s % 3600 / 60),
    }
}

/// ` --as <id>` for a subagent (its environment says it is the parent).
pub(crate) fn as_flag(me: &str) -> String {
    if ids::is_sub(me) { format!(" --as {me}") } else { String::new() }
}

/// ` -C <top>` when the agent's shell is not in that repo.
fn c_flag(top: Option<&Path>) -> String {
    top.map(|t| format!(" -C {}", qp(t))).unwrap_or_default()
}

fn list(files: &[String], max: usize) -> String {
    let mut s = files.iter().take(max).cloned().collect::<Vec<_>>().join(", ");
    if files.len() > max {
        s.push_str(&format!(", … and {} more", files.len() - max));
    }
    s
}

pub(crate) fn git_diff(top: &Path, rel: &str) -> String {
    format!("git -C {} diff -- {}", qp(top), q(rel))
}

// ── context ───────────────────────────────────────────────────────────────

/// `top`: the repo, when the agent's shell is elsewhere (commands then need `-C`).
fn rules(me: &str, top: Option<&Path>) -> String {
    let a = format!("{}{}", as_flag(me), c_flag(top));
    format!(
        "Your edits claim files until you commit them. If an edit is BLOCKED, do other work: the holder keeps the file until it commits. \
Never write a claimed file another way (redirection, sed -i, cp, scripts). \
Commit with `agent-lock commit{a} -m \"<msg>\"`: it replaces git add + git commit (`-m \"$(cat <<'EOF' … EOF)\"` works). \
Publish with `agent-lock push{a}`; pull with `agent-lock sync{a}`. \
While others work, the git shim refuses whole-tree git (add -A, commit -a, stash, reset --hard, checkout ., pull, rebase); installs queue. \
Run formatters only on your files: `agent-lock mine{a} -z | xargs -0 -r <tool>`. \
An index.lock error means wait and retry; never delete it."
    )
}

pub fn session_context(me: &str, top: Option<&Path>) -> String {
    format!("{T}This checkout may be shared with other agents working on this branch. Your agent id is {me}. {}", rules(me, top))
}

pub fn subagent_context(me: &str, top: Option<&Path>) -> String {
    format!(
        "{T}This checkout may be shared with other agents working on this branch. Your agent id is {me}; pass `--as {me}` to every agent-lock command and start package-manager commands with `AGENT_LOCK_AGENT_ID={me}` (e.g. `AGENT_LOCK_AGENT_ID={me} npm install x`): you share your parent's environment, so without them agent-lock acts as your parent. {}",
        rules(me, top)
    )
}

// ── pre-edit ──────────────────────────────────────────────────────────────

/// What a BLOCKED edit knows about the holder.
pub struct Blocked<'a> {
    pub abs: &'a Path,
    pub rel: &'a str,
    pub holder: &'a str,
    pub claimed: u64, // seconds since the claim was made
    pub idle: u64,    // seconds since the holder was last active
    pub alive: Option<bool>,
    pub edits: u32,
    pub never_ran: bool,       // no edits, a write pending for ≥ 60 s
    pub own_sub: Option<bool>, // the requester's own subagent holds it: Some(still running)
    pub inherited: bool,       // the requester's uncommitted edits moved with the claim
    pub me: &'a str,
}

pub fn blocked(b: &Blocked) -> String {
    let abs = qp(b.abs);
    let a = as_flag(b.me);
    let moved = if b.inherited { " Your uncommitted changes to it moved with the file and come back with it." } else { "" };
    match b.own_sub {
        Some(true) => {
            return format!(
                "{T}BLOCKED: {} is being edited by your subagent {}, which is still running. Wait for its completion notice: its files pass to you when it finishes.{moved} Do other work meanwhile; never write the file another way. If it was stopped and no notice comes, its files pass to you when your turn ends (to take this one sooner, ask the user: `agent-lock release --force {abs}` needs their approval).",
                b.rel, b.holder
            );
        }
        Some(false) => {
            return format!(
                "{T}BLOCKED: {} is held by your subagent {}, which has finished, but its last tool call on the file never completed.{moved} It passes to you when your turn ends; do other work meanwhile and never write the file another way.",
                b.rel, b.holder
            );
        }
        None => {}
    }
    let mut facts = vec![format!("claimed {} ago", ago(b.claimed)), format!("last active {} ago", ago(b.idle))];
    facts.push(match b.alive {
        Some(true) => "holder process alive".into(),
        Some(false) => "holder process gone".into(),
        None => "holder process unknown".into(),
    });
    if b.never_ran {
        facts.push("its edit never ran".into());
    } else if b.edits > 0 {
        facts.push(plural(b.edits as usize, "uncommitted edit"));
    }
    let (why, out) = if b.never_ran {
        (
            "Nothing was written yet: the holder's edit was probably rejected at a permission prompt (or the prompt is still open). Prefer other work.",
            format!("With the user's approval, `agent-lock release{a} --force {abs}` is safe (the holder has no edits in it)."),
        )
    } else {
        (
            "The holder keeps this file until it commits: do other work now.",
            format!("If the holder looks abandoned, ask the user: `agent-lock release{a} --force {abs}` needs their approval."),
        )
    };
    format!(
        "{T}BLOCKED: {} is claimed by {} ({}).\n{why}\nIf you must wait: `agent-lock wait{a} --claim {abs} --timeout 540` with run_in_background.\nNever write it another way (redirection, sed -i, cp, scripts).\n{out}",
        b.rel,
        b.holder,
        facts.join(", ")
    )
}

/// `chain`: each wait from the holder back to the requester (rules::wait_chain).
pub fn deadlock(rel: &str, holder: &str, chain: &[(String, String, String)], me: &str, top: Option<&Path>) -> String {
    let waits: Vec<String> = chain
        .iter()
        .map(
            |(who, path, by)| {
                if by == me { format!("{who} is waiting for {path} (yours)") } else { format!("{who} is waiting for {path} (held by {by})") }
            },
        )
        .collect();
    format!(
        "{T}DEADLOCK: {rel} is claimed by {holder}, and {}.\nCommit the files you hold now (`agent-lock commit{}{} -m \"<msg>\"`) so all of you can proceed.",
        waits.join(", "),
        as_flag(me),
        c_flag(top)
    )
}

/// A stale claim with uncommitted edits was taken over; the edit is refused once.
pub fn takeover(abs: &Path, rel: &str, top: &Path, from: &str, why: &str, edits: u32, me: &str) -> String {
    format!(
        "{T}TAKEOVER: {rel} was claimed by {from} ({why}), which left {} in it.\nThe claim is now yours, so your commit of this file will include those changes: look at `{}` first.\nRetry the edit to proceed, or leave the file alone: `agent-lock release{} {}`.",
        plural(edits as usize, "uncommitted edit"),
        git_diff(top, rel),
        as_flag(me),
        qp(abs)
    )
}

/// The same, when no other agent is active: the edit goes ahead.
pub fn takeover_note(abs: &Path, rel: &str, top: &Path, from: &str, why: &str, edits: u32, me: &str) -> String {
    format!(
        "{T}Note: {rel} was claimed by {from} ({why}), which left {} in it. The claim is now yours and this edit goes ahead, so your commit of this file will include those changes: check `{}` before committing it as your own (or `agent-lock release{} {}` if they are not yours to keep).",
        plural(edits as usize, "uncommitted edit"),
        git_diff(top, rel),
        as_flag(me),
        qp(abs)
    )
}

/// The caller joined a shared file (a config file several agents may edit).
pub fn shared_note(rel: &str, top: &Path, with: &[String]) -> String {
    format!(
        "{T}Note: {rel} is a shared file, also being edited by {}. Edit it freely (one edit runs at a time; re-read it if an edit says it changed). Whoever commits it first commits everyone's changes in it: check `{}` first.",
        with.join(", "),
        git_diff(top, rel)
    )
}

/// A claim was dropped without a commit; its changes may still be there.
pub fn orphan_note(rel: &str, top: &Path, from: &str) -> String {
    format!(
        "{T}Note: {rel} may still hold uncommitted changes by {from}, whose claim on it ended without a commit. Your commit of this file would include them: check `{}` before committing it as your own.",
        git_diff(top, rel)
    )
}

/// To a subagent whose claim came from its parent or a finished sibling with
/// uncommitted edits.
pub fn inherited(rel: &str, from: &str, me: &str, top: Option<&Path>) -> String {
    let who = if ids::is_sub(from) { "another subagent of your session" } else { "your parent agent" };
    format!(
        "{T}Note: {rel} carries uncommitted changes by {from}, {who}; the claim moved to you with them. Leave committing this file to your parent: `agent-lock commit{}{} -m \"<msg>\"` leaves it out unless you name it.",
        as_flag(me),
        c_flag(top)
    )
}

/// A first claim on a file that already differs from HEAD.
pub fn dirty_note(rel: &str, top: &Path) -> String {
    format!(
        "{T}Note: {rel} already has uncommitted changes that were not made through your edits (another agent's Bash command, the user, or an earlier session). Your commit of this file will include them: check `{}` first.",
        git_diff(top, rel)
    )
}

pub fn not_owner(rel: &str, holder: &str) -> String {
    format!(
        "{T}Warning: your write to {rel} landed while it is claimed by {holder}. Do not edit it again; tell the user so they can sort out whose change it is."
    )
}

pub fn sync_busy(rel: &str, by: &str) -> String {
    format!("{T}BLOCKED: `agent-lock sync` (run by {by}) is updating {rel} from upstream right now. Retry in a moment.")
}

pub fn state_busy(rel: &str, by: &str) -> String {
    format!("{T}BLOCKED: {rel} is claimed by {by}, and agent-lock's state file stayed locked for seconds, so this edit cannot be checked. Retry it in a moment; never write the file another way.")
}

// ── holder side ───────────────────────────────────────────────────────────

/// To the holder, once per waiter and file. `top`: the repo, when the agent's shell is elsewhere.
pub fn waiter_notice(waiter: &str, rel: &str, abs: &Path, me: &str, top: Option<&Path>) -> String {
    format!(
        "{T}{waiter} is blocked on {rel} (yours). When your change to it is consistent, commit it now: `agent-lock commit{}{} -m \"<msg>\" -- {}`.",
        as_flag(me),
        c_flag(top),
        qp(abs)
    )
}

/// End-of-turn reminder. `groups`: (repo top when the shell is elsewhere, files);
/// `waiters`: (waiter, file).
pub fn commit_reminder(groups: &[(Option<PathBuf>, Vec<String>)], waiters: &[(String, String)]) -> String {
    let what: Vec<String> = groups
        .iter()
        .map(|(top, files)| match top {
            Some(t) => format!("{} (in {})", list(files, 5), t.display()),
            None => list(files, 5),
        })
        .collect();
    let mut lines = vec![format!("{T}You have uncommitted changes in files you claimed: {}.", what.join("; "))];
    for (w, f) in waiters.iter().take(5) {
        lines.push(format!("{w} is waiting for {f}."));
    }
    let cmds: Vec<String> = groups.iter().map(|(top, _)| format!("`agent-lock commit{} -m \"<msg>\"`", c_flag(top.as_deref()))).collect();
    lines.push(format!(
        "The user set up this checkout so agents commit their own finished work. Unless the user asked you not to commit yet: if your change is complete and consistent, run {} (add `--push` only if the user wants it published). If the user wants to review first, or the work is mid-change, say so in one line and stop.",
        cmds.join(" and ")
    ));
    lines.join("\n")
}

// ── Bash ──────────────────────────────────────────────────────────────────

/// `files`: (repo-relative path, holder).
pub fn bash_wrote(files: &[(String, String)]) -> String {
    let xs: Vec<String> = files.iter().take(5).map(|(f, h)| format!("{f} (claimed by {h})")).collect();
    let more = if files.len() > 5 { format!(", … and {} more", files.len() - 5) } else { String::new() };
    format!(
        "{T}Warning: your Bash command changed files other agents have claimed: {}{more}. They keep those files until they commit; do not change them again by other means. If the change was not intended, tell the user so they can sort out whose change it is.",
        xs.join(", ")
    )
}

pub fn identity(eff: &str, me: &str, fixed: &str) -> String {
    format!(
        "{T}DENIED: this would run agent-lock as {eff}, but you are {me}. Subagents share the session's environment, so every agent-lock command needs your own id:\n  {fixed}"
    )
}

pub fn ask_killswitch(what: &str) -> String {
    format!("{T}`{what}` turns off file coordination, so other agents' claims are ignored. This needs the user's approval.")
}

pub fn ask_off() -> String {
    format!("{T}`agent-lock off` disables coordination for every agent in this checkout. This needs the user's approval.")
}

pub fn ask_force_release() -> String {
    format!(
        "{T}`agent-lock release --force` takes a file from an agent that may still be working on it and can destroy its uncommitted work. This needs the user's approval."
    )
}

pub fn ask_internals(what: &str) -> String {
    format!(
        "{T}this command modifies agent-lock's lock state ({what}). Lock state is never edited or removed by hand: use `agent-lock release`, and treat an index.lock error as \"wait and retry\". This needs the user's approval."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(s: &str) -> usize {
        s.split_whitespace().count()
    }

    #[test]
    fn contexts_are_short_and_carry_the_id() {
        let m = session_context("a1b2c3d4", None);
        assert!(words(&m) <= 130, "{} words", words(&m));
        assert!(m.contains("a1b2c3d4") && m.contains("agent-lock commit -m") && !m.contains("--as") && !m.contains(" -C "));
        // GNU xargs runs the tool once with no files unless -r: on the whole tree
        assert!(m.contains("`agent-lock mine -z | xargs -0 -r <tool>`"), "{m}");
        let s = subagent_context("a1b2c3d4:9f3e01aa", None);
        assert!(s.contains("agent-lock commit --as a1b2c3d4:9f3e01aa -m"));
        assert!(s.contains("agent-lock mine --as a1b2c3d4:9f3e01aa -z | xargs -0 -r"));
        // the package shims know a subagent only by this variable
        assert!(s.contains("`AGENT_LOCK_AGENT_ID=a1b2c3d4:9f3e01aa npm install x`"), "{s}");
    }

    #[test]
    fn commands_name_the_repo_when_the_shell_is_elsewhere() {
        let top = Path::new("/r/my repo");
        let m = session_context("a1b2c3d4", Some(top));
        for c in ["commit", "push", "sync", "mine"] {
            assert!(m.contains(&format!("agent-lock {c} -C '/r/my repo'")), "{c}: {m}");
        }
        assert!(subagent_context("a1b2c3d4:9f3e01aa", Some(top)).contains("agent-lock commit --as a1b2c3d4:9f3e01aa -C '/r/my repo' -m"));
        assert!(
            inherited("a.ts", "a1b2c3d4", "a1b2c3d4:9f3e01aa", Some(top)).contains("agent-lock commit --as a1b2c3d4:9f3e01aa -C '/r/my repo' -m")
        );
    }

    #[test]
    fn inherited_names_a_sibling_as_a_sibling() {
        let p = inherited("a.ts", "a1b2c3d4", "a1b2c3d4:9f3e01aa", None);
        assert!(p.contains("by a1b2c3d4, your parent agent;"), "{p}");
        let s = inherited("a.ts", "a1b2c3d4:11111111", "a1b2c3d4:9f3e01aa", None);
        assert!(s.contains("by a1b2c3d4:11111111, another subagent of your session;") && !s.contains("parent agent"), "{s}");
    }

    #[test]
    fn deadlock_prints_every_wait() {
        let e = |a: &str, p: &str, b: &str| (a.to_string(), p.to_string(), b.to_string());
        let two = deadlock("a.txt", "aaaaaaaa", &[e("aaaaaaaa", "b.txt", "cccccccc")], "cccccccc", None);
        assert!(two.contains("DEADLOCK: a.txt is claimed by aaaaaaaa, and aaaaaaaa is waiting for b.txt (yours)."), "{two}");
        let three = deadlock("a", "aaaaaaaa", &[e("aaaaaaaa", "b", "bbbbbbbb"), e("bbbbbbbb", "c", "cccccccc")], "cccccccc", Some(Path::new("/r")));
        assert!(three.contains("aaaaaaaa is waiting for b (held by bbbbbbbb), bbbbbbbb is waiting for c (yours)."), "{three}");
        assert!(three.contains("agent-lock commit -C /r -m"), "{three}");
    }

    #[test]
    fn quoting_and_ages() {
        assert_eq!(q("/a/b-c.ts"), "/a/b-c.ts");
        assert_eq!(q("/a b/it's"), r"'/a b/it'\''s'");
        assert_eq!(q(""), "''");
        assert_eq!(ago(5), "5s");
        assert_eq!(ago(125), "2m");
        assert_eq!(ago(7200), "2h");
        assert_eq!(ago(7500), "2h 5m");
    }

    #[test]
    fn blocked_texts() {
        let abs = Path::new("/r/my dir/a.ts");
        let mut b = Blocked {
            abs,
            rel: "my dir/a.ts",
            holder: "bbbbbbbb",
            claimed: 300,
            idle: 120,
            alive: Some(true),
            edits: 3,
            never_ran: false,
            own_sub: None,
            inherited: false,
            me: "aaaaaaaa:11111111",
        };
        let t = blocked(&b);
        assert!(t.contains("claimed by bbbbbbbb (claimed 5m ago, last active 2m ago, holder process alive, 3 uncommitted edits)"));
        assert!(t.contains("agent-lock wait --as aaaaaaaa:11111111 --claim '/r/my dir/a.ts' --timeout 540"));
        b.never_ran = true;
        assert!(blocked(&b).contains("is safe"));
        b.own_sub = Some(true);
        b.inherited = true;
        let t = blocked(&b);
        assert!(t.contains("still running") && t.contains("come back with it"));
        b.own_sub = Some(false);
        assert!(blocked(&b).contains("when your turn ends"));
    }

    #[test]
    fn reminder_groups_repos() {
        let g = vec![(None, vec!["a.ts".to_string()]), (Some(PathBuf::from("/other")), vec!["b.ts".to_string()])];
        let t = commit_reminder(&g, &[("bbbbbbbb".into(), "a.ts".into())]);
        assert!(t.contains("a.ts; b.ts (in /other)"));
        assert!(t.contains("`agent-lock commit -m \"<msg>\"` and `agent-lock commit -C /other -m \"<msg>\"`"));
        assert!(t.contains("bbbbbbbb is waiting for a.ts."));
    }
}
