//! Live terminal tiles: `herdr terminal session observe <pane>` run over ssh
//! streams newline-delimited JSON frames (base64 ANSI bytes); we feed them
//! into a vt100 emulator and paint its screen into a ratatui rect. Observers
//! are read-only and never resize the pane, so tiles don't disturb anyone
//! attached to the session.
//!
//! Fitted tiles (`wall_fit`, the default) use `terminal session control
//! --cols --rows` instead: the same frames, but the pane is sized to the tile,
//! so the agent lays out for it and the tile is filled. If someone else holds
//! the pane (a direct attach), control is refused and the tile falls back to
//! observing at the pane's own size.
//!
//! If a host's herdr can't stream (older version), the tile falls back to
//! polling `pane read --source visible --ansi` every 1.5 s.
//!
//! A control stream also takes input, as herdr's own attach does (JSON lines
//! on its stdin): keys and pastes (`terminal.input`) and clicks
//! (`terminal.mouse`). The wheel: an agent that takes it itself (it asked
//! for the mouse, or it's on its alternate screen: OpenCode) gets it
//! (`terminal.scroll`); any other pane's history is read once when the wheel
//! first goes up (`pane read --source recent`) and scrolled here, frozen —
//! herdr's own view of the history jumps back to the bottom each time an
//! agent clears and redraws its screen (Claude Code does, as it works), which
//! flickers. `pane get` says which (an alternate screen has no history to go
//! up into). An observed tile always scrolls its copy.

use crate::chan::{self, PATH_PRELUDE};
use crate::config::Target;
use crate::herdr::Herdr;
use crate::proc::sh_quote;
use crate::sshcfg;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use serde_json::json;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Virtual size we observe at; the pane renders into it top-left and we crop
/// to the tile, keeping the bottom (where an agent's prompt lives) in view.
pub const COLS: u16 = 220;
pub const ROWS: u16 = 64;

#[derive(Clone, Debug, Default)]
pub struct State {
    /// the pane is sized to the tile (a control stream)
    pub fitted: bool,
    pub connected: bool,
    pub polling: bool,
    pub frames: u64,
    pub last: Option<Instant>,
    pub err: Option<String>,
    pub closed: bool,
}

/// Lines the wheel moves at a time.
pub const WHEEL: u16 = 3;
/// How much recent output an observed tile scrolls through.
const HISTORY: usize = 3000;

/// How long a pane's way with the wheel is trusted before it's asked again.
const ROUTE_FOR: Duration = Duration::from_secs(20);

/// Who scrolls a control stream's pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Route {
    /// the agent itself: herdr passes the wheel on (mouse reporting, or the
    /// alternate screen's arrow keys)
    Agent,
    /// its history: a copy, read once and scrolled here
    Here,
}

/// Where the wheel has taken a tile.
#[derive(Default)]
struct Back {
    /// rows scrolled up (0: the live screen). Scrolled by the agent: herdr's
    /// own count once it's read (`pane get`), our estimate until then
    up: usize,
    /// the pane's recent output, once read (an observed tile, or a control
    /// stream's pane scrolled here)
    lines: Option<Vec<Line<'static>>>,
    loading: bool,
    /// scrolled by the agent: when the wheel last moved (herdr is asked
    /// where it is a moment after)
    moved: Option<Instant>,
    /// a control stream: who scrolls its pane, and since when that's known
    route: Option<(Route, Instant)>,
    /// asking herdr which, and the wheel's lines meanwhile (+ up, − down)
    deciding: bool,
    pending: isize,
}

/// What the wheel did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wheel {
    /// herdr scrolled the pane (or gave the wheel to the agent)
    Sent,
    /// the tile shows the pane's history, this far up
    Back(usize),
    /// back at the live screen
    Live,
    /// nothing to scroll yet
    None,
}

pub struct Stream {
    pub host: String,
    pub pane: String,
    /// the tile size asked for (None: observe at the pane's own size)
    pub size: Option<(u16, u16)>,
    target: Target,
    parser: Arc<Mutex<vt100::Parser>>,
    pub state: Arc<Mutex<State>>,
    child: Arc<Mutex<Option<Child>>>,
    stop: Arc<Mutex<bool>>,
    back: Arc<Mutex<Back>>,
    /// what's on screen, kept still while a selection is dragged across it
    held: Mutex<Option<vt100::Screen>>,
}

/// One JSON command on a control stream's stdin; false when there's none
/// to take it (not fitted, not connected, closed).
fn send(child: &Mutex<Option<Child>>, state: &Mutex<State>, v: serde_json::Value) -> bool {
    {
        let s = state.lock().unwrap();
        if !(s.fitted && s.connected && !s.closed && !s.polling) {
            return false;
        }
    }
    let mut g = child.lock().unwrap();
    let Some(si) = g.as_mut().and_then(|c| c.stdin.as_mut()) else {
        return false;
    };
    let mut line = v.to_string();
    line.push('\n');
    si.write_all(line.as_bytes())
        .and_then(|_| si.flush())
        .is_ok()
}

