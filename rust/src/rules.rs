//! Claim rules: who may take a file, when a claim is stale, hand-overs.
//! Pure functions over `State`; the caller runs them inside one transaction,
//! so a check and the change it justifies can never be split.

use crate::ids::{family, is_sub};
use crate::state::{Blocked, Claim, Phase, Session, State, Sub, Want};
use crate::sys::{Live, Owner};

/// Ownerless claims (scripts, humans without $CLAUDE_PID) expire after this.
pub const TTL: u64 = 600;
/// A live owner's claim that was never written (rejected or cancelled edit).
pub const UNWRITTEN_GRACE: u64 = 45 * 60;
/// A tool call's marker on a shared file older than this is left over (its
/// PostToolUse never came): an edit takes milliseconds, and co-holders must
/// not wait on a lost hook.
pub const SHARED_IN_FLIGHT: u64 = 120;
/// The hooks' fallback when Stop reports no background tasks: a subagent with
/// no tool call in flight and no hook for this long counts as finished.
pub const SUB_IDLE: u64 = 30 * 60;

pub struct Ctx<'a> {
    pub now: u64,
    pub live: &'a dyn Fn(&Owner) -> Live,
}

impl<'a> Ctx<'a> {
    pub fn real(now: u64) -> Ctx<'static> {
        Ctx { now, live: &crate::sys::liveness }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stale {
    OwnerGone,
    SessionEnded,
    Unwritten,
    Idle,
}

impl Stale {
    pub fn says(self) -> &'static str {
        match self {
            Stale::OwnerGone => "its process has exited",
            Stale::SessionEnded => "its session ended in this same Claude process",
            Stale::Unwritten => "its edit never ran",
            Stale::Idle => "it has been idle",
        }
    }
}

pub fn session_for<'s>(st: &'s State, c: &Claim) -> Option<&'s Session> {
    c.session.as_ref().and_then(|id| st.sessions.get(id)).or_else(|| st.session_of(&c.by))
}

pub fn owner_for(st: &State, c: &Claim) -> Option<Owner> {
    c.owner.or_else(|| session_for(st, c).and_then(|s| s.owner))
}

pub fn last_active(st: &State, c: &Claim) -> u64 {
    let seen = session_for(st, c).map(|s| s.seen).unwrap_or(0);
    c.touched.max(seen)
}

/// What a claim's `pending` marker belongs to: a tool call (its
/// tool_use_id, or anything unknown from an older file), or a process —
/// `agent-lock commit` or a package shim — written "<kind>:<pid>:<start>".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Marker {
    Tool,
    Process(Owner),
}

pub fn marker(pending: &str) -> Marker {
    let mut it = pending.splitn(3, ':');
    match (it.next(), it.next().and_then(|p| p.parse().ok()), it.next().and_then(|s| s.parse().ok())) {
        (Some(kind), Some(pid), Some(start)) if !kind.is_empty() && kind.bytes().all(|b| b.is_ascii_lowercase()) => {
            Marker::Process(Owner { pid, start })
        }
        _ => Marker::Tool,
    }
}

/// The marker a process writes on the claims it works on ("commit", "pkg").
pub fn proc_marker(kind: &str, o: Owner) -> String {
    format!("{kind}:{}:{}", o.pid, o.start)
}

/// A process marker whose process is gone (killed mid-commit or mid-install):
/// left behind, so anyone may clear it.
pub fn marker_dead(pending: &str) -> bool {
    matches!(marker(pending), Marker::Process(o) if crate::sys::liveness(&o) == Live::Dead)
}

/// Why a claim may be taken over, or None while it is live.
pub fn stale(st: &State, c: &Claim, ctx: &Ctx, requester: Option<Owner>) -> Option<Stale> {
    if let Some(Marker::Process(o)) = c.pending.as_deref().map(marker)
        && (ctx.live)(&o) == Live::Alive
    {
        return None; // a commit or an install is working on it right now, however long it takes
    }
    let owner = owner_for(st, c);
    if let Some(o) = owner {
        if (ctx.live)(&o) == Live::Dead {
            return Some(Stale::OwnerGone);
        }
    }
    if let (Some(s), Some(o), Some(r)) = (session_for(st, c), owner, requester) {
        if s.phase == Phase::Ended && o == r && s.id != "" {
            return Some(Stale::SessionEnded);
        }
    }
    let idle = ctx.now.saturating_sub(last_active(st, c));
    match owner {
        Some(_) if c.edits > 0 => None,
        Some(_) if idle >= UNWRITTEN_GRACE => Some(Stale::Unwritten),
        Some(_) => None,
        None if idle >= TTL => Some(Stale::Idle),
        None => None,
    }
}

fn sub<'s>(st: &'s State, id: &str) -> Option<&'s Sub> {
    st.session_of(id).and_then(|s| s.subs.get(id))
}

/// A subagent is finished: SubagentStop (or Stop's background_tasks) said so,
/// or nothing knows it.
pub fn sub_done(st: &State, id: &str) -> bool {
    sub(st, id).is_none_or(|x| x.done)
}

