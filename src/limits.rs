//! Usage / rate limits: an agent that stopped because its plan's limit was
//! hit says so on screen ("5-hour limit reached ∙ resets 2:40am", "You've hit
//! your usage limit … try again in 2 hours", "exhausted your daily quota").
//! The fleet reads the last lines of idle sessions now and then (`fleet.rs`)
//! and `detect` decides; the session then shows "◷ at limit · resets 2:40am".
//!
//! Only strong, specific phrasings count (an agent *talking about* rate
//! limits in code shouldn't), warnings ("approaching", "85% used", "heads
//! up") and notices that the agent carried on (a fallback model, auto mode,
//! extra usage) don't, and only the last lines of the screen are looked at.
//! The phrases are each agent's own, read from their source where it's open
//! (Claude Code's bundle, codex-rs, gemini-cli, qwen-code, opencode, cline,
//! …) and their issue trackers where it isn't (Antigravity, Cursor) —
//! checked 2026-10-25.

/// Is the agent compacting its conversation right now? Its spinner's line says
/// so: `✽ Compacting conversation… (5s · ↓ 468 tokens)` (Claude Code), others'
/// "Compressing chat history…". Only a line that starts with it and trails off:
/// an agent talking about compaction doesn't count, nor does `⎿ Compacted`.
/// herdr has no state for this: it says working, then done, as for any turn.
pub fn compacting(screen: &str) -> bool {
    const WORDS: &[&str] = &["compacting", "auto-compacting", "compressing"];
    screen.lines().rev().take(LINES).any(|l| {
        let t = l
            .trim()
            .trim_start_matches(|c: char| !c.is_alphanumeric())
            .to_lowercase();
        WORDS.iter().any(|w| t.starts_with(w)) && (t.contains('…') || t.contains("..."))
    })
}

/// How many lines from the bottom of the screen are looked at.
pub const LINES: usize = 14;

/// Phrases that mean "stopped: out of quota" on their own.
const STRONG: &[&str] = &[
    // Claude Code, Codex, Gemini CLI, OpenCode, Droid, Qoder, …
    "usage limit reached",
    "usage limit has been reached",
    "claude usage limit reached",
    "5-hour limit reached",
    "weekly limit reached",
    "session limit reached",
    "opus limit reached",
    "daily limit reached",
    "monthly limit reached",
    "credit limit reached",
    "limits are reached",
    "limit has been reached",
    "free usage limit",
    "clinepass limit reached",
    "daily free model limit reached",
    // quota (Gemini, Antigravity, Qwen, Kimi, Amp)
    "exhausted your daily quota",
    "exhausted your quota",
    "quota exceeded",
    "quota has been exhausted",
    "quota exhausted",
    "quota reached",
    "insufficient_quota",
    "exceeded your current quota",
    "exceeded your current token quota",
    "exceeded your usage quota",
    "exceeded your usage limit",
    "no remaining quota",
    // credits, budgets, spend
    "credit balance is too low",
    "credits balance is too low",
    "credit balance is exhausted",
    "credits have been exhausted",
    "insufficient balance",
    "insufficient credits",
    "out of credits",
    "run out of credits",
    "out of usage",
    "out of extra usage",
    "requires usage credits",
    "shared budget",
    "spend cap",
    "spend limit",
    "spending limit",
    "premium requests used up",
    "premium credits exhausted",
    "run out of your",
    "used all your",
    "used up the allowance",
    "reached your usage limit",
    "reached the usage limit",
];

/// Weaker phrases that count only with a "try again" / "resets" next to them.
const WEAK: &[&str] = &[
    "rate limit",
    "rate-limited",
    "too many requests",
    "limit reached",
    "exhausted your capacity",
    "resource has been exhausted",
    "resource_exhausted",
    "usage window limit",
];

/// A line with these is a warning, a cooldown, or the agent carrying on —
/// never "stopped at its limit".
const NEVER: &[&str] = &[
    "approaching",
    "% of your",
    "% used",
    "% remaining",
    "used over",
    "you've used",
    "you’ve used",
    "you're close to",
    "you’re close to",
    "heads up",
    "running low on",
    "hit your fast limit",
    "trying fallback",
    "switching to auto",
    "switched to auto",
    "automatically switch",
    "switched to fallback model",
    "switching to the",
    "continuing on",
    "and continuing",
    "now using extra usage",
    "now using usage credits",
    "now using your usage allocation",
    "high demand",
    "at capacity",
    "trying to reach",
];

/// The next line says the agent carried on (a fallback model, auto mode).
const CARRIED: &[&str] = &[
    "automatically switch",
    "switched to fallback model",
    "switching to auto",
    "trying fallback",
    "continuing on",
];