/// The wheel to herdr: `lines` up (+) or down (−) at a cell.
fn scroll_cmd(lines: isize, c: u16, r: u16) -> serde_json::Value {
    json!({
        "type": "terminal.scroll",
        "direction": if lines > 0 { "up" } else { "down" },
        "lines": lines.unsigned_abs().clamp(1, u16::MAX as usize), "column": c, "row": r,
    })
}

impl Stream {
    pub fn start(t: &Target, pane: &str) -> Stream {
        Stream::start_sized(t, pane, None)
    }

    /// Stream `pane`, sized to `size` (cols, rows) when given.
    pub fn start_sized(t: &Target, pane: &str, size: Option<(u16, u16)>) -> Stream {
        let parser = Arc::new(Mutex::new(vt100::Parser::new(ROWS, COLS, 0)));
        let state = Arc::new(Mutex::new(State::default()));
        let child: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
        let stop = Arc::new(Mutex::new(false));
        let (t2, p2, pa, st, ch, sp) = (
            t.clone(),
            pane.to_string(),
            parser.clone(),
            state.clone(),
            child.clone(),
            stop.clone(),
        );
        thread::spawn(move || run(t2, p2, pa, st, ch, sp, size));
        Stream {
            host: t.ssh.clone(),
            pane: pane.into(),
            size,
            target: t.clone(),
            parser,
            state,
            child,
            stop,
            back: Arc::new(Mutex::new(Back::default())),
            held: Mutex::new(None),
        }
    }

    /// A stream with nothing behind it, showing this output (render tests).
    #[cfg(test)]
    pub fn fixed(pane: &str, cols: u16, rows: u16, output: &[u8]) -> Stream {
        let mut p = vt100::Parser::new(rows, cols, 0);
        p.process(output);
        Stream {
            host: String::new(),
            pane: pane.into(),
            size: Some((cols, rows)),
            target: Target {
                ssh: String::new(),
                machine: String::new(),
                source: "",
            },
            parser: Arc::new(Mutex::new(p)),
            state: Arc::new(Mutex::new(State::default())),
            child: Arc::new(Mutex::new(None)),
            stop: Arc::new(Mutex::new(true)),
            back: Arc::new(Mutex::new(Back::default())),
            held: Mutex::new(None),
        }
    }

    pub fn state(&self) -> State {
        self.state.lock().unwrap().clone()
    }

    /// Paint the emulated screen into `area`, bottom-anchored to the last
    /// non-blank row (or, scrolled back in an observed tile, its history).
    /// Returns the cursor cell if it's visible in the area.
    pub fn render(&self, buf: &mut Buffer, area: Rect) -> Option<(u16, u16)> {
        {
            let b = self.back.lock().unwrap();
            if let (Some(lines), true) = (&b.lines, b.up > 0) {
                paint_history(lines, b.up, buf, area);
                return None;
            }
        }
        if let Some(s) = self.held.lock().unwrap().as_ref() {
            return paint(s, buf, area);
        }
        let p = self.parser.lock().unwrap();
        paint(p.screen(), buf, area)
    }

    /// Keep the screen as it is while a selection is dragged across it.
    pub fn hold(&self) {
        let mut h = self.held.lock().unwrap();
        if h.is_none() {
            *h = Some(self.parser.lock().unwrap().screen().clone());
        }
    }

    /// Live again.
    pub fn release(&self) {
        *self.held.lock().unwrap() = None;
    }

    /// The text from one point of `area` to another, both ends included, as
    /// `render` drew it: a line the terminal wrapped stays one line, and what
    /// a tile cuts off a row's end comes along when the selection runs past it.
    pub fn text_between(&self, area: Rect, a: (u16, u16), b: (u16, u16)) -> String {
        // by row, then by column
        let ((c1, r1), (c2, r2)) = if (a.1, a.0) <= (b.1, b.0) {
            (a, b)
        } else {
            (b, a)
        };
        let (c1, c2) = (c1.saturating_sub(area.x), c2.saturating_sub(area.x));
        let (r1, r2) = (r1.saturating_sub(area.y), r2.saturating_sub(area.y));
        {
            let back = self.back.lock().unwrap();
            if let (Some(lines), true) = (&back.lines, back.up > 0) {
                let end = lines.len().saturating_sub(back.up);
                let start = end.saturating_sub(area.height as usize);
                return history_text(&lines[start..end], (r1, c1), (r2, c2));
            }
        }
        let held = self.held.lock().unwrap();
        let p = self.parser.lock().unwrap();
        let s = held.as_ref().unwrap_or(p.screen());
        let top = (content_bottom(s) + 1).saturating_sub(area.height);
        trim_lines(&s.contents_between(r1 + top, c1, r2 + top, c2 + 1))
    }

    /// The agent asked for application cursor keys (as far as its frames say).
    pub fn app_cursor(&self) -> bool {
        self.parser.lock().unwrap().screen().application_cursor()
    }

    /// Does it take input (a control stream, live)?
    pub fn writable(&self) -> bool {
        let s = self.state();
        s.fitted && s.connected && !s.closed && !s.polling
    }

