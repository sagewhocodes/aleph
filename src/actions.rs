//! Two-way from the phone. A question's ntfy notification carries Approve and
//! Deny buttons; a tap posts a signed token to a private reply topic (the
//! notification topic plus `-r`), which the hub follows. Before anything is
//! sent to the agent:
//!
//! * the token's signature (HMAC-SHA256 with a key kept in the keystore),
//!   its 30-minute expiry, and that it wasn't used before;
//! * the remote-control permission for that machine and agent
//!   (`remote.allow` minus `remote.deny`) — no permission, no buttons;
//! * that the same question is still waiting (blocked since the same time).
//!
//! Approve answers with the question's own yes option, Deny with its no
//! option (never a blind `1`). Every action lands in the Inbox (`remote`),
//! and a short confirmation goes back to the phone.

use crate::config::{state_dir, Config};
use crate::herdr::{Herdr, Session};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::Duration;

/// How long a button works.
pub const TTL: u64 = 30 * 60;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Act {
    Approve,
    Deny,
}

impl Act {
    fn word(self) -> &'static str {
        match self {
            Act::Approve => "approve",
            Act::Deny => "deny",
        }
    }
    fn parse(s: &str) -> Option<Act> {
        match s {
            "approve" => Some(Act::Approve),
            "deny" => Some(Act::Deny),
            _ => None,
        }
    }
}