/// "reached your weekly rate limit", "hit your session limit": the verb,
/// then "limit" within a few words.
fn reached(l: &str) -> bool {
    for verb in [
        "hit your ",
        "hit the ",
        "reached your ",
        "reached the ",
        "reached today's ",
        "reached today’s ",
        "reached its ",
    ] {
        let mut from = 0;
        while let Some(i) = l[from..].find(verb) {
            let at = from + i + verb.len();
            if l[at..]
                .split_whitespace()
                .take(5)
                .any(|w| w.trim_matches(|c: char| !c.is_alphanumeric()) == "limit")
            {
                return true;
            }
            from = at;
        }
    }
    false
}

/// A retry banner (`⚠ Retrying in 12s · attempt 3/10 · …`) is the agent
/// still trying — except Claude Code's own wait at its limit
/// (`Session limit reached · Retrying in 4m (2:40am) · attempt 3/10`).
fn retrying(l: &str) -> bool {
    (l.contains("attempt ") && (l.contains("retrying") || l.contains("retry ")))
        && !l.contains("limit reached")
}

/// If the screen's tail says the agent is stopped at a limit: when it
/// resets, as the agent put it (`2:40am`, `in 2 hours 13 minutes`), or ""
/// when it doesn't say.
pub fn detect(screen: &str) -> Option<String> {
    let lines: Vec<&str> = screen.lines().collect();
    let tail = &lines[lines.len().saturating_sub(LINES)..];
    // newest first: the latest message wins
    for (i, raw) in tail.iter().enumerate().rev() {
        let l = raw.trim().to_lowercase();
        if l.is_empty() || l.len() > 260 || NEVER.iter().any(|p| l.contains(p)) || retrying(&l) {
            continue;
        }
        let strong = reached(&l) || STRONG.iter().any(|p| l.contains(p));
        let weak = !strong
            && WEAK.iter().any(|p| l.contains(p))
            && (l.contains("try again")
                || l.contains("trying again")
                || l.contains("reset")
                || l.contains("retry after")
                || l.contains("retry in"));
        // the agent said so, then carried on (a fallback model, auto mode)
        let carried = tail[i + 1..].iter().take(2).any(|n| {
            let n = n.to_lowercase();
            CARRIED.iter().any(|p| n.contains(p))
        });
        if (strong || weak) && !carried {
            // the reset time may be on this line or the next one
            let next = tail.get(i + 1).map(|x| x.trim()).unwrap_or_default();
            return Some(
                reset_of(raw.trim())
                    .or_else(|| reset_of(next))
                    .unwrap_or_default(),
            );
        }
    }
    None
}

/// `… resets 2:40am (Europe/London)` → `2:40am (Europe/London)`;
/// `try again in 2 hours 13 minutes.` → `in 2 hours 13 minutes`.
fn reset_of(orig: &str) -> Option<String> {
    // find the words case-insensitively, keep the agent's own casing
    // ("Asia/Kolkata") when lowercasing kept the byte offsets
    let lower = orig.to_lowercase();
    let l = if lower.len() == orig.len() {
        orig
    } else {
        lower.as_str()
    };
    let find = |k: &str| lower.find(k);
    const KEYS: &[&str] = &[
        "will reset at ",
        "will reset after ",
        "resets at ",
        "reset at ",
        "resets in ",
        "reset in ",
        "resets on ",
        "reset on ",
        "reset after ",
        "resets ",
        "try again in ",
        "try again at ",
        "try again after ",
        "retry after ",
        "retry in ",
        "retrying in ",
        "refreshes in ",
        "refreshed in ",
        "cycle ends on ",
        "available again at ",
        "available again in ",
    ];
    for k in KEYS {
        if let Some(i) = find(k) {
            let rest = &l[i + k.len()..];
            let mut end = rest.len();
            for stop in [". ", "·", "∙", "|", " - ", " — ", ", or ", " or ", "; "] {
                if let Some(j) = rest.find(stop) {
                    end = end.min(j);
                }
            }
            let mut v = rest[..end]
                .trim()
                .trim_end_matches(['.', '!', ',', ']'])
                .trim();
            if v.chars().count() > 32 {
                v = v
                    .char_indices()
                    .nth(32)
                    .map_or(v, |(j, _)| &v[..j])
                    .trim_end();
            }
            if v.is_empty() {
                continue;
            }
            let prefix = if k.ends_with(" in ") { "in " } else { "" };
            return Some(format!("{prefix}{v}"));
        }
    }
    None
}