/// A known, unfinished subagent with no tool call in flight and no hook for
/// `limit` seconds. Only a fallback for when Claude Code does not report
/// background tasks: a running agent can read and think for a long time.
pub fn sub_idle(st: &State, id: &str, now: u64, limit: u64) -> bool {
    sub(st, id).is_some_and(|x| !x.done && x.busy == 0 && now.saturating_sub(x.seen) >= limit)
}

/// Stop last saw this subagent running in the background.
fn in_bg(st: &State, id: &str) -> bool {
    st.session_of(id).and_then(|s| s.bg.as_ref()).is_some_and(|bg| bg.iter().any(|b| b == id))
}

/// A subagent of `main`'s family may still be running.
pub fn live_subs(st: &State, main: &str) -> bool {
    let fam = family(main);
    st.sessions.values().any(|s| s.agent == fam && s.subs.values().any(|x| !x.done))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Conflict {
    Own,
    Transfer,
    Join,
    Block,
}

pub fn conflict(st: &State, c: &Claim, requester: &str) -> Conflict {
    if c.by == requester && (c.with.is_empty() || c.pending.is_none()) {
        return Conflict::Own; // alone, its own marker is its own to replace; shared, it may be a co-holder's
    }
    if c.pending.is_some() {
        return Conflict::Block; // a write in flight never changes hands, and a shared file takes one write at a time
    }
    if c.holds(requester) {
        return Conflict::Own;
    }
    if family(&c.by) == family(requester) {
        if !is_sub(&c.by) && is_sub(requester) {
            return Conflict::Transfer; // main hands its file to its own subagent
        }
        if is_sub(&c.by) && sub_done(st, &c.by) && !in_bg(st, &c.by) {
            return Conflict::Transfer; // a finished subagent's files go back to the family
        }
    }
    if st.is_shared(&c.path) {
        return Conflict::Join; // a config file: one more co-holder
    }
    Conflict::Block
}

/// Who is asking for a claim.
#[derive(Clone, Debug, Default)]
pub struct Ask {
    pub agent: String,
    pub session: Option<String>,
    pub owner: Option<Owner>,
    pub pending: Option<String>,
    pub made_by: Option<String>,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Got {
    Created,
    Refreshed,
    Transferred { from: String, inherited: bool },
    TookOver { from: Claim, why: Stale },
    /// Now a co-holder of a shared file; `with` = the other holders.
    Joined { with: Vec<String> },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Held {
    pub claim: Claim,
    pub own_sub: bool,
    pub idle: u64,
    pub alive: Option<bool>,
}

fn fresh(path: &str, a: &Ask, now: u64) -> Claim {
    Claim {
        path: path.to_string(),
        by: a.agent.clone(),
        session: a.session.clone(),
        owner: a.owner,
        at: now,
        touched: now,
        edits: 0,
        pending: a.pending.clone(),
        made_by: a.made_by.clone(),
        inherited: None,
        reason: a.reason.clone(),
        with: Vec::new(),
    }
}

/// Move a claim to another agent (no checks).
fn hand_over(c: &mut Claim, to: &str, session: Option<String>, owner: Option<Owner>, now: u64) {
    let from = std::mem::replace(&mut c.by, to.to_string());
    c.inherited = if c.edits > 0 && is_sub(to) { Some(from) } else { None };
    if session.is_some() {
        c.session = session;
    }
    if owner.is_some() {
        c.owner = owner;
    }
    c.pending = None;
    c.made_by = None;
    c.reason = None;
    c.touched = now;
}

pub fn acquire(st: &mut State, path: &str, a: &Ask, ctx: &Ctx) -> Result<Got, Held> {
    let now = ctx.now;
    let Some(mut cur) = st.claim(path).cloned() else {
        st.put_claim(fresh(path, a, now));
        return Ok(Got::Created);
    };
    if cur.pending.as_deref().is_some_and(|p| marker(p) == Marker::Tool)
        && st.is_shared(path)
        && now.saturating_sub(cur.touched) >= SHARED_IN_FLIGHT
        && let Some(c) = st.claim_mut(path)
    {
        c.pending = None; // a co-holder's lost PostToolUse
        cur.pending = None;
    }
    let mut how = conflict(st, &cur, &a.agent);
    if how == Conflict::Join && cur.with.is_empty() && stale(st, &cur, ctx, a.owner).is_some() {
        how = Conflict::Block; // a dead holder is taken over (with its note), not joined
    }
    match how {
        Conflict::Join => {
            let c = st.claim_mut(path).expect("present");
            let with: Vec<String> = c.holders().map(str::to_string).collect();
            c.with.push(a.agent.clone());
            c.touched = now;
            c.pending = a.pending.clone();
            Ok(Got::Joined { with })
        }
        Conflict::Own => {
            let c = st.claim_mut(path).expect("present");
            c.touched = now;
            if a.pending.is_some() {
                c.pending = a.pending.clone();
            }
            if a.owner.is_some() {
                c.owner = a.owner;
            }
            if a.session.is_some() {
                c.session = a.session.clone();
            }
            if a.reason.is_none() {
                c.reason = None; // a real edit turns a reservation into a claim
            }
            Ok(Got::Refreshed)
        }
        Conflict::Transfer => {
            let c = st.claim_mut(path).expect("present");
            hand_over(c, &a.agent, a.session.clone(), a.owner, now);
            c.pending = a.pending.clone();
            let inherited = c.inherited.is_some();
            Ok(Got::Transferred { from: cur.by, inherited })
        }
        Conflict::Block => {
            if let Some(why) = stale(st, &cur, ctx, a.owner) {
                st.put_claim(fresh(path, a, now));
                return Ok(Got::TookOver { from: cur, why });
            }
            let owner = owner_for(st, &cur);
            Err(Held {
                own_sub: family(&cur.by) == family(&a.agent) && is_sub(&cur.by),
                idle: now.saturating_sub(last_active(st, &cur)),
                alive: owner.map(|o| (ctx.live)(&o) == Live::Alive),
                claim: cur,
            })
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Touch {
    Counted,
    PostHoc,
    NotOwner(String),
}

/// A write succeeded: count it and clear its in-flight marker.
pub fn wrote(st: &mut State, path: &str, a: &Ask, tool_use_id: Option<&str>, now: u64) -> Touch {
    let join = st.claim(path).is_some_and(|c| !c.holds(&a.agent) && c.pending.is_none() && st.is_shared(&c.path));
    match st.claim_mut(path) {
        Some(c) if c.holds(&a.agent) || join => {
            if join {
                c.with.push(a.agent.clone());
            }
            c.edits += 1;
            c.touched = now;
            c.reason = None;
            if tool_use_id.is_none() || c.pending.as_deref() == tool_use_id {
                c.pending = None;
            }
            Touch::Counted
        }
        Some(c) => Touch::NotOwner(c.by.clone()),
        None => {
            let mut c = fresh(path, a, now);
            c.pending = None;
            c.edits = 1;
            st.put_claim(c);
            Touch::PostHoc
        }
    }
}

/// A write failed or was denied: drop a claim this very call created, else
/// only clear its marker. Returns true when the claim was released.
pub fn failed(st: &mut State, path: &str, agent: &str, tool_use_id: Option<&str>) -> bool {
    let Some(c) = st.claim_mut(path) else { return false };
    if !c.holds(agent) {
        return false;
    }
    if c.by == agent && c.with.is_empty() && c.edits == 0 && c.made_by.is_some() && c.made_by.as_deref() == tool_use_id {
        st.take_claim(path);
        return true;
    }
    if tool_use_id.is_some() && c.pending.as_deref() == tool_use_id {
        c.pending = None;
    }
    false
}

/// Hand every claim matching `pred` to `to`. Claims with a write in flight stay.
pub fn transfer_all(st: &mut State, pred: impl Fn(&Claim) -> bool, to: &str, session: Option<String>, owner: Option<Owner>, now: u64) -> usize {
    let mut n = 0;
    for c in st.claims.values_mut() {
        if c.by != to && c.pending.is_none() && pred(c) {
            hand_over(c, to, session.clone(), owner, now);
            n += 1;
        }
    }
    n
}

/// Release a claim. With `agent`, only its owner may; `force` skips that.
/// A dirty claim dropped without a commit is remembered as an orphan.
pub fn release(st: &mut State, path: &str, agent: Option<&str>, force: bool, now: u64) -> Result<Claim, Option<String>> {
    let Some(c) = st.claim(path) else { return Err(None) };
    if !force && agent.is_some_and(|a| !c.holds(a)) {
        return Err(Some(c.by.clone()));
    }
    // A co-holder leaves a shared file: the others keep it (no orphan, no waits dropped).
    if !force && let Some(a) = agent.filter(|_| !c.with.is_empty()) {
        let c = st.claim_mut(path).expect("present");
        let out = c.clone();
        if c.by == a {
            c.by = c.with.remove(0);
            c.pending = None;
        } else {
            c.with.retain(|w| w != a);
        }
        return Ok(out);
    }
    let c = st.take_claim(path).expect("present");
    if c.edits > 0 {
        let k = st.key(&c.path);
        st.orphans.insert(k, crate::state::Orphan { path: c.path.clone(), from: c.by.clone(), at: now });
    }
    drop_wants(st, &c.path);
    Ok(c)
}

/// Remove a claim because its changes were committed (no orphan).
pub fn committed(st: &mut State, path: &str, agent: &str) -> bool {
    match st.claim(path) {
        Some(c) if c.holds(agent) => {
            st.take_claim(path); // a shared file's commit takes every co-holder's changes
            let k = st.key(path);
            st.orphans.remove(&k);
            drop_wants(st, path);
            true
        }
        _ => false,
    }
}

pub fn drop_wants(st: &mut State, path: &str) {
    let k = st.key(path);
    let fold = st.fold;
    for s in st.sessions.values_mut() {
        s.wants.retain(|w| (if fold { w.path.to_lowercase() } else { w.path.clone() }) != k);
    }
}

/// Live claims held by anyone but `me`.
pub fn others<'s>(st: &'s State, me: &str, ctx: &Ctx) -> Vec<&'s Claim> {
    st.claims.values().filter(|c| c.by != me && stale(st, c, ctx, None).is_none()).collect()
}

/// Another agent is at work in this checkout: a live claim by someone else,
/// or another session that is not ended and whose process is alive.
pub fn multi_agent(st: &State, me: &str, ctx: &Ctx) -> bool {
    if !others(st, me, ctx).is_empty() {
        return true;
    }
    st.sessions.values().any(|s| {
        s.agent != family(me)
            && s.phase != Phase::Ended
            && match s.owner {
                Some(o) => (ctx.live)(&o) == Live::Alive,
                None => ctx.now.saturating_sub(s.seen) < TTL,
            }
    })
}

// ── sessions ──────────────────────────────────────────────────────────────

/// Create or refresh a session record; returns it.
pub fn see_session<'s>(st: &'s mut State, id: &str, agent: &str, owner: Option<Owner>, now: u64) -> &'s mut Session {
    let s = st.sessions.entry(id.to_string()).or_insert_with(|| Session {
        id: id.to_string(),
        agent: agent.to_string(),
        started: now,
        ..Session::default()
    });
    s.seen = now;
    if owner.is_some() {
        s.owner = owner;
    }
    s
}

/// Record activity of a subagent (busy +/-).
pub fn see_sub(s: &mut Session, sub: &str, now: u64, busy: i32) {
    let x = s.subs.entry(sub.to_string()).or_insert_with(Sub::default);
    x.seen = now;
    x.busy = (x.busy as i32 + busy).max(0) as u32;
}

/// The waits that lead from `holder` back to `requester`, when `requester`
/// is about to wait for a file `holder` holds: each (agent, path, holder of
/// that path now), following `session.blocked` up to 8 hops. Some = deadlock.
pub fn wait_chain(st: &State, requester: &str, holder: &str) -> Option<Vec<(String, String, String)>> {
    let mut chain = Vec::new();
    let mut cur = holder.to_string();
    for _ in 0..8 {
        let b = st.session_of(&cur)?.blocked.get(&cur)?;
        let next = st.claim(&b.path)?.by.clone();
        chain.push((cur, b.path.clone(), next.clone()));
        if next == requester {
            return Some(chain);
        }
        cur = next;
    }
    None
}

/// A denied requester waits for `holder`'s file: recorded on both sides.
/// Returns the deadlock chain (`wait_chain`), if any.
pub fn record_wait(st: &mut State, path: &str, requester: &str, holder: &str, now: u64) -> Option<Vec<(String, String, String)>> {
    let deadlock = wait_chain(st, requester, holder);
    if let Some(s) = st.session_of_mut(requester) {
        s.blocked.insert(requester.to_string(), Blocked { path: path.to_string(), holder: holder.to_string(), at: now });
    }
    if let Some(s) = st.session_of_mut(holder) {
        if !s.wants.iter().any(|w| w.waiter == requester && w.path == path) {
            s.wants.push(Want { waiter: requester.to_string(), path: path.to_string(), at: now, told: false });
        }
    }
    deadlock
}

/// Waiters on files `agent` still holds that it has not been told about yet;
/// marks them told.
pub fn take_notices(st: &mut State, agent: &str) -> Vec<(String, String)> {
    let held: Vec<String> = st.claims.values().filter(|c| c.by == agent).map(|c| c.path.clone()).collect();
    let fold = st.fold;
    let same = |a: &str, b: &str| if fold { a.eq_ignore_ascii_case(b) } else { a == b };
    let mut out = Vec::new();
    if let Some(s) = st.session_of_mut(agent) {
        for w in s.wants.iter_mut() {
            if !w.told && held.iter().any(|p| same(p, &w.path)) {
                w.told = true;
                out.push((w.waiter.clone(), w.path.clone()));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Claim;

    const A: &str = "aaaaaaaa";
    const B: &str = "bbbbbbbb";
    const ASUB: &str = "aaaaaaaa:11111111";
    const ASUB2: &str = "aaaaaaaa:22222222";
    const ALIVE: Owner = Owner { pid: 10, start: 1 };
    const DEAD: Owner = Owner { pid: 11, start: 1 };

    fn live(o: &Owner) -> Live {
        if o.pid == 11 { Live::Dead } else { Live::Alive }
    }

    fn ctx(now: u64) -> Ctx<'static> {
        Ctx { now, live: &live }
    }

    fn ask(agent: &str, owner: Option<Owner>) -> Ask {
        Ask { agent: agent.into(), owner, ..Ask::default() }
    }

    fn with_sessions() -> State {
        let mut st = State::default();
        see_session(&mut st, "aaaaaaaa-s", A, Some(ALIVE), 1000);
        see_session(&mut st, "bbbbbbbb-s", B, Some(ALIVE), 1000);
        st
    }

    #[test]
    fn create_refresh_block() {
        let mut st = with_sessions();
        assert_eq!(acquire(&mut st, "a.ts", &ask(A, Some(ALIVE)), &ctx(1000)), Ok(Got::Created));
        assert_eq!(acquire(&mut st, "a.ts", &ask(A, Some(ALIVE)), &ctx(1001)), Ok(Got::Refreshed));
        let held = acquire(&mut st, "a.ts", &ask(B, Some(ALIVE)), &ctx(1002)).unwrap_err();
        assert_eq!(held.claim.by, A);
        assert!(!held.own_sub);
    }

    #[test]
    fn family_rules() {
        let mut st = with_sessions();
        acquire(&mut st, "a.ts", &ask(A, Some(ALIVE)), &ctx(1000)).unwrap();
        wrote(&mut st, "a.ts", &ask(A, Some(ALIVE)), None, 1000);
        // main → own subagent: handed over, uncommitted edits marked inherited
        assert_eq!(
            acquire(&mut st, "a.ts", &ask(ASUB, Some(ALIVE)), &ctx(1001)),
            Ok(Got::Transferred { from: A.into(), inherited: true })
        );
        assert_eq!(st.claim("a.ts").unwrap().inherited.as_deref(), Some(A));
        // a running sibling or the main agent is blocked by a running subagent
        st.sessions.get_mut("aaaaaaaa-s").unwrap().subs.insert(ASUB.into(), Sub { seen: 1001, done: false, busy: 1 });
        assert!(acquire(&mut st, "a.ts", &ask(ASUB2, Some(ALIVE)), &ctx(1002)).unwrap_err().own_sub);
        assert!(acquire(&mut st, "a.ts", &ask(A, Some(ALIVE)), &ctx(1002)).is_err());
        // once it is done, the family gets it back
        st.sessions.get_mut("aaaaaaaa-s").unwrap().subs.get_mut(ASUB).unwrap().done = true;
        assert!(matches!(acquire(&mut st, "a.ts", &ask(A, Some(ALIVE)), &ctx(1003)), Ok(Got::Transferred { inherited: false, .. })));
    }

    #[test]
    fn an_idle_running_subagent_keeps_its_files() {
        // state/F2, hooks/H1: a subagent that only reads or thinks fires no hook; it is still running
        let mut st = with_sessions();
        acquire(&mut st, "a.ts", &ask(ASUB, Some(ALIVE)), &ctx(1000)).unwrap();
        wrote(&mut st, "a.ts", &ask(ASUB, Some(ALIVE)), None, 1000);
        fn subs(st: &mut State) -> &mut std::collections::BTreeMap<String, Sub> {
            &mut st.sessions.get_mut("aaaaaaaa-s").unwrap().subs
        }
        subs(&mut st).insert(ASUB.into(), Sub { seen: 1000, done: false, busy: 0 });
        let late = 1000 + 24 * 3600;
        assert!(acquire(&mut st, "a.ts", &ask(ASUB2, Some(ALIVE)), &ctx(late)).unwrap_err().own_sub, "sibling");
        assert!(acquire(&mut st, "a.ts", &ask(A, Some(ALIVE)), &ctx(late)).is_err(), "main");
        assert!(sub_idle(&st, ASUB, late, SUB_IDLE) && !sub_done(&st, ASUB), "idle is only the hooks' fallback signal");
        assert!(!sub_idle(&st, ASUB, 1000 + SUB_IDLE - 1, SUB_IDLE));
        // finished but Stop saw it running in the background: still its own
        subs(&mut st).get_mut(ASUB).unwrap().done = true;
        st.sessions.get_mut("aaaaaaaa-s").unwrap().bg = Some(vec![ASUB.into()]);
        assert!(acquire(&mut st, "a.ts", &ask(A, Some(ALIVE)), &ctx(late)).is_err(), "in bg");
        st.sessions.get_mut("aaaaaaaa-s").unwrap().bg = None;
        assert!(matches!(acquire(&mut st, "a.ts", &ask(ASUB2, Some(ALIVE)), &ctx(late)), Ok(Got::Transferred { inherited: true, .. })));
        // a subagent nothing knows (no record) counts as finished
        st.put_claim(Claim { path: "u.ts".into(), by: "aaaaaaaa:99999999".into(), edits: 1, ..Claim::default() });
        assert!(sub_done(&st, "aaaaaaaa:99999999") && !sub_idle(&st, "aaaaaaaa:99999999", late, SUB_IDLE));
        assert!(acquire(&mut st, "u.ts", &ask(A, Some(ALIVE)), &ctx(late)).is_ok());
    }

    #[test]
    fn live_subagents_of_a_family() {
        let mut st = with_sessions();
        assert!(!live_subs(&st, A));
        st.sessions.get_mut("aaaaaaaa-s").unwrap().subs.insert(ASUB.into(), Sub { seen: 1000, done: false, busy: 0 });
        assert!(live_subs(&st, A) && live_subs(&st, ASUB2) && !live_subs(&st, B));
        st.sessions.get_mut("aaaaaaaa-s").unwrap().subs.get_mut(ASUB).unwrap().done = true;
        assert!(!live_subs(&st, A));
    }

    #[test]
    fn markers_describe_themselves() {
        let me = crate::sys::self_owner().unwrap();
        let gone = Owner { pid: me.pid, start: me.start + 1 };
        assert_eq!(marker("toolu_01ABC"), Marker::Tool);
        assert_eq!(marker("pkg"), Marker::Tool, "v3.0's bare markers read as tool calls");
        assert_eq!(marker("commit"), Marker::Tool);
        assert_eq!(marker(&proc_marker("commit", me)), Marker::Process(me));
        assert_eq!(marker("pkg:12:34"), Marker::Process(Owner { pid: 12, start: 34 }));
        assert_eq!(marker("x:y:z"), Marker::Tool);
        assert!(!marker_dead(&proc_marker("commit", me)) && marker_dead(&proc_marker("pkg", gone)));
        assert!(!marker_dead("toolu_01ABC"), "a tool marker is never 'dead': hooks decide when its call ended");
    }

    #[test]
    fn pending_never_changes_hands() {
        let mut st = with_sessions();
        let mut a = ask(A, Some(ALIVE));
        a.pending = Some("tu1".into());
        acquire(&mut st, "a.ts", &a, &ctx(1000)).unwrap();
        assert!(acquire(&mut st, "a.ts", &ask(ASUB, Some(ALIVE)), &ctx(1001)).is_err());
    }

    #[test]
    fn staleness() {
        let mut st = with_sessions();
        // dead owner → taken over at once
        st.put_claim(Claim { path: "d.ts".into(), by: B.into(), owner: Some(DEAD), edits: 3, touched: 1000, ..Claim::default() });
        assert!(matches!(acquire(&mut st, "d.ts", &ask(A, Some(ALIVE)), &ctx(1001)), Ok(Got::TookOver { why: Stale::OwnerGone, .. })));
        // alive owner with edits → never stale, even after a long idle
        st.put_claim(Claim { path: "e.ts".into(), by: B.into(), owner: Some(ALIVE), edits: 1, touched: 0, ..Claim::default() });
        st.sessions.get_mut("bbbbbbbb-s").unwrap().seen = 0;
        assert!(acquire(&mut st, "e.ts", &ask(A, Some(ALIVE)), &ctx(1_000_000)).is_err());
        // alive owner, never written → stale after the grace
        st.put_claim(Claim { path: "u.ts".into(), by: B.into(), owner: Some(ALIVE), pending: Some("t".into()), touched: 0, ..Claim::default() });
        assert!(acquire(&mut st, "u.ts", &ask(A, Some(ALIVE)), &ctx(UNWRITTEN_GRACE - 1)).is_err());
        assert!(matches!(acquire(&mut st, "u.ts", &ask(A, Some(ALIVE)), &ctx(UNWRITTEN_GRACE + 1)), Ok(Got::TookOver { why: Stale::Unwritten, .. })));
        // no owner → TTL
        st.put_claim(Claim { path: "s.ts".into(), by: "script".into(), touched: 0, ..Claim::default() });
        assert!(acquire(&mut st, "s.ts", &ask(A, None), &ctx(TTL - 1)).is_err());
        assert!(acquire(&mut st, "s.ts", &ask(A, None), &ctx(TTL)).is_ok());
    }

    #[test]
    fn a_live_process_marker_is_never_stale() {
        let mut st = with_sessions();
        const C: &str = "cccccccc"; // no session record: ownerless
        // an ownerless claim idle past the TTL, its install (or commit) still running
        st.put_claim(Claim { path: "package.json".into(), by: C.into(), pending: Some(proc_marker("pkg", ALIVE)), touched: 0, ..Claim::default() });
        assert!(acquire(&mut st, "package.json", &ask(A, Some(ALIVE)), &ctx(1_000_000)).is_err());
        // an owned claim never written, past the grace, mid-commit
        st.put_claim(Claim { path: "c.ts".into(), by: B.into(), owner: Some(ALIVE), pending: Some(proc_marker("commit", ALIVE)), ..Claim::default() });
        assert!(acquire(&mut st, "c.ts", &ask(A, Some(ALIVE)), &ctx(UNWRITTEN_GRACE + 1)).is_err());
        // its process died: the usual rules again
        st.put_claim(Claim { path: "package.json".into(), by: C.into(), pending: Some(proc_marker("pkg", DEAD)), touched: 0, ..Claim::default() });
        assert!(matches!(acquire(&mut st, "package.json", &ask(A, Some(ALIVE)), &ctx(TTL + 1)), Ok(Got::TookOver { why: Stale::Idle, .. })));
        // any marker blocks a hand-over within the family
        st.put_claim(Claim { path: "f.ts".into(), by: A.into(), pending: Some(proc_marker("commit", ALIVE)), ..Claim::default() });
        assert!(acquire(&mut st, "f.ts", &ask(ASUB, Some(ALIVE)), &ctx(1000)).is_err());
    }

    #[test]
    fn ended_session_in_same_process_hands_over() {
        let mut st = with_sessions();
        acquire(&mut st, "a.ts", &Ask { agent: B.into(), session: Some("bbbbbbbb-s".into()), owner: Some(ALIVE), ..Ask::default() }, &ctx(1000)).unwrap();
        wrote(&mut st, "a.ts", &ask(B, Some(ALIVE)), None, 1000);
        st.sessions.get_mut("bbbbbbbb-s").unwrap().phase = Phase::Ended;
        let other = Owner { pid: 12, start: 9 };
        assert!(acquire(&mut st, "a.ts", &ask(A, Some(other)), &ctx(1001)).is_err(), "different process");
        assert!(matches!(acquire(&mut st, "a.ts", &ask(A, Some(ALIVE)), &ctx(1001)), Ok(Got::TookOver { why: Stale::SessionEnded, .. })));
    }

    #[test]
    fn write_and_failure_bookkeeping() {
        let mut st = with_sessions();
        let mut a = ask(A, Some(ALIVE));
        a.pending = Some("t1".into());
        a.made_by = Some("t1".into());
        acquire(&mut st, "a.ts", &a, &ctx(1000)).unwrap();
        assert!(failed(&mut st, "a.ts", A, Some("t1")), "created by this call, never written");
        assert!(st.claim("a.ts").is_none());
        // a claim from `wait --claim` survives a failed first edit
        st.put_claim(Claim { path: "w.ts".into(), by: A.into(), reason: Some("wait".into()), ..Claim::default() });
        let mut b = ask(A, Some(ALIVE));
        b.pending = Some("t2".into());
        b.made_by = Some("t2".into());
        acquire(&mut st, "w.ts", &b, &ctx(1000)).unwrap();
        assert!(!failed(&mut st, "w.ts", A, Some("t2")));
        assert_eq!(st.claim("w.ts").unwrap().pending, None);
        // writes count; a write on someone else's claim is reported
        assert_eq!(wrote(&mut st, "w.ts", &ask(A, Some(ALIVE)), Some("t3"), 1001), Touch::Counted);
        assert_eq!(st.claim("w.ts").unwrap().edits, 1);
        assert_eq!(wrote(&mut st, "w.ts", &ask(B, Some(ALIVE)), None, 1001), Touch::NotOwner(A.into()));
        assert_eq!(wrote(&mut st, "new.ts", &ask(B, Some(ALIVE)), None, 1001), Touch::PostHoc);
    }

    #[test]
    fn release_orphans_and_commits() {
        let mut st = with_sessions();
        st.put_claim(Claim { path: "a.ts".into(), by: A.into(), edits: 2, ..Claim::default() });
        assert_eq!(release(&mut st, "a.ts", Some(B), false, 1), Err(Some(A.into())));
        release(&mut st, "a.ts", Some(B), true, 1).unwrap();
        assert!(st.orphans.contains_key("a.ts"), "dirty work dropped without a commit is remembered");
        st.put_claim(Claim { path: "a.ts".into(), by: B.into(), edits: 1, ..Claim::default() });
        assert!(committed(&mut st, "a.ts", B));
        assert!(st.orphans.is_empty());
    }

    #[test]
    fn multi_agent_and_waits() {
        let mut st = State::default();
        see_session(&mut st, "aaaaaaaa-s", A, Some(ALIVE), 1000);
        assert!(!multi_agent(&st, A, &ctx(1000)));
        see_session(&mut st, "bbbbbbbb-s", B, Some(DEAD), 1000);
        assert!(!multi_agent(&st, A, &ctx(1000)), "dead session");
        see_session(&mut st, "bbbbbbbb-s", B, Some(ALIVE), 1000);
        assert!(multi_agent(&st, A, &ctx(1000)));
        st.put_claim(Claim { path: "a.ts".into(), by: A.into(), edits: 1, owner: Some(ALIVE), ..Claim::default() });
        st.put_claim(Claim { path: "b.ts".into(), by: B.into(), edits: 1, owner: Some(ALIVE), ..Claim::default() });
        assert!(record_wait(&mut st, "b.ts", A, B, 1000).is_none());
        assert_eq!(record_wait(&mut st, "a.ts", B, A, 1001), Some(vec![(A.into(), "b.ts".into(), B.into())]), "B waits for A's file while A waits for B's");
        assert_eq!(take_notices(&mut st, B), vec![(A.to_string(), "b.ts".to_string())]);
        assert!(take_notices(&mut st, B).is_empty(), "told once");
    }

    #[test]
    fn deadlocks_are_found_over_several_hops() {
        // simplicity/S9: A waits for b (B's), B waits for c (C's); C asking for a closes the loop
        const C: &str = "cccccccc";
        let mut st = with_sessions();
        see_session(&mut st, "cccccccc-s", C, Some(ALIVE), 1000);
        for (p, by) in [("a", A), ("b", B), ("c", C)] {
            st.put_claim(Claim { path: p.into(), by: by.into(), edits: 1, owner: Some(ALIVE), ..Claim::default() });
        }
        assert!(record_wait(&mut st, "b", A, B, 1000).is_none());
        assert!(record_wait(&mut st, "c", B, C, 1000).is_none());
        let chain = vec![(A.to_string(), "b".to_string(), B.to_string()), (B.to_string(), "c".to_string(), C.to_string())];
        assert_eq!(wait_chain(&st, C, A), Some(chain.clone()));
        assert_eq!(record_wait(&mut st, "a", C, A, 1001), Some(chain));
        // the edge follows the file's holder now, not the one recorded
        st.claim_mut("c").unwrap().by = "dddddddd".into();
        assert_eq!(wait_chain(&st, C, A), None);
        // a loop that does not reach the requester ends
        st.claim_mut("c").unwrap().by = A.into();
        assert_eq!(wait_chain(&st, C, A), None);
    }

    #[test]
    fn shared_files_take_co_holders_one_write_at_a_time() {
        let mut st = with_sessions();
        let tool = |agent: &str, t: &str| Ask { agent: agent.into(), owner: Some(ALIVE), pending: Some(t.into()), ..Ask::default() };
        assert_eq!(acquire(&mut st, "package.json", &tool(A, "t1"), &ctx(1000)), Ok(Got::Created));
        // B arrives while A's write is in flight: waits for it (milliseconds)
        assert!(acquire(&mut st, "package.json", &tool(B, "t2"), &ctx(1000)).is_err());
        assert_eq!(wrote(&mut st, "package.json", &ask(A, Some(ALIVE)), Some("t1"), 1000), Touch::Counted);
        assert_eq!(acquire(&mut st, "package.json", &tool(B, "t2"), &ctx(1001)), Ok(Got::Joined { with: vec![A.into()] }));
        // now A waits for B's write, even though A is the first holder
        assert!(acquire(&mut st, "package.json", &tool(A, "t3"), &ctx(1001)).is_err());
        assert_eq!(wrote(&mut st, "package.json", &ask(B, Some(ALIVE)), Some("t2"), 1001), Touch::Counted);
        assert_eq!(acquire(&mut st, "package.json", &tool(A, "t3"), &ctx(1002)), Ok(Got::Refreshed));
        // a co-holder's write whose PostToolUse never came stops blocking after a while
        assert!(acquire(&mut st, "package.json", &tool(B, "t4"), &ctx(1002 + SHARED_IN_FLIGHT - 1)).is_err());
        assert_eq!(acquire(&mut st, "package.json", &tool(B, "t4"), &ctx(1002 + SHARED_IN_FLIGHT)), Ok(Got::Refreshed));
        // a file that is not shared still blocks
        acquire(&mut st, "src/a.ts", &ask(A, Some(ALIVE)), &ctx(1000)).unwrap();
        assert!(acquire(&mut st, "src/a.ts", &ask(B, Some(ALIVE)), &ctx(1000)).is_err());
    }

    #[test]
    fn shared_files_release_and_commit_for_everyone() {
        let mut st = with_sessions();
        const C: &str = "cccccccc";
        for x in [A, B, C] {
            acquire(&mut st, "tsconfig.json", &ask(x, Some(ALIVE)), &ctx(1000)).unwrap();
            wrote(&mut st, "tsconfig.json", &ask(x, Some(ALIVE)), None, 1000);
        }
        assert_eq!(st.claim("tsconfig.json").unwrap().holders().collect::<Vec<_>>(), [A, B, C]);
        // a co-holder leaves; the first holder leaves and the next one becomes it; no orphan
        release(&mut st, "tsconfig.json", Some(B), false, 1).unwrap();
        release(&mut st, "tsconfig.json", Some(A), false, 1).unwrap();
        let c = st.claim("tsconfig.json").unwrap();
        assert_eq!((c.by.as_str(), c.with.len()), (C, 0));
        assert!(st.orphans.is_empty());
        // one commit by any holder takes everyone's changes
        acquire(&mut st, "tsconfig.json", &ask(A, Some(ALIVE)), &ctx(1001)).unwrap();
        assert!(!committed(&mut st, "tsconfig.json", B), "not a holder");
        assert!(committed(&mut st, "tsconfig.json", A));
        assert!(st.claim("tsconfig.json").is_none());
        // a dead holder is taken over, not joined
        st.put_claim(Claim { path: "go.mod".into(), by: B.into(), owner: Some(DEAD), edits: 1, ..Claim::default() });
        assert!(matches!(acquire(&mut st, "go.mod", &ask(A, Some(ALIVE)), &ctx(1002)), Ok(Got::TookOver { why: Stale::OwnerGone, .. })));
    }

    #[test]
    fn transfer_all_skips_in_flight_writes() {
        let mut st = with_sessions();
        st.put_claim(Claim { path: "x".into(), by: ASUB.into(), edits: 1, ..Claim::default() });
        st.put_claim(Claim { path: "y".into(), by: ASUB.into(), pending: Some("t".into()), ..Claim::default() });
        assert_eq!(transfer_all(&mut st, |c| c.by == ASUB, A, None, None, 5), 1);
        assert_eq!(st.claim("x").unwrap().by, A);
        assert_eq!(st.claim("x").unwrap().inherited, None);
        assert_eq!(st.claim("y").unwrap().by, ASUB);
    }
}