    /// One JSON command on the control stream's stdin; false when there's
    /// no control stream to take it.
    fn command(&self, v: serde_json::Value) -> bool {
        send(&self.child, &self.state, v)
    }

    /// Keys, as a terminal sends them (`key_bytes`); herdr brings the pane
    /// back to its live screen first.
    pub fn input(&self, text: &str) -> bool {
        let ok = self.command(json!({ "type": "terminal.input", "text": text }));
        if ok {
            let mut b = self.back.lock().unwrap();
            b.up = 0;
            b.lines = None;
            b.moved = None;
        }
        ok
    }

    /// A paste, bracketed: herdr hands it to the agent as one paste (an
    /// agent that takes pastes never sees its newlines as enter).
    pub fn paste(&self, text: &str) -> bool {
        self.input(&format!("\x1b[200~{text}\x1b[201~"))
    }

    /// The screen cell under a point of `area` (as `render` drew it).
    pub fn cell_at(&self, area: Rect, col: u16, row: u16) -> Option<(u16, u16)> {
        if col < area.x || row < area.y || col >= area.x + area.width || row >= area.y + area.height
        {
            return None;
        }
        let p = self.parser.lock().unwrap();
        let top = (content_bottom(p.screen()) + 1).saturating_sub(area.height);
        Some((col - area.x, row - area.y + top))
    }

    /// A click (`down`, `up`, `drag`) at a point of `area`: herdr gives it
    /// to an agent that asked for the mouse, and drops it otherwise.
    pub fn mouse(&self, area: Rect, action: &str, col: u16, row: u16) -> bool {
        let Some((c, r)) = self.cell_at(area, col, row) else {
            return false;
        };
        self.command(json!({
            "type": "terminal.mouse", "action": action, "button": "left", "column": c, "row": r,
        }))
    }

    /// The wheel at a point of `area`. A control stream: to the agent when
    /// it takes the wheel itself (herdr passes it on), else through a copy
    /// of the pane's history, here; which is asked of herdr the first time
    /// (the wheel's lines wait meanwhile). An observed tile: its copy.
    pub fn wheel(&self, area: Rect, up: bool, col: u16, row: u16) -> Wheel {
        let step = if up {
            WHEEL as isize
        } else {
            -(WHEEL as isize)
        };
        if self.writable() {
            let (c, r) = self.cell_at(area, col, row).unwrap_or((0, 0));
            let mut b = self.back.lock().unwrap();
            // the way stays known while scrolled; asked again after a while at the bottom
            let route = b
                .route
                .filter(|(_, t)| b.up > 0 || t.elapsed() < ROUTE_FOR)
                .map(|(x, _)| x);
            match route {
                Some(Route::Agent) => {
                    drop(b);
                    if !self.command(scroll_cmd(step, c, r)) {
                        return Wheel::None;
                    }
                    let mut b = self.back.lock().unwrap();
                    b.up = (b.up as isize + step).max(0) as usize;
                    b.moved = Some(Instant::now());
                    return Wheel::Sent;
                }
                Some(Route::Here) => return self.through_copy(&mut b, area, step),
                None => {
                    b.pending += step;
                    if !b.deciding {
                        b.deciding = true;
                        let (t, pane, back) =
                            (self.target.clone(), self.pane.clone(), self.back.clone());
                        let (child, state) = (self.child.clone(), self.state.clone());
                        let height = area.height as usize;
                        thread::spawn(move || {
                            let h = Herdr::new(&t);
                            // no history to go up into: an alternate screen (or an
                            // empty pane), so the agent takes the wheel
                            let here = h.scroll_metrics(&pane).is_some_and(|(_, max)| max > 0);
                            let lines = here
                                .then(|| h.read(&pane, "recent", HISTORY, true).ok())
                                .flatten()
                                .map(|t| crate::ansi::lines(&t));
                            let mut b = back.lock().unwrap();
                            b.deciding = false;
                            let pending = std::mem::take(&mut b.pending);
                            match lines {
                                Some(l) => {
                                    b.route = Some((Route::Here, Instant::now()));
                                    let room = l.len().saturating_sub(height);
                                    b.up =
                                        (b.up as isize + pending).clamp(0, room as isize) as usize;
                                    b.lines = Some(l);
                                }
                                None => {
                                    b.route = Some((Route::Agent, Instant::now()));
                                    if pending != 0
                                        && send(&child, &state, scroll_cmd(pending, c, r))
                                    {
                                        b.up = (b.up as isize + pending).max(0) as usize;
                                        b.moved = Some(Instant::now());
                                    }
                                }
                            }
                        });
                    }
                    return Wheel::None;
                }
            }
        }
        let mut b = self.back.lock().unwrap();
        self.through_copy(&mut b, area, step)
    }