/// `◷ limit · resets 2:40am`, `◷ limit · back in 2 hours`, `◷ at limit`.
pub fn label(reset: &str) -> String {
    let r = reset.split(" (").next().unwrap_or(reset).trim();
    let r = short_dur(r);
    let r = r.as_str();
    if r.is_empty() {
        "◷ at limit".into()
    } else if r.starts_with("in ") {
        format!("◷ limit · back {r}")
    } else {
        format!("◷ limit · resets {r}")
    }
}

/// `in 2 hours 13 minutes` → `in 2h 13m`.
fn short_dur(s: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for w in s.split_whitespace() {
        let unit = match w.trim_end_matches(',') {
            "days" | "day" => Some("d"),
            "hours" | "hour" | "hrs" | "hr" => Some("h"),
            "minutes" | "minute" | "mins" | "min" => Some("m"),
            "seconds" | "second" | "secs" | "sec" => Some("s"),
            "and" => Some(""),
            _ => None,
        };
        match (unit, out.last_mut()) {
            (Some(""), _) => {}
            (Some(u), Some(prev)) if prev.chars().all(|c| c.is_ascii_digit()) => prev.push_str(u),
            _ => out.push(w.to_string()),
        }
    }
    out.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_compaction_is_read_from_its_spinner() {
        // as Claude Code 2.1.314 draws it
        assert!(compacting(
            "❯ /compact\n\n✽ Compacting conversation… (5s · ↓ 468 tokens)\n  ⎿  Tip: Double-tap esc\n"
        ));
        assert!(compacting("· Compacting conversation… (1s)"));
        assert!(compacting("⠋ Compressing chat history..."));
        // over, or only talked about
        assert!(!compacting("  ⎿  Compacted (ctrl+o to see full summary)"));
        assert!(!compacting(
            "I'd avoid compacting conversation history here…"
        ));
        assert!(!compacting("Compacting is a good idea"));
        assert!(!compacting(""));
    }

    #[test]
    fn claude_code_limits() {
        // as seen on a real machine
        let real = "❯ what's the status?\n  ⎿  You've hit your session limit · resets 2:40am (Asia/Kolkata)\n     Use your limit reset to reset it now: clau.de/reset\n     /upgrade or /usage-credits to finish what you’re working on.\n\n✻ Baked for 0s · done 1:59 AM\n\n──────\n❯\n──────\n  ⏵⏵ auto mode on (shift+tab to cycle)\n";
        assert_eq!(detect(real).as_deref(), Some("2:40am (Asia/Kolkata)"));
        assert_eq!(
            detect("You've hit your weekly limit · resets Oct 27, 9am").as_deref(),
            Some("Oct 27, 9am")
        );
        assert_eq!(detect("we hit your limits config in the test"), None);
        assert_eq!(
            detect("⏺ Done.\n\n  5-hour limit reached ∙ resets 2:40am\n  /upgrade to increase your usage limit.\n> ").as_deref(),
            Some("2:40am")
        );
        assert_eq!(
            detect("Claude usage limit reached. Your limit will reset at 3pm (Europe/London).")
                .as_deref(),
            Some("3pm (Europe/London)")
        );
        assert_eq!(
            detect("You've hit your limit · resets 9pm (Asia/Kolkata)").as_deref(),
            Some("9pm (Asia/Kolkata)")
        );
    }

    #[test]
    fn codex_gemini_and_others() {
        assert_eq!(
            detect("■ You've hit your usage limit. Upgrade to Pro, or try again in 2 hours 13 minutes.")
                .as_deref(),
            Some("in 2 hours 13 minutes")
        );
        assert_eq!(
            detect("✕ You have exhausted your daily quota on this model.").as_deref(),
            Some("")
        );
        assert_eq!(
            detect("Error: 429 Too Many Requests — rate limit exceeded, try again in 20s")
                .as_deref(),
            Some("in 20s")
        );
        assert_eq!(
            detect("Your credit balance is too low to access the Anthropic API.").as_deref(),
            Some("")
        );
    }

    #[test]
    fn every_harness_in_its_own_words() {
        let at = |s: &str| detect(s);
        // Gemini CLI: the dialog, the error after Stop, the footer
        assert_eq!(
            at("  Usage limit reached for gemini-2.5-pro.\n  Access resets at 3:45 PM PDT.\n  /stats model for usage details\n  ● 1. Switch to gemini-2.5-flash\n    2. Stop").as_deref(),
            Some("3:45 PM PDT")
        );
        assert_eq!(
            at("✕ [API Error: You have exhausted your capacity on this model. Your quota will reset after 19h14m47s.]").as_deref(),
            Some("19h14m47s")
        );
        assert_eq!(
            at("Limit reached, resets in 3h 20m").as_deref(),
            Some("in 3h 20m")
        );
        // Antigravity CLI
        assert_eq!(
            at("⚠ Individual quota reached. Please upgrade your subscription to increase your limits. Resets in 143h57m55s.\nError ID: 1f2e").as_deref(),
            Some("in 143h57m55s")
        );
        assert_eq!(
            at("Your AI credits balance is too low to continue.").as_deref(),
            Some("")
        );
        // Codex (its curly apostrophe)
        assert_eq!(
            at("■ You’ve hit your usage limit. Upgrade to Pro, or try again at 3:45 PM.")
                .as_deref(),
            Some("3:45 PM")
        );
        // Copilot: the reset, not what follows "or"
        assert_eq!(
            at("You've reached your weekly rate limit. Please wait for your limit to reset in 3 hours 12 minutes or switch to auto model to continue.").as_deref(),
            Some("in 3 hours 12 minutes")
        );
        // Qwen Code, Kimi, Droid, OpenCode, Cline, Command Code
        assert_eq!(
            at("Quota exhausted: Your token-plan 1-week quota has been exhausted. The quota will reset at 07-27 09:25:00 UTC.").as_deref(),
            Some("07-27 09:25:00 UTC")
        );
        assert!(at(
            "[provider.auth_error] 403 You've reached your usage limit for this billing cycle."
        )
        .is_some());
        assert!(at("⚠ Credit Limit Reached").is_some());
        assert_eq!(
            at("Workspace usage limit reached. Model use is paused until the limit resets.")
                .as_deref(),
            Some("")
        );
        assert_eq!(
            at("You've reached today's free usage limit for this model.\nTry again in 23h 59m or select another model.").as_deref(),
            Some("in 23h 59m")
        );
        assert_eq!(
            at("You've reached your 5-hour usage limit. Resets in 2h 13m (3:45 PM).").as_deref(),
            Some("in 2h 13m (3:45 PM)")
        );
        // Claude Code waiting at its limit still counts
        assert_eq!(
            at("Session limit reached · Retrying in 4m (2:40am) · attempt 3/10").as_deref(),
            Some("in 4m (2:40am)")
        );
        assert!(at("You're out of usage credits").is_some());
        assert!(at("You've hit your team's shared budget.").is_some());
    }

    #[test]
    fn warnings_cooldowns_and_carrying_on_are_not_a_limit() {
        // Claude Code
        assert_eq!(detect("You've hit your fast limit · resets in 3m"), None);
        assert_eq!(
            detect("You've used 90% of your session limit · resets 2:40am"),
            None
        );
        assert_eq!(
            detect("Fable limit reached · continuing on Opus uses usage credits"),
            None
        );
        // Copilot, Codex, Gemini, Antigravity
        assert_eq!(
            detect("You've used over 75% of your weekly limit. Your limit resets Monday."),
            None
        );
        assert_eq!(
            detect("Heads up, you have less than 25% of your 5h limit left."),
            None
        );
        assert_eq!(detect("85% used (Limit resets in 3h 20m)"), None);
        assert_eq!(detect("85% remaining · Refreshes in 84h 25m"), None);
        assert_eq!(
            detect("Rate limited. Switching to auto mode to continue."),
            None
        );
        // a fallback model said on the next line: it carried on
        assert_eq!(
            detect("⚡ You have reached your daily gemini-2.5-pro quota limit.\n⚡ Automatically switching from gemini-2.5-pro to gemini-2.5-flash for the remainder of this session."),
            None
        );
        // Hermes falls back; OpenCode / Kimi retry banners; Gemini's capacity
        assert_eq!(
            detect(
                "⏳ Your Nous account has hit its rate limit; it resets in 5m. Trying fallback..."
            ),
            None
        );
        assert_eq!(
            detect("⚠ Retrying in 12s · attempt 3 · rate limit exceeded, try again later"),
            None
        );
        assert_eq!(
            detect("We are currently experiencing high demand for gemini-2.5-pro."),
            None
        );
        assert_eq!(
            detect("Quota limit reached; temporarily switching to Auto and continuing..."),
            None
        );
    }

    #[test]
    fn talk_about_limits_is_not_a_limit() {
        assert_eq!(detect("I added a rate limit middleware to the API."), None);
        assert_eq!(detect("⚠ Approaching usage limit · resets 2:40am"), None);
        assert_eq!(detect("handle 429 Too Many Requests in the client"), None);
        // only the last lines count
        let old = format!(
            "5-hour limit reached ∙ resets 2:40am\n{}",
            "line\n".repeat(30)
        );
        assert_eq!(detect(&old), None);
    }
}