/// What a button may do: this session's question (blocked since `since`).
#[derive(Clone, Debug, PartialEq)]
pub struct Grant {
    pub host: String,
    pub pane: String,
    pub since: u64,
    pub act: Act,
    pub exp: u64,
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn sig(key: &[u8], g: &Grant) -> String {
    let mut m = Hmac::<Sha256>::new_from_slice(key).expect("any key length");
    m.update(
        format!(
            "v1|{}|{}|{}|{}|{}",
            g.host,
            g.pane,
            g.since,
            g.act.word(),
            g.exp
        )
        .as_bytes(),
    );
    hex(&m.finalize().into_bytes())
}

/// `v1.<host>.<pane>.<since>.<act>.<exp>.<sig>` (dots never appear in the
/// parts: hosts and panes are escaped).
pub fn encode(key: &[u8], g: &Grant) -> String {
    let esc = |s: &str| s.replace('%', "%25").replace('.', "%2E");
    format!(
        "v1.{}.{}.{}.{}.{}.{}",
        esc(&g.host),
        esc(&g.pane),
        g.since,
        g.act.word(),
        g.exp,
        sig(key, g)
    )
}

/// A token back: checked signature and expiry (`now`), or why not.
pub fn decode(key: &[u8], token: &str, now: u64) -> Result<Grant, String> {
    let un = |s: &str| s.replace("%2E", ".").replace("%25", "%");
    let p: Vec<&str> = token.trim().split('.').collect();
    if p.len() != 7 || p[0] != "v1" {
        return Err("not an Aleph action".into());
    }
    let g = Grant {
        host: un(p[1]),
        pane: un(p[2]),
        since: p[3].parse().map_err(|_| "a bad token")?,
        act: Act::parse(p[4]).ok_or("an unknown action")?,
        exp: p[5].parse().map_err(|_| "a bad token")?,
    };
    // constant-time compare through the Mac
    let mut m = Hmac::<Sha256>::new_from_slice(key).expect("any key length");
    m.update(
        format!(
            "v1|{}|{}|{}|{}|{}",
            g.host,
            g.pane,
            g.since,
            g.act.word(),
            g.exp
        )
        .as_bytes(),
    );
    let want: Vec<u8> = (0..p[6].len() / 2)
        .filter_map(|i| u8::from_str_radix(p[6].get(i * 2..i * 2 + 2)?, 16).ok())
        .collect();
    m.verify_slice(&want)
        .map_err(|_| "a forged or garbled token")?;
    if now > g.exp {
        return Err("it expired (buttons work for 30 minutes)".into());
    }
    Ok(g)
}

/// The signing key: in the keystore, made on first use.
pub fn key() -> Option<Vec<u8>> {
    if let Some(k) = crate::secrets::get("aleph-actions") {
        let b: Vec<u8> = (0..k.len() / 2)
            .filter_map(|i| u8::from_str_radix(k.get(i * 2..i * 2 + 2)?, 16).ok())
            .collect();
        if b.len() >= 32 {
            return Some(b);
        }
    }
    let mut b = [0u8; 32];
    std::io::Read::read_exact(&mut std::fs::File::open("/dev/urandom").ok()?, &mut b).ok()?;
    crate::secrets::set("aleph-actions", &hex(&b)).ok()?;
    Some(b.to_vec())
}

/// `https://ntfy.sh/topic` → (`https://ntfy.sh`, `topic`).
pub fn split_topic(url: &str) -> Option<(String, String)> {
    let u = url.trim_end_matches('/');
    let (base, topic) = u.rsplit_once('/')?;
    (base.contains("://") && !topic.is_empty()).then(|| (base.to_string(), topic.to_string()))
}

/// Signed (approve, deny) tokens for a waiting question — ntfy buttons and
/// Web Push actions carry them. None: it isn't asking, or remote control
/// doesn't cover it.
pub fn tokens(cfg: &Config, s: &Session) -> Option<(String, String)> {
    if !s.blocked() || !crate::permit::allowed(cfg, &s.host, &s.agent) {
        return None;
    }
    let key = key()?;
    let since = s.since.unwrap_or(0);
    let exp = crate::cache::now() + TTL;
    let tok = |act: Act| {
        encode(
            &key,
            &Grant {
                host: s.host.clone(),
                pane: s.pane_id.clone(),
                since,
                act,
                exp,
            },
        )
    };
    Some((tok(Act::Approve), tok(Act::Deny)))
}

/// Buttons on a test alert (`aleph notify test`): signed like real ones and
/// sent back the same way, but their grant names this marker instead of a
/// pane, so a tap is checked, recorded and confirmed — never sent anywhere.
const TEST_PANE: &str = "test:";

/// Approve / Deny for a test alert about `agent` on `host`, sent at `sent`
/// (None: no ntfy, or no signing key).
pub fn test_actions(
    cfg: &Config,
    host: &str,
    agent: &str,
    sent: u64,
    labels: (&str, &str),
) -> Option<serde_json::Value> {
    let (base, topic) = split_topic(&cfg.get("notify.ntfy")?)?;
    let key = key()?;
    let exp = crate::cache::now() + TTL;
    let tok = |act: Act| {
        encode(
            &key,
            &Grant {
                host: host.to_string(),
                pane: format!("{TEST_PANE}{agent}"),
                since: sent,
                act,
                exp,
            },
        )
    };
    // the hub follows the reply topic while these can still come back
    let _ = crate::config::write_atomic(&test_file(), exp.to_string().as_bytes());
    let reply = format!("{base}/{topic}-r");
    let button = |label: &str, tok: String| serde_json::json!({"action": "http", "label": label, "url": reply, "method": "POST", "body": tok, "clear": true});
    Some(serde_json::json!([
        button(labels.0, tok(Act::Approve)),
        button(labels.1, tok(Act::Deny))
    ]))
}

fn test_file() -> std::path::PathBuf {
    state_dir().join("ntfy_test_until")
}

/// Can a test alert's button still come back?
fn test_pending() -> bool {
    std::fs::read_to_string(test_file())
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .is_some_and(|exp| exp > crate::cache::now())
}

/// The ntfy JSON `actions` for a waiting question (None: no permission, or
/// no ntfy).
pub fn ntfy_actions(cfg: &Config, s: &Session) -> Option<serde_json::Value> {
    let (base, topic) = split_topic(&cfg.get("notify.ntfy")?)?;
    let (yes, no) = tokens(cfg, s)?;
    let reply = format!("{base}/{topic}-r");
    let button = |label: &str, tok: String| serde_json::json!({"action": "http", "label": label, "url": reply, "method": "POST", "body": tok, "clear": true});
    Some(serde_json::json!([
        button("Approve", yes),
        button("Deny", no)
    ]))
}

// ------------------------------------------------------------------ single use

fn used_file() -> std::path::PathBuf {
    state_dir().join("actions_used")
}

/// Mark a token used (false: it already was). Old entries age out.
fn use_once(token: &str, now: u64) -> bool {
    let sig = token.rsplit('.').next().unwrap_or(token).to_string();
    let mut v: Vec<(String, u64)> = std::fs::read_to_string(used_file())
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let (s, t) = l.split_once(' ')?;
            Some((s.to_string(), t.parse().ok()?))
        })
        .filter(|(_, t)| now.saturating_sub(*t) < 2 * TTL)
        .collect();
    if v.iter().any(|(s, _)| *s == sig) {
        return false;
    }
    v.push((sig, now));
    let body: String = v.iter().map(|(s, t)| format!("{s} {t}\n")).collect();
    let _ = crate::config::write_private(&used_file(), body.as_bytes());
    true
}

// ------------------------------------------------------------------ acting on it

/// The question's own no option (or esc).
pub fn deny_keys(detection: &str) -> Vec<String> {
    let opts = crate::moshi::options(detection);
    match opts
        .iter()
        .find(|(_, l)| l.trim().to_lowercase().starts_with("no"))
    {
        Some((n, _)) => vec![n.to_string()],
        None => vec!["esc".into()],
    }
}