    /// The wheel through the pane's copied history (read the first time the
    /// wheel goes up from the bottom, afresh each time).
    fn through_copy(&self, b: &mut Back, area: Rect, step: isize) -> Wheel {
        if step > 0 && b.lines.is_none() && !b.loading {
            b.loading = true;
            let (t, pane, back) = (self.target.clone(), self.pane.clone(), self.back.clone());
            let height = area.height as usize;
            thread::spawn(move || {
                let r = Herdr::new(&t).read(&pane, "recent", HISTORY, true);
                let mut b = back.lock().unwrap();
                b.loading = false;
                let l = r.map(|t| crate::ansi::lines(&t)).unwrap_or_default();
                // the wheel went on while it was read: no further than the top
                b.up = b.up.min(l.len().saturating_sub(height));
                b.lines = Some(l);
            });
        }
        b.up = (b.up as isize + step).max(0) as usize;
        if let Some(n) = b.lines.as_ref().map(Vec::len) {
            b.up = b.up.min(n.saturating_sub(area.height as usize));
        }
        if b.up == 0 && !b.loading {
            // back at the bottom: read afresh next time
            b.lines = None;
            Wheel::Live
        } else {
            Wheel::Back(b.up)
        }
    }

    /// How far back the tile is (0: live), and whether its history is still
    /// being read. A control stream asks herdr where its pane really is a
    /// moment after the wheel stops (an agent that took the wheel scrolled
    /// itself, not the pane).
    pub fn scrolled(&self) -> (usize, bool) {
        let mut b = self.back.lock().unwrap();
        if let Some(t) = b.moved {
            if t.elapsed() > Duration::from_millis(400) {
                b.moved = None;
                let (tg, pane, back) = (self.target.clone(), self.pane.clone(), self.back.clone());
                thread::spawn(move || {
                    if let Some(n) = Herdr::new(&tg).scroll_offset(&pane) {
                        let mut b = back.lock().unwrap();
                        if b.moved.is_none() {
                            b.up = n as usize;
                        }
                    }
                });
            }
        }
        (b.up, b.loading)
    }

    /// Back to the live screen.
    pub fn live(&self) {
        let agent = matches!(self.back.lock().unwrap().route, Some((Route::Agent, _)));
        if agent && self.writable() && self.back.lock().unwrap().up > 0 {
            // herdr's own way back to the bottom: a scroll down past it
            let n = self.back.lock().unwrap().up.min(u16::MAX as usize) as u16;
            self.command(
                json!({ "type": "terminal.scroll", "direction": "down", "lines": n.max(1) }),
            );
        }
        let mut b = self.back.lock().unwrap();
        b.up = 0;
        b.lines = None;
        b.moved = None;
    }

