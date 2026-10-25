//! Agent ids. A session's main agent is `<sid8>`; a subagent is
//! `<sid8>:<sub8>`. Scripts may use any id without ':'.

/// FNV-1a, for ids that are not plain hex.
fn fnv8(s: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{:08x}", (h >> 32) as u32)
}

/// First 8 chars of a session id when they are alphanumeric, else a hash.
pub fn sid8(session_id: &str) -> String {
    let head: String = session_id.chars().take(8).collect();
    if head.len() == 8 && head.chars().all(|c| c.is_ascii_alphanumeric()) {
        head.to_ascii_lowercase()
    } else {
        fnv8(session_id)
    }
}

/// From Claude Code's `agent_id`: the first 8 of a trailing 16-hex run, else a hash.
pub fn sub8(agent_id: &str) -> String {
    let b = agent_id.as_bytes();
    if b.len() >= 16 && b[b.len() - 16..].iter().all(|c| c.is_ascii_hexdigit()) {
        agent_id[agent_id.len() - 16..agent_id.len() - 8].to_ascii_lowercase()
    } else {
        fnv8(agent_id)
    }
}

pub fn agent_id(session_id: &str, subagent: Option<&str>) -> String {
    match subagent.filter(|s| !s.is_empty()) {
        Some(a) => format!("{}:{}", sid8(session_id), sub8(a)),
        None => sid8(session_id),
    }
}

pub fn family(id: &str) -> &str {
    id.split(':').next().unwrap_or(id)
}

pub fn is_sub(id: &str) -> bool {
    id.contains(':')
}

/// Accepts what a user or agent types after `--as`: `a1b2c3d4`,
/// `a1b2c3d4:9f3e01aa`, a full session id, or v2's `claude-code-sess-…`.
pub fn normalize(input: &str) -> String {
    let s = input.trim();
    let s = s.strip_prefix("claude-code-sess-").unwrap_or(s);
    match s.split_once(':') {
        Some((main, sub)) => format!("{}:{}", main.to_ascii_lowercase(), sub.to_ascii_lowercase()),
        None if s.len() == 36 && s.matches('-').count() == 4 => sid8(s), // a full session id (UUID)
        None if s.len() == 8 && s.chars().all(|c| c.is_ascii_hexdigit()) => s.to_ascii_lowercase(),
        None => s.to_string(), // a script's own id
    }
}

/// The caller of a CLI command: `--as` › `$AGENT_LOCK_AGENT_ID` › `$CLAUDE_CODE_SESSION_ID`.
pub fn caller(flag: Option<&str>) -> Option<String> {
    if let Some(f) = flag.filter(|f| !f.is_empty()) {
        return Some(normalize(f));
    }
    if let Ok(v) = std::env::var("AGENT_LOCK_AGENT_ID") {
        if !v.is_empty() {
            return Some(normalize(&v));
        }
    }
    std::env::var("CLAUDE_CODE_SESSION_ID").ok().filter(|v| !v.is_empty()).map(|v| sid8(&v))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids() {
        assert_eq!(sid8("a1b2c3d4-1111-4111-8111-111111111111"), "a1b2c3d4");
        assert_eq!(sid8("served:abc").len(), 8);
        assert_ne!(sid8("served:abc"), sid8("served:abd"));
        assert_eq!(sub8("a479a55566d243534"), "479a5556");
        assert_eq!(agent_id("a1b2c3d4-x", Some("a479a55566d243534")), "a1b2c3d4:479a5556");
        assert_eq!(family("a1b2c3d4:479a5556"), "a1b2c3d4");
        assert!(is_sub("a:b") && !is_sub("a"));
        assert_eq!(normalize("claude-code-sess-A1B2C3D4"), "a1b2c3d4");
        assert_eq!(normalize("nightly-import"), "nightly-import");
        assert_eq!(normalize("a1b2c3d4-1111-4111-8111-111111111111"), "a1b2c3d4");
        assert_eq!(normalize("A1B2C3D4:FFFF0000"), "a1b2c3d4:ffff0000");
    }
}