/// A tapped button: check it all, then answer. Ok: what was done.
pub fn handle(cfg: &Config, sessions: &[Session], token: &str) -> Result<String, String> {
    handle_from(cfg, sessions, token, "the phone")
}

/// [`handle`], saying where it came from in the audit line (`the phone`,
/// `web · pixel`).
pub fn handle_from(
    cfg: &Config,
    sessions: &[Session],
    token: &str,
    who: &str,
) -> Result<String, String> {
    let now = crate::cache::now();
    let key = key().ok_or("no signing key (the keystore isn't reachable)")?;
    let g = decode(&key, token, now)?;
    // a test alert's button: the way back works — record it, touch nothing
    if let Some(agent) = g.pane.strip_prefix(TEST_PANE) {
        if !use_once(token, now) {
            return Err("that button was already used".into());
        }
        let label = crate::agents::label(agent);
        let what = format!(
            "test: {} from {who} reached Aleph {} after the alert",
            if g.act == Act::Approve { "Yes" } else { "No" },
            crate::ui::dur(now.saturating_sub(g.since))
        );
        crate::events::append(crate::events::Event {
            t: now,
            host: g.host.clone(),
            name: format!("test-{agent}"),
            kind: crate::events::Kind::Remote,
            title: what.clone(),
            agent: agent.to_string(),
            source: "test".into(),
            ..Default::default()
        });
        return Ok(format!(
            "Test reply works: {} · {label} on {}",
            what.trim_start_matches("test: "),
            g.host
        ));
    }
    let s = sessions
        .iter()
        .find(|s| s.host == g.host && s.pane_id == g.pane)
        .ok_or("that session has ended")?;
    if !crate::permit::allowed(cfg, &s.host, &s.agent) {
        return Err(format!(
            "remote control isn't allowed for {}:{} (remote.allow)",
            s.host, s.agent
        ));
    }
    if !s.blocked() || s.since.unwrap_or(0) != g.since {
        return Err(format!(
            "{} isn't waiting on that question any more",
            s.display()
        ));
    }
    if !use_once(token, now) {
        return Err("that button was already used".into());
    }
    let t = cfg.target_for(&s.host, crate::herdr::machine_for);
    let h = Herdr::new(&t);
    let q = h
        .read(&s.pane_id, "detection", 0, false)
        .unwrap_or_default();
    if crate::permit::not_from_phone(cfg, &q).is_some() {
        return Err("that question waits for you — answer it at the terminal".into());
    }
    let keys = match g.act {
        Act::Approve => crate::herdr::approve_keys(&q).ok_or("no yes option in that question")?,
        Act::Deny => deny_keys(&q),
    };
    let k: Vec<&str> = keys.iter().map(String::as_str).collect();
    h.send_keys(&s.pane_id, &k)?;
    let what = format!(
        "{} from {who} ({})",
        if g.act == Act::Approve {
            "approved"
        } else {
            "denied"
        },
        keys.join(" ")
    );
    crate::events::append(crate::events::Event {
        t: now,
        host: s.host.clone(),
        name: s.name.clone(),
        kind: crate::events::Kind::Remote,
        title: what.clone(),
        agent: s.agent.clone(),
        source: "aleph".into(),
        ..Default::default()
    });
    crate::log::info("actions", &format!("{}: {what}", s.key()));
    Ok(format!("{} on {}: {what}", s.display(), s.host))
}

/// A one-line note back to the phone (no buttons).
fn confirm(cfg: &Config, text: &str) {
    if let Some(u) = cfg.get("notify.ntfy") {
        let e = crate::events::Event {
            t: crate::cache::now(),
            host: crate::brand::NAME.into(),
            kind: crate::events::Kind::Remote,
            title: text.to_string(),
            ..Default::default()
        };
        let _ = crate::notify::send(&crate::notify::Sink::Ntfy(u), &e);
    }
}

// ------------------------------------------------------------------ the follower

fn cursor_file() -> std::path::PathBuf {
    state_dir().join("ntfy_reply_cursor")
}

