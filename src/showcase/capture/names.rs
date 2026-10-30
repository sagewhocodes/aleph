//! The keycast's names (`showcase.keycast = names`): what a key did, in a
//! couple of words beside its keycap — `j  next session`, `a  attach`,
//! `?  help`. The dashboard says what the key does where it was pressed
//! (its key map's action, its global keys, the footer's hints); here that
//! becomes a keycap's label. Typed text never gets this far.

/// Short names for the key map's actions (`ACTS`' config names): its help
/// says it all, but too long for a keycap.
const SHORT: &[(&str, &str)] = &[
    ("up", "previous session"),
    ("down", "next session"),
    ("prev_session", "previous session"),
    ("next_session", "next session"),
    ("prev_machine", "previous machine"),
    ("next_machine", "next machine"),
    ("mark", "mark"),
    ("filter", "filter"),
    ("refresh", "refresh"),
    ("open", "open in Focus"),
    ("attach", "attach"),
    ("direct", "into the agent"),
    ("open_tab", "new terminal tab"),
    ("new", "new session"),
    ("presets", "presets"),
    ("duplicate", "duplicate"),
    ("shell", "a shell"),
    ("mute", "mute"),
    ("note", "a note"),
    ("browse", "browse folders"),
    ("wall", "tile side by side"),
    ("send", "send a prompt"),
    ("approve", "approve"),
    ("keys", "send keys"),
    ("broadcast", "broadcast"),
    ("rename", "rename"),
    ("close", "close"),
    ("copy", "copy output"),
    ("tile", "into one herdr tab"),
    ("hub", "the herdr hub"),
    ("collapse", "fold"),
    ("expand", "unfold"),
    ("queue", "queue a task"),
    ("heal", "heal herdr"),
    ("stats_mode", "machine panel"),
    ("add_machine", "add a machine"),
    ("remove_machine", "remove machine"),
    ("edit_machine", "edit machine"),
    ("test_machine", "test login"),
    ("key_setup", "key setup"),
    ("default_machine", "make default"),
    ("pause_machine", "pause polling"),
    ("machine_color", "next colour"),
    ("herdr_setup", "set up herdr"),
    ("wake", "wake it"),
];

/// The longest label a keycap gets.
const MAX: usize = 22;

/// What a keycap says beside its key: nothing with `showcase.keycast =
/// keys`, else the dashboard's answer for what it did (asked only then).
pub fn of(
    cfg: &crate::config::Config,
    does: impl FnOnce() -> Option<(&'static str, String)>,
) -> String {
    if cfg.get_or("showcase.keycast") == "keys" {
        return String::new();
    }
    does()
        .map(|(act, words)| label(act, &words))
        .unwrap_or_default()
}

/// What the keycap says beside the key: the action's short name, else the
/// words cut to their first clause.
pub fn label(act: &str, words: &str) -> String {
    if let Some((_, s)) = SHORT.iter().find(|(a, _)| *a == act && !act.is_empty()) {
        return s.to_string();
    }
    trim(words)
}

/// Words cut at their first clause (`:`, `(`, `,`, ` — `, ` · `, `;`, `…`)
/// and to a keycap's length.
pub fn trim(w: &str) -> String {
    let mut s = w.trim();
    for cut in [": ", " (", ", ", " — ", " · ", "; ", "…"] {
        if let Some(i) = s.find(cut) {
            if i > 0 {
                s = &s[..i];
            }
        }
    }
    let s = s.trim();
    if s.chars().count() <= MAX {
        return s.to_string();
    }
    // whole words, then the ellipsis
    let mut out = String::new();
    for word in s.split(' ') {
        if out.chars().count() + word.chars().count() + 1 > MAX - 1 {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    if out.is_empty() {
        out = s.chars().take(MAX - 1).collect();
    }
    out + "…"
}

/// Does a footer hint's key (`↑↓`, `[ ]`, `f o c m g p n`, `hjkl`, `⇥ ←→`,
/// `ctrl+k`) name the key pressed (`down`, `]`, `o`, `j`, `tab`, `ctrl+k`)?
pub fn hint_has(hint: &str, k: &str) -> bool {
    let glyph = match k {
        "up" => "↑",
        "down" => "↓",
        "left" => "←",
        "right" => "→",
        "tab" => "⇥",
        "enter" => "↵",
        "space" => "␣",
        "backspace" => "⌫",
        _ => "",
    };
    hint.split(' ').filter(|t| !t.is_empty()).any(|t| {
        if t == k || (!glyph.is_empty() && t.contains(glyph)) {
            return true;
        }
        let named = NAMED.contains(&t)
            || (t.len() > 1 && t.starts_with('f') && t[1..].bytes().all(|b| b.is_ascii_digit()));
        if named || t.contains('+') || t.chars().count() < 2 || k.chars().count() != 1 {
            return false;
        }
        // a cluster of one-character keys (`hjkl`)
        t.chars().any(|c| c.to_string() == k)
    })
}

/// Keys a hint spells out by name (never a cluster of letters).
const NAMED: &[&str] = &[
    "enter",
    "esc",
    "space",
    "tab",
    "backspace",
    "delete",
    "home",
    "end",
    "pageup",
    "pagedown",
    "up",
    "down",
    "left",
    "right",
    "any",
    "key",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_short_and_say_what_happened() {
        assert_eq!(label("down", "move down"), "next session");
        assert_eq!(
            label("attach", "attach in herdr, in this terminal"),
            "attach"
        );
        assert_eq!(label("", "help"), "help");
        // a long hint: its first clause
        assert_eq!(
            label("", "open in Focus: fullscreen here, with messages"),
            "open in Focus"
        );
        assert_eq!(
            label("", "start a preset… (sessions marked)"),
            "start a preset"
        );
        let long = trim("the next sub-view among many many more words");
        assert!(long.ends_with('…') && long.chars().count() <= MAX, "{long}");
        // every action the key map has, a short name (the key map's own
        // names: tui::ACTS)
        for (_, s) in SHORT {
            assert!(s.chars().count() <= MAX, "{s}");
        }
    }

    #[test]
    fn hints_name_their_keys() {
        assert!(hint_has("↑↓", "down"));
        assert!(hint_has("↑↓←→", "left"));
        assert!(hint_has("[ ]", "]"));
        assert!(hint_has("f o c m g p n", "o"));
        assert!(hint_has("hjkl", "j"));
        assert!(hint_has("⇥ ←→", "tab"));
        assert!(hint_has("ctrl+k", "ctrl+k"));
        assert!(hint_has("enter", "enter"));
        assert!(!hint_has("ctrl+k", "k"));
        assert!(!hint_has("enter", "e"));
        assert!(!hint_has("space", "a"));
        assert!(!hint_has("s S", "x"));
        assert!(hint_has("s S", "S"));
    }
}