    /// Plain text of the visible screen (for tests and copy).
    pub fn text(&self) -> String {
        self.parser.lock().unwrap().screen().contents()
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        *self.stop.lock().unwrap() = true;
        if let Some(mut c) = self.child.lock().unwrap().take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn color(c: vt100::Color) -> Color {
    match c {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

/// Last row with any content (or the cursor row, whichever is lower).
fn content_bottom(s: &vt100::Screen) -> u16 {
    let (rows, cols) = s.size();
    let mut bottom = 0;
    for r in (0..rows).rev() {
        if (0..cols).any(|c| s.cell(r, c).is_some_and(|x| x.has_contents())) {
            bottom = r;
            break;
        }
    }
    bottom.max(s.cursor_position().0.min(rows.saturating_sub(1)))
}

pub fn paint(s: &vt100::Screen, buf: &mut Buffer, area: Rect) -> Option<(u16, u16)> {
    if area.width == 0 || area.height == 0 {
        return None;
    }
    let bottom = content_bottom(s);
    let top = (bottom + 1).saturating_sub(area.height);
    for y in 0..area.height {
        let r = top + y;
        for x in 0..area.width {
            let Some(cell) = s.cell(r, x) else { continue };
            if cell.is_wide_continuation() {
                continue;
            }
            let mut st = Style::new()
                // on a theme's own ground, its colours made to read there
                .fg(crate::ui::ink(color(cell.fgcolor())))
                .bg(color(cell.bgcolor()));
            if cell.bold() {
                st = st.add_modifier(Modifier::BOLD);
            }
            if cell.italic() {
                st = st.add_modifier(Modifier::ITALIC);
            }
            if cell.underline() {
                st = st.add_modifier(Modifier::UNDERLINED);
            }
            if cell.inverse() {
                st = st.add_modifier(Modifier::REVERSED);
            }
            let txt = cell.contents();
            if let Some(c) = buf.cell_mut((area.x + x, area.y + y)) {
                c.set_symbol(if txt.is_empty() { " " } else { &txt });
                c.set_style(st);
            }
        }
    }
    crate::ui::agent_screen(area);
    // showcase: the screen redacted or hidden (`showcase.screens`; no-op otherwise)
    if crate::showcase::screen_area(buf, area) {
        return None;
    }
    let (cr, cc) = s.cursor_position();
    (!s.hide_cursor() && cr >= top && cr < top + area.height && cc < area.width)
        .then(|| (area.x + cc, area.y + cr - top))
}

/// Blanks at the ends of lines, and empty rows after the last line, are the
/// screen's, not the text's.
fn trim_lines(text: &str) -> String {
    let v: Vec<&str> = text.lines().map(str::trim_end).collect();
    v.join("\n").trim_end().to_string()
}

/// What's selected of rows of history (row, col), both ends included.
fn history_text(rows: &[Line<'static>], from: (u16, u16), to: (u16, u16)) -> String {
    let mut out: Vec<String> = Vec::new();
    for (i, l) in rows.iter().enumerate() {
        let i = i as u16;
        if i < from.0 || i > to.0 {
            continue;
        }
        let first = if i == from.0 { from.1 as usize } else { 0 };
        let last = if i == to.0 {
            to.1 as usize + 1
        } else {
            usize::MAX
        };
        let (mut col, mut s) = (0, String::new());
        for ch in l.spans.iter().flat_map(|sp| sp.content.chars()) {
            if col >= first && col < last {
                s.push(ch);
            }
            col += crate::ui::width(ch.encode_utf8(&mut [0; 4]));
        }
        out.push(s.trim_end().to_string());
    }
    out.join("\n").trim_end().to_string()
}

/// An observed tile scrolled back: the history's rows ending `up` above its
/// last one.
fn paint_history(lines: &[Line<'static>], up: usize, buf: &mut Buffer, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let end = lines.len().saturating_sub(up);
    let start = end.saturating_sub(area.height as usize);
    for (i, l) in lines[start..end].iter().enumerate() {
        let y = area.y + i as u16;
        for x in area.x..area.x + area.width {
            if let Some(c) = buf.cell_mut((x, y)) {
                c.reset();
            }
        }
        buf.set_line(area.x, y, l, area.width);
    }
    crate::ui::agent_screen(area);
    crate::showcase::screen_area(buf, area);
}

/// The bytes a terminal sends for a key (xterm's: CSI arrows with their
/// modifiers, `\x7f` backspace, `\r` enter; alt as a leading esc; ctrl
/// letters as control characters); None for keys a terminal sends nothing
/// for. `app_cursor`: the agent asked for application cursor keys.
pub fn key_bytes(k: &crossterm::event::KeyEvent, app_cursor: bool) -> Option<String> {
    use crossterm::event::{KeyCode, KeyModifiers};
    let m = k.modifiers;
    // Cmd + a key is the terminal's own: as bytes it would just be the letter
    if m.contains(KeyModifiers::SUPER) {
        return None;
    }
    let (shift, alt, ctrl) = (
        m.contains(KeyModifiers::SHIFT),
        m.contains(KeyModifiers::ALT),
        m.contains(KeyModifiers::CONTROL),
    );
    // xterm's modifier parameter: 1 + shift + 2·alt + 4·ctrl
    let mods = 1 + shift as u8 + 2 * alt as u8 + 4 * ctrl as u8;
    let esc = |s: &str| {
        if alt {
            format!("\x1b{s}")
        } else {
            s.to_string()
        }
    };
    let csi = |fin: char| {
        if mods > 1 {
            format!("\x1b[1;{mods}{fin}")
        } else if app_cursor {
            format!("\x1bO{fin}")
        } else {
            format!("\x1b[{fin}")
        }
    };
    let tilde = |n: u8| {
        if mods > 1 {
            format!("\x1b[{n};{mods}~")
        } else {
            format!("\x1b[{n}~")
        }
    };
    Some(match k.code {
        KeyCode::Char(c) if ctrl => {
            let b = match c.to_ascii_lowercase() {
                c @ 'a'..='z' => (c as u8) - b'a' + 1,
                ' ' | '@' | '2' => 0,
                '[' | '3' => 0x1b,
                '\\' | '4' => 0x1c,
                ']' | '5' => 0x1d,
                '^' | '6' => 0x1e,
                '_' | '-' | '7' => 0x1f,
                '?' | '8' => 0x7f,
                // ctrl+. ctrl+, ctrl+; …: CSI u (its code point, its modifiers)
                c => return Some(format!("\x1b[{};{mods}u", c as u32)),
            };
            esc(&(b as char).to_string())
        }
        KeyCode::Char(c) => esc(&c.to_string()),
        // ctrl with enter, tab or esc has no bytes of its own in the classic
        // encoding: the one modern terminals use for it (CSI u, as kitty,
        // Ghostty, WezTerm, foot, iTerm2 send) — what an agent listens for
        KeyCode::Enter if ctrl => format!("\x1b[13;{mods}u"),
        KeyCode::Tab if ctrl => format!("\x1b[9;{mods}u"),
        KeyCode::Esc if ctrl || shift => format!("\x1b[27;{mods}u"),
        // shift+enter: a newline in the agent's box (as alt+enter is)
        KeyCode::Enter if shift || alt => "\x1b\r".into(),
        KeyCode::Enter => "\r".into(),
        KeyCode::Tab if shift => "\x1b[Z".into(),
        KeyCode::Tab => esc("\t"),
        KeyCode::BackTab => "\x1b[Z".into(),
        KeyCode::Backspace if ctrl => esc("\x08"),
        KeyCode::Backspace => esc("\x7f"),
        KeyCode::Esc => "\x1b".into(),
        KeyCode::Up => csi('A'),
        KeyCode::Down => csi('B'),
        KeyCode::Right => csi('C'),
        KeyCode::Left => csi('D'),
        KeyCode::Home => csi('H'),
        KeyCode::End => csi('F'),
        KeyCode::Insert => tilde(2),
        KeyCode::Delete => tilde(3),
        KeyCode::PageUp => tilde(5),
        KeyCode::PageDown => tilde(6),
        KeyCode::F(n @ 1..=4) => {
            let fin = (b'P' + n - 1) as char;
            if mods > 1 {
                format!("\x1b[1;{mods}{fin}")
            } else {
                format!("\x1bO{fin}")
            }
        }
        KeyCode::F(n @ 5..=12) => tilde([15, 17, 18, 19, 20, 21, 23, 24][(n - 5) as usize]),
        _ => return None,
    })
}

/// Minimal standard base64 decoder (frames are small; no extra dependency).
pub fn b64(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits = 0;
    for ch in s.bytes() {
        let v = match ch {
            b'A'..=b'Z' => ch - b'A',
            b'a'..=b'z' => ch - b'a' + 26,
            b'0'..=b'9' => ch - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => continue,
        } as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    out
}

fn run(
    t: Target,
    pane: String,
    parser: Arc<Mutex<vt100::Parser>>,
    state: Arc<Mutex<State>>,
    child: Arc<Mutex<Option<Child>>>,
    stop: Arc<Mutex<bool>>,
    size: Option<(u16, u16)>,
) {
    let stopped = || *stop.lock().unwrap();
    // showcase's demo world: its frames, drawn at the tile's size, 4 a second
    while let Some(r) = crate::showcase::screen(&t.ssh, &pane, size) {
        if stopped() {
            return;
        }
        if !show(&parser, &state, r) {
            return;
        }
        thread::sleep(Duration::from_millis(250));
    }
    let mut attempts = 0u32;
    let mut fit = size;
    // observing because control was refused: when to try control again
    let mut refit_at: Option<Instant> = None;
    loop {
        if stopped() {
            return;
        }
        if fit.is_none() && refit_at.is_some_and(|t| Instant::now() >= t) {
            fit = size;
            refit_at = None;
        }
        let before = state.lock().unwrap().frames;
        state.lock().unwrap().fitted = fit.is_some();
        let r = observe(&t, &pane, &parser, &state, &child, &stop, fit);
        if stopped() {
            return;
        }
        let frames = state.lock().unwrap().frames;
        // control refused (someone holds the pane) or unsupported: observe for
        // now, and try control again in a while (the holder may detach)
        if fit.is_some() && frames == before {
            fit = None;
            refit_at = Some(Instant::now() + Duration::from_secs(30));
            state.lock().unwrap().fitted = false;
            continue;
        }
        // a run that streamed was healthy: the next drop retries at once, and
        // never counts toward giving up on streaming
        if frames > before {
            attempts = 0;
        }
        attempts += 1;
        match r {
            // pane gone: say so and stop
            Err(e) if e.contains("not found") => {
                let mut s = state.lock().unwrap();
                s.connected = false;
                s.closed = true;
                s.err = Some("session ended".into());
                return;
            }
            // never got a frame twice in a row → this herdr can't stream: poll instead
            _ if frames == 0 && attempts >= 2 => break,
            // the stream ended without herdr saying the pane closed (ssh dropped,
            // a herdr restart): a reconnect, after a short wait
            Err(e) => state.lock().unwrap().err = Some(e),
            Ok(()) => {}
        }
        state.lock().unwrap().connected = false;
        let wait = if attempts <= 1 {
            300
        } else {
            800 * attempts.min(5) as u64
        };
        thread::sleep(Duration::from_millis(wait));
    }
    // polling fallback
    state.lock().unwrap().polling = true;
    let h = Herdr::new(&t);
    while !stopped() {
        if !show(&parser, &state, h.read(&pane, "visible", 0, true)) {
            return;
        }
        thread::sleep(Duration::from_millis(1500));
    }
}

/// A whole screen read (polled, or the demo world's frame) into the emulator;
/// false when the pane is gone.
fn show(
    parser: &Arc<Mutex<vt100::Parser>>,
    state: &Arc<Mutex<State>>,
    r: Result<String, String>,
) -> bool {
    match r {
        Ok(text) => {
            let mut p = parser.lock().unwrap();
            p.process(b"\x1b[0m\x1b[2J\x1b[H");
            p.process(text.replace('\n', "\r\n").as_bytes());
            let mut s = state.lock().unwrap();
            s.connected = true;
            s.frames += 1;
            s.last = Some(Instant::now());
            s.err = None;
        }
        Err(e) => {
            let mut s = state.lock().unwrap();
            if e.contains("not found") {
                s.closed = true;
                s.err = Some("session ended".into());
                return false;
            }
            s.err = Some(e);
            s.connected = false;
        }
    }
    true
}

fn observe(
    t: &Target,
    pane: &str,
    parser: &Arc<Mutex<vt100::Parser>>,
    state: &Arc<Mutex<State>>,
    child: &Arc<Mutex<Option<Child>>>,
    stop: &Arc<Mutex<bool>>,
    fit: Option<(u16, u16)>,
) -> Result<(), String> {
    let herdr = chan::herdr_path(&t.ssh);
    let (verb, cols, rows) = match fit {
        Some((c, r)) => ("control", c.max(20), r.max(5)),
        None => ("observe", COLS, ROWS),
    };
    let script = format!(
        "{PATH_PRELUDE}; {}exec {} terminal session {verb} {} --cols {cols} --rows {rows}\n",
        chan::herdr_env(&t.ssh),
        sh_quote(&herdr),
        sh_quote(pane)
    );
    let mut c = sshcfg::shell(&t.ssh, "sh -s")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("ssh: {e}"))?;
    if let Some(si) = c.stdin.as_mut() {
        // stdin stays open (owned by the child handle): sh exec's herdr, which inherits it
        let _ = si.write_all(script.as_bytes());
        let _ = si.flush();
    }
    let out = c.stdout.take().ok_or("no stdout")?;
    let err = c.stderr.take();
    {
        let mut g = child.lock().unwrap();
        if *stop.lock().unwrap() {
            let _ = c.kill();
            return Ok(());
        }
        *g = Some(c);
    }
    let errbuf = Arc::new(Mutex::new(String::new()));
    if let Some(e) = err {
        let eb = errbuf.clone();
        thread::spawn(move || {
            for l in BufReader::new(e).lines().map_while(Result::ok) {
                let mut g = eb.lock().unwrap();
                if g.len() < 2000 {
                    g.push_str(&l);
                    g.push('\n');
                }
            }
        });
    }
    for l in BufReader::new(out).lines().map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&l) else {
            continue;
        };
        match v.get("type").and_then(|x| x.as_str()) {
            Some("terminal.frame") => {
                let bytes = b64(v.get("bytes").and_then(|x| x.as_str()).unwrap_or(""));
                let w = v
                    .get("width")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(COLS as u64) as u16;
                let h = v
                    .get("height")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(ROWS as u64) as u16;
                let mut p = parser.lock().unwrap();
                if p.screen().size() != (h.max(1), w.max(1)) {
                    p.set_size(h.max(1), w.max(1));
                }
                p.process(&bytes);
                let mut s = state.lock().unwrap();
                s.connected = true;
                s.frames += 1;
                s.last = Some(Instant::now());
                s.err = None;
            }
            Some("terminal.closed") => {
                let r = v
                    .get("reason")
                    .and_then(|x| x.as_str())
                    .unwrap_or("closed")
                    .to_string();
                return Err(r);
            }
            _ => {}
        }
    }
    if let Some(mut c) = child.lock().unwrap().take() {
        let _ = c.kill();
        let _ = c.wait();
    }
    let e = errbuf.lock().unwrap().clone();
    let last = e
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .to_string();
    if last.contains("not found") && last.contains("herdr") && !last.contains("terminal") {
        chan::forget(&t.ssh);
    }
    if last.is_empty() {
        Ok(())
    } else {
        Err(last)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_selection_reads_what_the_tile_shows() {
        let area = Rect::new(10, 5, 20, 4);
        // (the screen is as wide as the tile; what's on it starts at its top)
        let st = Stream::fixed("w:p", 20, 4, b"first line   \r\nsecond line\r\nthird");
        let at = |c: u16, r: u16| (area.x + c, area.y + r);
        let top = 0;
        let one = st.text_between(area, at(6, top), at(9, top));
        assert_eq!(one, "line");
        // either way round, and blanks at line ends aren't the text's
        assert_eq!(
            st.text_between(area, at(2, top + 1), at(0, top)),
            "first line\nsec"
        );
        assert_eq!(
            st.text_between(area, at(0, top), at(19, top + 2)),
            "first line\nsecond line\nthird"
        );
        // a line the terminal wrapped is one line; wide characters stay whole
        let st = Stream::fixed("w:p", 8, 3, "abcdefghij\r\n日本語x".as_bytes());
        let area = Rect::new(0, 0, 8, 3);
        assert_eq!(st.text_between(area, (0, 0), (7, 2)), "abcdefghij\n日本語x");
        // held: the screen stays as it was while the drag goes on
        st.hold();
        st.parser
            .lock()
            .unwrap()
            .process(b"\r\nmore\r\nand more\r\n");
        assert_eq!(st.text_between(area, (0, 0), (7, 0)), "abcdefgh");
        st.release();
        assert_ne!(st.text_between(area, (0, 0), (7, 0)), "abcdefgh");
    }

    #[test]
    fn a_selection_reads_a_tiles_history() {
        let area = Rect::new(0, 0, 10, 2);
        let st = Stream::fixed("w:p", 10, 2, b"live");
        {
            let mut b = st.back.lock().unwrap();
            b.lines = Some(vec![
                Line::raw("one"),
                Line::raw("two words  "),
                Line::raw("three"),
                Line::raw("four"),
            ]);
            b.up = 1;
        }
        // two rows ending one above the last: two words, three
        assert_eq!(st.text_between(area, (4, 0), (2, 1)), "words\nthr");
        assert_eq!(st.text_between(area, (0, 1), (9, 1)), "three");
    }

    #[test]
    fn keys_are_what_a_terminal_sends() {
        use crossterm::event::{KeyCode as C, KeyEvent, KeyModifiers as M};
        let b = |code, m| key_bytes(&KeyEvent::new(code, m), false).unwrap();
        assert_eq!(b(C::Char('a'), M::NONE), "a");
        assert_eq!(b(C::Char('A'), M::SHIFT), "A");
        assert_eq!(b(C::Char('c'), M::CONTROL), "\x03");
        assert_eq!(b(C::Char(']'), M::CONTROL), "\x1d");
        assert_eq!(b(C::Char('5'), M::CONTROL), "\x1d");
        assert_eq!(b(C::Char('x'), M::ALT), "\x1bx");
        assert_eq!(b(C::Enter, M::NONE), "\r");
        assert_eq!(b(C::Enter, M::SHIFT), "\x1b\r");
        assert_eq!(b(C::Backspace, M::NONE), "\x7f");
        assert_eq!(b(C::Esc, M::NONE), "\x1b");
        assert_eq!(b(C::BackTab, M::SHIFT), "\x1b[Z");
        assert_eq!(b(C::Up, M::NONE), "\x1b[A");
        assert_eq!(b(C::Right, M::CONTROL), "\x1b[1;5C");
        assert_eq!(b(C::Left, M::ALT), "\x1b[1;3D");
        assert_eq!(b(C::Delete, M::NONE), "\x1b[3~");
        assert_eq!(b(C::PageUp, M::NONE), "\x1b[5~");
        assert_eq!(b(C::F(1), M::NONE), "\x1bOP");
        assert_eq!(b(C::F(5), M::NONE), "\x1b[15~");
        assert_eq!(b(C::F(12), M::NONE), "\x1b[24~");
        // chords with no classic bytes: CSI u, as modern terminals send them
        assert_eq!(b(C::Enter, M::CONTROL), "\x1b[13;5u");
        assert_eq!(b(C::Enter, M::CONTROL | M::SHIFT), "\x1b[13;6u");
        assert_eq!(b(C::Tab, M::CONTROL), "\x1b[9;5u");
        assert_eq!(b(C::Esc, M::SHIFT), "\x1b[27;2u");
        assert_eq!(b(C::Char('.'), M::CONTROL), "\x1b[46;5u");
        // the agent asked for application cursor keys
        let up = KeyEvent::new(C::Up, M::NONE);
        assert_eq!(key_bytes(&up, true).unwrap(), "\x1bOA");
        assert_eq!(key_bytes(&KeyEvent::new(C::F(20), M::NONE), false), None);
        // Cmd+C is the terminal's: never a c for the agent
        assert_eq!(
            key_bytes(&KeyEvent::new(C::Char('c'), M::SUPER), false),
            None
        );
    }

    #[test]
    fn a_tile_scrolled_back_shows_its_history_up_from_the_bottom() {
        let lines: Vec<Line<'static>> = (0..10).map(|i| Line::raw(format!("line {i}"))).collect();
        let area = Rect::new(0, 0, 12, 3);
        let mut buf = Buffer::empty(area);
        paint_history(&lines, 2, &mut buf, area);
        let row = |y: u16| {
            (0..12)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect::<String>()
        };
        assert_eq!(
            [row(0), row(1), row(2)].map(|r| r.trim_end().to_string()),
            ["line 5", "line 6", "line 7"]
        );
    }

    #[test]
    fn the_wheel_to_herdr() {
        let up = scroll_cmd(9, 3, 4);
        assert_eq!(
            (
                up["direction"].as_str(),
                up["lines"].as_u64(),
                up["column"].as_u64()
            ),
            (Some("up"), Some(9), Some(3))
        );
        let down = scroll_cmd(-3, 0, 0);
        assert_eq!(
            (down["direction"].as_str(), down["lines"].as_u64()),
            (Some("down"), Some(3))
        );
        // a stream that isn't up yet takes no command, and the wheel waits for nothing
        let st = Mutex::new(State::default());
        assert!(!send(&Mutex::new(None), &st, up));
    }

    #[test]
    fn base64_and_paint() {
        assert_eq!(b64("aGVsbG8="), b"hello");
        assert_eq!(b64("AAEC"), vec![0, 1, 2]);
        let mut p = vt100::Parser::new(10, 20, 0);
        p.process(b"line one\r\n\x1b[31mred\x1b[0m\r\n\r\n> prompt");
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 2));
        let cur = paint(p.screen(), &mut buf, Rect::new(0, 0, 10, 2));
        // bottom-anchored: last content row is "> prompt" (row 3); rows 2-3 shown
        let row = |y: u16| {
            (0..10)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect::<String>()
        };
        assert_eq!(row(1).trim_end(), "> prompt");
        assert_eq!(cur, Some((8, 1)));
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 4));
        paint(p.screen(), &mut buf, Rect::new(0, 0, 10, 4));
        assert_eq!(buf[(0, 1)].fg, Color::Indexed(1));
    }
}