/// Follows the reply topic (`curl -sN …/json`), reconnecting; each message
/// body goes to `tx`. Runs until the hub drops the receiver.
pub fn follow(cfg: Config, tx: std::sync::mpsc::Sender<String>) {
    std::thread::spawn(move || {
        let mut wait = 2u64;
        loop {
            let Some((base, topic)) = cfg.get("notify.ntfy").and_then(|u| split_topic(&u)) else {
                std::thread::sleep(Duration::from_secs(60));
                continue;
            };
            let since = std::fs::read_to_string(cursor_file())
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "30m".into());
            // the URL (with its secret topic) goes to curl on stdin
            let conf = format!("url = \"{base}/{topic}-r/json?since={since}\"\n");
            let child = Command::new("curl")
                .args(["-sN", "--max-time", "3600", "-K", "-"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn();
            let Ok(mut c) = child else {
                std::thread::sleep(Duration::from_secs(60));
                continue;
            };
            if let Some(mut si) = c.stdin.take() {
                let _ = std::io::Write::write_all(&mut si, conf.as_bytes());
            }
            let t0 = std::time::Instant::now();
            if let Some(out) = c.stdout.take() {
                for l in BufReader::new(out).lines().map_while(Result::ok) {
                    let Ok(v) = serde_json::from_str::<serde_json::Value>(&l) else {
                        continue;
                    };
                    if v.get("event").and_then(|x| x.as_str()) != Some("message") {
                        continue;
                    }
                    if let Some(id) = v.get("id").and_then(|x| x.as_str()) {
                        let _ = crate::config::write_atomic(&cursor_file(), id.as_bytes());
                    }
                    if let Some(m) = v.get("message").and_then(|x| x.as_str()) {
                        if tx.send(m.to_string()).is_err() {
                            let _ = c.kill();
                            return;
                        }
                    }
                }
            }
            let _ = c.wait();
            // a stream that lasted was fine: reconnect at once; else back off
            wait = if t0.elapsed() > Duration::from_secs(60) {
                2
            } else {
                (wait * 2).min(300)
            };
            std::thread::sleep(Duration::from_secs(wait));
        }
    });
}

/// The hub's side: start following once (when ntfy is set and someone is
/// allowed remote control, or a test alert's buttons are out), then handle
/// what arrives.
#[derive(Default)]
pub struct Replies {
    rx: Option<std::sync::mpsc::Receiver<String>>,
}

impl Replies {
    pub fn tick(&mut self, cfg: &Config, fleet: &crate::fleet::Fleet) {
        if self.rx.is_none() {
            if cfg.get("notify.ntfy").is_none()
                || (cfg.get_or("remote.allow").trim().is_empty() && !test_pending())
            {
                return;
            }
            let (tx, rx) = std::sync::mpsc::channel();
            follow(cfg.clone(), tx);
            self.rx = Some(rx);
        }
        let Some(rx) = &self.rx else { return };
        let msgs: Vec<String> = rx.try_iter().collect();
        if msgs.is_empty() {
            return;
        }
        let sessions: Vec<Session> = fleet.sessions().into_iter().cloned().collect();
        let cfg = cfg.clone();
        std::thread::spawn(move || {
            for m in msgs {
                if !m.starts_with("v1.") {
                    continue;
                }
                let note = match handle(&cfg, &sessions, &m) {
                    Ok(x) => format!("✓ {x}"),
                    Err(e) => {
                        crate::log::warn("actions", &format!("a phone action was refused: {e}"));
                        format!("✕ not done: {e}")
                    }
                };
                confirm(&cfg, &note);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g() -> Grant {
        Grant {
            host: "10.0.0.49".into(),
            pane: "w1:p2".into(),
            since: 1_792_000_000,
            act: Act::Approve,
            exp: 1_792_001_800,
        }
    }

    #[test]
    fn tokens_check_out() {
        let key = [7u8; 32];
        let t = encode(&key, &g());
        assert_eq!(decode(&key, &t, 1_792_000_100).unwrap(), g());
        // expired
        assert!(decode(&key, &t, 1_792_001_801)
            .unwrap_err()
            .contains("expired"));
        // another key, or a changed action, is a forgery
        assert!(decode(&[8u8; 32], &t, 1_792_000_100).is_err());
        let forged = t.replace(".approve.", ".deny.");
        assert!(decode(&key, &forged, 1_792_000_100).is_err());
        assert!(decode(&key, "hello", 0).is_err());
    }

    #[test]
    fn deny_picks_the_no_option() {
        let q = "Do you want to proceed?\n❯ 1. Yes\n  2. Yes, and don't ask again\n  3. No, and tell Claude what to do differently (esc)\n";
        assert_eq!(deny_keys(q), vec!["3"]);
        assert_eq!(deny_keys("Continue? (y/n)"), vec!["esc"]);
    }

    #[test]
    fn topics() {
        assert_eq!(
            split_topic("https://ntfy.sh/abc-123"),
            Some(("https://ntfy.sh".into(), "abc-123".into()))
        );
        assert_eq!(
            split_topic("https://push.example.com/ntfy/t/"),
            Some(("https://push.example.com/ntfy".into(), "t".into()))
        );
        assert!(split_topic("abc").is_none());
    }
}
