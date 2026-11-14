//! What an agent's terminal shows now: a small terminal emulator fed with
//! its output (the characters of each cell; colours and attributes are
//! dropped), and a reading of that screen for what Claude Code waits on
//! (SPEC.md, "Sixth build: Safe answering"): a menu (a permission prompt,
//! the trust prompt, another dialog) with its options as the screen
//! numbers and labels them and the one the cursor (❯) is on, its text
//! prompt (idle, or after an interrupt), or work in progress.
//!
//! Workbench answers a menu only by what this reading says is on the
//! screen at that moment, so that a button never types a key that the
//! prompt reads as something else.

/// A terminal's screen: `rows` lines of `cols` cells.
pub struct Screen {
    cols: usize,
    rows: usize,
    grid: Vec<Vec<char>>,
    /// Per row: the width it had when the terminal wrapped it (its text
    /// goes on in the next row), or 0.
    wrapped: Vec<usize>,
    cx: usize,
    cy: usize,
    /// The next printable character wraps first (xterm's pending wrap).
    wrap: bool,
    saved: (usize, usize),
    /// The main screen while the alternate one shows.
    main: Option<(Vec<Vec<char>>, Vec<usize>, usize, usize)>,
    top: usize,
    bottom: usize,
    /// DECCKM: the arrow keys are sent as `ESC O A` rather than `ESC [ A`.
    app_cursor: bool,
    st: St,
    params: String,
    utf8: Vec<u8>,
    /// Bumped by every byte that changes the screen.
    pub version: u64,
}

#[derive(Clone, Copy, PartialEq)]
enum St {
    Ground,
    Esc,
    Csi,
    /// OSC, DCS, APC, PM, SOS: skipped up to BEL or ESC \.
    Str,
    StrEsc,
    /// ESC ( X and the like: one more byte.
    Charset,
}

/// A blank cell; the second cell of a wide character holds `WIDE`.
const BLANK: char = ' ';
const WIDE: char = '\0';

fn wide(c: char) -> bool {
    let u = c as u32;
    (0x1100..=0x115F).contains(&u)
        || (0x2E80..=0xA4CF).contains(&u)
        || (0xAC00..=0xD7A3).contains(&u)
        || (0xF900..=0xFAFF).contains(&u)
        || (0xFE30..=0xFE4F).contains(&u)
        || (0xFF00..=0xFF60).contains(&u)
        || (0xFFE0..=0xFFE6).contains(&u)
        || (0x1F300..=0x1F64F).contains(&u)
        || (0x1F900..=0x1F9FF).contains(&u)
        || (0x20000..=0x3FFFD).contains(&u)
}

impl Screen {
    pub fn new(cols: u16, rows: u16) -> Screen {
        let (cols, rows) = (cols.max(1) as usize, rows.max(1) as usize);
        Screen {
            cols,
            rows,
            grid: vec![vec![BLANK; cols]; rows],
            wrapped: vec![0; rows],
            cx: 0,
            cy: 0,
            wrap: false,
            saved: (0, 0),
            main: None,
            top: 0,
            bottom: rows - 1,
            app_cursor: false,
            st: St::Ground,
            params: String::new(),
            utf8: Vec::new(),
            version: 0,
        }
    }

    /// Whether the program asked for application cursor keys.
    pub fn app_cursor(&self) -> bool {
        self.app_cursor
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        let (cols, rows) = (cols.max(1) as usize, rows.max(1) as usize);
        // Lines leave at the top when it shrinks, as a terminal scrolls.
        while self.grid.len() > rows {
            self.grid.remove(0);
            self.wrapped.remove(0);
            self.cy = self.cy.saturating_sub(1);
        }
        while self.grid.len() < rows {
            self.grid.push(vec![BLANK; cols]);
            self.wrapped.push(0);
        }
        // A narrower terminal keeps what is past its edge (out of reach of
        // the cursor) until the program redraws: the text is not cut, as a
        // terminal that reflows would not cut it.
        for l in &mut self.grid {
            if l.len() < cols {
                l.resize(cols, BLANK);
            }
        }
        self.cols = cols;
        self.rows = rows;
        self.top = 0;
        self.bottom = rows - 1;
        self.cx = self.cx.min(cols - 1);
        self.cy = self.cy.min(rows - 1);
        self.wrap = false;
        self.version += 1;
    }

    /// The screen's lines, without trailing blanks; a line the terminal
    /// wrapped (a long one printed past the last column) is one line, as
    /// it was printed (up to the width it wrapped at: a terminal made wider
    /// since pads the row, and the padding is not part of the line).
    pub fn lines(&self) -> Vec<String> {
        let mut out = Vec::with_capacity(self.rows);
        let mut cur = String::new();
        for (l, &w) in self.grid.iter().zip(&self.wrapped) {
            let end = if w == 0 { l.len() } else { w.min(l.len()) };
            cur.extend(l[..end].iter().filter(|c| **c != WIDE));
            if w == 0 {
                out.push(cur.trim_end().to_string());
                cur.clear();
            }
        }
        if !cur.is_empty() {
            out.push(cur.trim_end().to_string());
        }
        out
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        if !bytes.is_empty() {
            self.version += 1;
        }
        for &b in bytes {
            self.byte(b);
        }
    }

    fn byte(&mut self, b: u8) {
        match self.st {
            St::Ground => self.ground(b),
            St::Esc => self.esc(b),
            St::Csi => {
                if (0x40..=0x7e).contains(&b) {
                    self.st = St::Ground;
                    let p = std::mem::take(&mut self.params);
                    self.csi(&p, b as char);
                } else if b == 0x1b {
                    self.params.clear();
                    self.st = St::Esc;
                } else if self.params.len() < 64 {
                    self.params.push(b as char);
                }
            }
            St::Str => match b {
                0x07 => self.st = St::Ground,
                0x1b => self.st = St::StrEsc,
                _ => {}
            },
            St::StrEsc => self.st = if b == b'\\' { St::Ground } else { St::Str },
            St::Charset => self.st = St::Ground,
        }
    }

    fn ground(&mut self, b: u8) {
        if !self.utf8.is_empty() || b >= 0x80 {
            if b < 0x80 || (b & 0xc0 == 0xc0 && !self.utf8.is_empty()) {
                // A sequence cut short.
                self.utf8.clear();
                if b < 0x80 {
                    return self.ground(b);
                }
            }
            self.utf8.push(b);
            let need = match self.utf8[0] {
                x if x >= 0xf0 => 4,
                x if x >= 0xe0 => 3,
                x if x >= 0xc0 => 2,
                _ => 1,
            };
            if self.utf8.len() >= need {
                let s = String::from_utf8_lossy(&self.utf8).to_string();
                self.utf8.clear();
                for c in s.chars() {
                    self.put(c);
                }
            }
            return;
        }
        match b {
            0x1b => self.st = St::Esc,
            b'\r' => {
                self.cx = 0;
                self.wrap = false;
            }
            b'\n' | 0x0b | 0x0c => self.linefeed(),
            0x08 => {
                self.cx = self.cx.saturating_sub(1);
                self.wrap = false;
            }
            b'\t' => {
                self.cx = ((self.cx / 8 + 1) * 8).min(self.cols - 1);
                self.wrap = false;
            }
            0x20..=0x7e => self.put(b as char),
            _ => {}
        }
    }

    fn put(&mut self, c: char) {
        let w = if wide(c) { 2 } else { 1 };
        if self.wrap || self.cx + w > self.cols {
            if self.wrap || self.cx > 0 {
                self.wrapped[self.cy] = self.cols;
                self.cx = 0;
                self.linefeed();
            }
            self.wrap = false;
        }
        let (x, y) = (self.cx, self.cy);
        self.grid[y][x] = c;
        if w == 2 && x + 1 < self.cols {
            self.grid[y][x + 1] = WIDE;
        }
        if x + w >= self.cols {
            self.cx = self.cols - 1;
            self.wrap = true;
        } else {
            self.cx = x + w;
        }
    }

    fn linefeed(&mut self) {
        self.wrap = false;
        if self.cy == self.bottom {
            self.scroll_up(1);
        } else if self.cy + 1 < self.rows {
            self.cy += 1;
        }
    }

    fn scroll_up(&mut self, n: usize) {
        for _ in 0..n.min(self.bottom + 1 - self.top) {
            self.grid.remove(self.top);
            self.grid.insert(self.bottom, vec![BLANK; self.cols]);
            self.wrapped.remove(self.top);
            self.wrapped.insert(self.bottom, 0);
        }
    }

    fn scroll_down(&mut self, n: usize) {
        for _ in 0..n.min(self.bottom + 1 - self.top) {
            self.grid.remove(self.bottom);
            self.grid.insert(self.top, vec![BLANK; self.cols]);
            self.wrapped.remove(self.bottom);
            self.wrapped.insert(self.top, 0);
        }
    }

    fn esc(&mut self, b: u8) {
        self.st = St::Ground;
        match b {
            b'[' => {
                self.params.clear();
                self.st = St::Csi;
            }
            b']' | b'P' | b'_' | b'^' | b'X' => self.st = St::Str,
            b'(' | b')' | b'*' | b'+' | b'#' | b'%' => self.st = St::Charset,
            b'7' => self.saved = (self.cx, self.cy),
            b'8' => {
                (self.cx, self.cy) = self.saved;
                self.wrap = false;
            }
            b'D' => self.linefeed(),
            b'E' => {
                self.cx = 0;
                self.linefeed();
            }
            b'M' => {
                if self.cy == self.top {
                    self.scroll_down(1);
                } else {
                    self.cy = self.cy.saturating_sub(1);
                }
            }
            b'c' => {
                let (c, r) = (self.cols as u16, self.rows as u16);
                let v = self.version;
                *self = Screen::new(c, r);
                self.version = v;
            }
            _ => {}
        }
    }

    fn csi(&mut self, p: &str, f: char) {
        let private = p.starts_with('?');
        let nums: Vec<usize> = p.trim_start_matches(['?', '>', '=', '<']).split(';').map(|x| x.parse().unwrap_or(0)).collect();
        let n = |i: usize| nums.get(i).copied().filter(|v| *v > 0).unwrap_or(1);
        let raw = |i: usize| nums.get(i).copied().unwrap_or(0);
        let (cols, rows) = (self.cols, self.rows);
        if !matches!(f, 'm') {
            self.wrap = false;
        }
        match f {
            'A' => self.cy = self.cy.saturating_sub(n(0)).max(if self.cy >= self.top { self.top } else { 0 }),
            'B' | 'e' => self.cy = (self.cy + n(0)).min(if self.cy <= self.bottom { self.bottom } else { rows - 1 }),
            'C' | 'a' => self.cx = (self.cx + n(0)).min(cols - 1),
            'D' => self.cx = self.cx.saturating_sub(n(0)),
            'E' => {
                self.cy = (self.cy + n(0)).min(rows - 1);
                self.cx = 0;
            }
            'F' => {
                self.cy = self.cy.saturating_sub(n(0));
                self.cx = 0;
            }
            'G' | '`' => self.cx = (n(0) - 1).min(cols - 1),
            'd' => self.cy = (n(0) - 1).min(rows - 1),
            'H' | 'f' => {
                self.cy = (n(0) - 1).min(rows - 1);
                self.cx = (n(1) - 1).min(cols - 1);
            }
            'J' => {
                let (x, y) = (self.cx, self.cy);
                match raw(0) {
                    0 => {
                        self.grid[y][x..].fill(BLANK);
                        for l in &mut self.grid[y + 1..] {
                            l.fill(BLANK);
                        }
                        self.wrapped[y..].fill(0);
                    }
                    1 => {
                        self.grid[y][..=x].fill(BLANK);
                        for l in &mut self.grid[..y] {
                            l.fill(BLANK);
                        }
                        self.wrapped[..y].fill(0);
                    }
                    _ => {
                        for l in &mut self.grid {
                            l.fill(BLANK);
                        }
                        self.wrapped.fill(0);
                    }
                }
            }
            'K' => {
                let (x, y) = (self.cx, self.cy);
                match raw(0) {
                    0 => self.grid[y][x..].fill(BLANK),
                    1 => self.grid[y][..=x].fill(BLANK),
                    _ => self.grid[y].fill(BLANK),
                }
                if raw(0) != 1 {
                    self.wrapped[y] = 0;
                }
            }
            'X' => {
                let (x, y) = (self.cx, self.cy);
                let end = (x + n(0)).min(cols);
                self.grid[y][x..end].fill(BLANK);
            }
            'P' => {
                let (x, y) = (self.cx, self.cy);
                for _ in 0..n(0).min(cols - x) {
                    self.grid[y].remove(x);
                    self.grid[y].push(BLANK);
                }
            }
            '@' => {
                let (x, y) = (self.cx, self.cy);
                for _ in 0..n(0).min(cols - x) {
                    self.grid[y].insert(x, BLANK);
                    self.grid[y].pop();
                }
            }
            'L' | 'M' if (self.top..=self.bottom).contains(&self.cy) => {
                let (top, cy) = (self.top, self.cy);
                self.top = cy;
                if f == 'L' {
                    self.scroll_down(n(0));
                } else {
                    self.scroll_up(n(0));
                }
                self.top = top;
                self.cx = 0;
            }
            'S' => self.scroll_up(n(0)),
            'T' if !private => self.scroll_down(n(0)),
            'r' if !private => {
                let t = n(0) - 1;
                let b = if raw(1) == 0 { rows } else { raw(1) }.min(rows) - 1;
                if t < b {
                    self.top = t;
                    self.bottom = b;
                    self.cx = 0;
                    self.cy = 0;
                }
            }
            's' if !private => self.saved = (self.cx, self.cy),
            'u' if !private => (self.cx, self.cy) = self.saved,
            'h' | 'l' if private => {
                let on = f == 'h';
                for m in &nums {
                    match m {
                        1 => self.app_cursor = on,
                        47 | 1047 | 1049 => self.alternate(on),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    fn alternate(&mut self, on: bool) {
        if on && self.main.is_none() {
            let blank = vec![vec![BLANK; self.cols]; self.rows];
            let grid = std::mem::replace(&mut self.grid, blank);
            let wrapped = std::mem::replace(&mut self.wrapped, vec![0; self.rows]);
            self.main = Some((grid, wrapped, self.cx, self.cy));
        } else if !on {
            if let Some((grid, wrapped, x, y)) = self.main.take() {
                self.grid = grid;
                self.wrapped = wrapped;
                self.grid.resize(self.rows, vec![BLANK; self.cols]);
                self.wrapped.resize(self.rows, 0);
                for l in &mut self.grid {
                    if l.len() < self.cols {
                        l.resize(self.cols, BLANK);
                    }
                }
                self.cx = x.min(self.cols - 1);
                self.cy = y.min(self.rows - 1);
            }
        }
    }
}

// What the screen waits on.

/// One option of a menu, as the screen shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct MenuOption {
    /// Its number on the screen (`2.`), when the menu numbers them.
    pub n: Option<u32>,
    pub label: String,
}

/// What kind of menu: Claude Code's permission prompt, its trust prompt,
/// or another dialog that waits on a choice.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MenuKind {
    Permission,
    Trust,
    Dialog,
}

impl MenuKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MenuKind::Permission => "permission",
            MenuKind::Trust => "trust",
            MenuKind::Dialog => "dialog",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Menu {
    pub kind: MenuKind,
    /// The line that asks: "Do you want to proceed?".
    pub question: String,
    /// The lines above the options, up to the prompt's frame (at most 12).
    pub header: Vec<String>,
    pub options: Vec<MenuOption>,
    /// The option the cursor is on, from 0.
    pub selected: usize,
    /// The line under the options: "Esc to cancel · Tab to amend".
    pub footer: String,
}

impl Menu {
    pub fn labels(&self) -> Vec<String> {
        self.options.iter().map(|o| o.label.clone()).collect()
    }

    /// What tells one prompt from another: its kind, its options and the
    /// text above them, without spaces (so that a new wrap of the same
    /// prompt is the same prompt).
    pub fn signature(&self) -> String {
        let squeeze = |s: &str| s.chars().filter(|c| !c.is_whitespace() && !matches!(c, '│' | '╎' | '┃' | '╌' | '─' | '|')).collect::<String>();
        let mut s = format!("{}|", self.kind.as_str());
        for h in &self.header {
            s.push_str(&squeeze(h));
        }
        s.push('|');
        s.push_str(&squeeze(&self.question));
        for l in self.labels() {
            s.push('|');
            s.push_str(&squeeze(&l));
        }
        s
    }
}

/// What a screen shows Claude Code waiting on.
#[derive(Clone, Debug, PartialEq)]
pub enum Waits {
    /// A menu: a key chooses.
    Menu(Menu),
    /// Its text prompt with nothing running; `interrupted` after Esc.
    Input { interrupted: bool },
    /// Work in progress (its spinner).
    Busy,
    /// Nothing this reading knows.
    Unknown,
}

const CURSORS: [char; 3] = ['❯', '›', '▶'];

/// A solid rule: the frame of the text prompt or of a prompt's box.
fn is_rule(l: &str) -> bool {
    let t = l.trim();
    t.chars().count() >= 12 && t.chars().all(|c| matches!(c, '─' | '━' | '═'))
}

/// A dashed rule: what Claude Code draws between a command and its
/// question, inside the prompt.
fn is_dashed(l: &str) -> bool {
    let t = l.trim();
    t.chars().count() >= 12 && t.chars().all(|c| matches!(c, '╌' | '┄' | '-' | '╍'))
}

/// The text after a leading cursor mark, and whether there was one.
fn after_cursor(l: &str) -> (bool, &str) {
    let t = l.trim_start();
    for c in CURSORS {
        if let Some(rest) = t.strip_prefix(c) {
            return (true, rest);
        }
    }
    (false, t)
}

/// `2. Yes, and …` → (2, "Yes, and …").
fn numbered(s: &str) -> Option<(u32, &str)> {
    let s = s.trim_start();
    let dot = s.find(". ")?;
    let n: u32 = s[..dot].parse().ok()?;
    (n > 0 && n < 100).then(|| (n, s[dot + 2..].trim()))
}

fn indent(l: &str) -> usize {
    l.chars().take_while(|c| *c == ' ').count()
}

/// Claude Code's spinner line: "✻ Shenaniganing… (26s · ↓ 2.2k tokens)",
/// "· Thinking… (esc to interrupt)".
fn is_spinner(l: &str) -> bool {
    let t = l.trim();
    if t.contains("esc to interrupt") {
        return true;
    }
    let mut cs = t.chars();
    let Some(first) = cs.next() else { return false };
    if first.is_alphanumeric() || first == '❯' || first == '>' {
        return false;
    }
    let rest: String = cs.collect();
    let rest = rest.trim_start();
    let Some(dots) = rest.find('…') else { return false };
    let word = &rest[..dots];
    !word.is_empty() && word.chars().all(|c| c.is_alphabetic() || c == ' ' || c == '-') && word.split(' ').count() <= 3 && rest[dots..].contains('(')
}

/// Where Claude Code's text prompt is: the `❯` line between two rules.
fn input_box(lines: &[String]) -> Option<usize> {
    let rules: Vec<usize> = lines.iter().enumerate().filter(|(_, l)| is_rule(l)).map(|(i, _)| i).collect();
    for w in rules.windows(2).rev() {
        let (a, b) = (w[0], w[1]);
        if b > a + 1 && b - a <= 10 {
            let first = lines[a + 1].trim_start();
            if first.starts_with('❯') || first.starts_with('>') {
                return Some(a + 1);
            }
        }
    }
    None
}

/// Reads what the screen waits on.
pub fn read(lines: &[String]) -> Waits {
    if let Some(m) = menu(lines) {
        return Waits::Menu(m);
    }
    let input = input_box(lines);
    let above = input.unwrap_or(lines.len());
    // The spinner sits right above the prompt: the last few lines of text.
    if lines[..above].iter().rev().filter(|l| !l.trim().is_empty()).take(6).any(|l| is_spinner(l)) {
        return Waits::Busy;
    }
    match input {
        Some(i) => {
            let from = i.saturating_sub(14);
            let interrupted = lines[from..i].iter().any(|l| l.contains("Interrupted · What should Claude do instead?") || l.contains("Interrupted by user"));
            Waits::Input { interrupted }
        }
        None => Waits::Unknown,
    }
}

/// The menu on the screen, if one is: the last line that starts with a
/// cursor mark outside the text prompt, and the options around it.
pub fn menu(lines: &[String]) -> Option<Menu> {
    let input = input_box(lines);
    let c = (0..lines.len()).rev().find(|&i| Some(i) != input && after_cursor(&lines[i]).0 && !after_cursor(&lines[i]).1.trim().is_empty())?;
    // A menu stands below everything else but its footer: a cursor line
    // with the text prompt below it is something the agent printed.
    if input.is_some_and(|i| i > c) {
        return None;
    }
    let (_, rest) = after_cursor(&lines[c]);
    let col = lines[c].len() - rest.trim_start().len();
    let col = lines[c][..col].chars().count();
    // (line, number, label) of each option, top to bottom.
    let mut opts: Vec<(usize, Option<u32>, String)> = Vec::new();
    if let Some((_, _)) = numbered(rest) {
        let num_col = col;
        // Up from the cursor, then down: option lines, and lines of a
        // wrapped label (indented past the number).
        let mut i = c;
        let mut above: Vec<(usize, u32, String)> = Vec::new();
        let mut cont: Vec<String> = Vec::new();
        loop {
            let l = &lines[i];
            let (cur, body) = after_cursor(l);
            let at = l.chars().count() - body.trim_start().chars().count();
            match numbered(body) {
                Some((n, label)) if (cur || at == num_col) => {
                    let mut label = label.to_string();
                    for x in cont.drain(..).rev() {
                        label.push(' ');
                        label.push_str(&x);
                    }
                    above.push((i, n, label));
                }
                _ if !l.trim().is_empty() && indent(l) > num_col && i != c => cont.push(l.trim().to_string()),
                _ => break,
            }
            if i == 0 {
                break;
            }
            i -= 1;
        }
        // What is left in `cont` was above the first option: not a wrap.
        above.reverse();
        for (i, n, l) in above {
            opts.push((i, Some(n), l));
        }
        let mut j = c + 1;
        while j < lines.len() {
            let l = &lines[j];
            let (_, body) = after_cursor(l);
            let at = l.chars().count() - body.trim_start().chars().count();
            match numbered(body) {
                Some((n, label)) if at == num_col || after_cursor(l).0 => opts.push((j, Some(n), label.to_string())),
                _ if !l.trim().is_empty() && indent(l) > num_col && !opts.is_empty() => {
                    let last = opts.last_mut().unwrap();
                    last.2.push(' ');
                    last.2.push_str(l.trim());
                }
                _ => break,
            }
            j += 1;
        }
        // Numbered 1, 2, 3… with no gap.
        if opts.iter().enumerate().any(|(k, o)| o.1 != Some(k as u32 + 1)) {
            return None;
        }
    } else {
        // Unnumbered (the trust prompt): lines whose text starts in the
        // cursor line's text column, next to each other.
        let same = |l: &str| {
            let (cur, body) = after_cursor(l);
            let at = l.chars().count() - body.trim_start().chars().count();
            let _ = cur;
            !body.trim().is_empty() && at == col
        };
        let mut i = c;
        while i > 0 && same(&lines[i - 1]) && !after_cursor(&lines[i - 1]).0 {
            i -= 1;
        }
        let mut j = c;
        while j + 1 < lines.len() && same(&lines[j + 1]) {
            j += 1;
        }
        for (k, l) in lines.iter().enumerate().take(j + 1).skip(i) {
            opts.push((k, None, after_cursor(l).1.trim().to_string()));
        }
        if opts.len() < 2 {
            return None;
        }
    }
    if opts.len() < 2 || opts.len() > 12 {
        return None;
    }
    let first = opts[0].0;
    let last = opts.last().unwrap().0;
    // A menu is the last thing on the screen but its footer; one without
    // numbers says how it is answered (Enter, Esc).
    let below: Vec<&str> = lines[last + 1..].iter().map(|l| l.trim()).filter(|l| !l.is_empty()).collect();
    if below.len() > 3 {
        return None;
    }
    if opts[0].1.is_none() && !below.first().is_some_and(|f| f.to_lowercase().contains("enter") || f.to_lowercase().contains("esc")) {
        return None;
    }
    let selected = opts.iter().position(|o| o.0 == c)?;
    // Labels end where a hint begins: "Yes, and switch to auto mode · auto
    // mode handles these prompts for you" is "Yes, and switch to auto mode".
    let options: Vec<MenuOption> = opts.into_iter().map(|(_, n, l)| MenuOption { n, label: clean_label(&l) }).collect();
    // Above: up to 12 lines, to the frame or a blank line after text.
    let mut header = Vec::new();
    let mut k = first;
    while k > 0 && header.len() < 12 {
        k -= 1;
        let l = &lines[k];
        if is_rule(l) || (input == Some(k)) {
            break;
        }
        if is_dashed(l) {
            continue;
        }
        if l.trim().is_empty() {
            if header.len() >= 4 {
                break;
            }
            continue;
        }
        header.push(l.trim().to_string());
    }
    header.reverse();
    let question = header.iter().rev().find(|l| !l.is_empty()).cloned().unwrap_or_default();
    let footer = lines[last + 1..].iter().map(|l| l.trim()).find(|l| !l.is_empty()).unwrap_or("").to_string();
    let all = format!("{} {}", header.join(" "), options.iter().map(|o| o.label.as_str()).collect::<Vec<_>>().join(" ")).to_lowercase();
    let kind = if all.contains("trust this folder") || all.contains("trust the files") {
        MenuKind::Trust
    } else if options[0].label.starts_with("Yes") && options.iter().any(|o| o.label.starts_with("No")) {
        MenuKind::Permission
    } else {
        MenuKind::Dialog
    };
    Some(Menu { kind, question, header, options, selected, footer })
}

fn clean_label(l: &str) -> String {
    let l = l.split(" · ").next().unwrap_or(l);
    l.split_whitespace().collect::<Vec<_>>().join(" ")
}

// Choosing an option.

/// How much an option grants beyond this one call, in a few words, or
/// `None` for a plain yes or no.
pub fn grants_more(label: &str) -> Option<&'static str> {
    // Claude Code prints "don’t" with a curly apostrophe: read it as "don't".
    let l = label.to_lowercase().replace(['\u{2019}', '\u{2018}'], "'");
    if !l.starts_with("yes") {
        return None;
    }
    if l.contains("auto mode") || l.contains("auto-accept") || l.contains("bypass") {
        Some("switches the agent to auto mode: it stops asking for the rest of the session")
    } else if l.contains("don't ask again") || l.contains("do not ask again") || l.contains("always") {
        Some("stops it asking again for commands like this one")
    } else if l.contains("during this session") || l.contains("all edits") || l.contains("allow all") {
        Some("allows all of this for the rest of the session")
    } else {
        None
    }
}

/// The option a refusal chooses: the first that starts with "No" (`None`:
/// the menu has none, and Esc refuses).
pub fn no_option(m: &Menu) -> Option<usize> {
    m.options.iter().position(|o| {
        let l = o.label.to_lowercase();
        l == "no" || l.starts_with("no,") || l.starts_with("no ") || l.starts_with("no.")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen_of(text: &str) -> Vec<String> {
        text.lines().map(|l| l.trim_end().to_string()).collect()
    }

    /// Maya's screen (11-two-agents-grid.png): four options, the second
    /// wrapped over three lines, a hint after the third.
    const FOUR: &str = "    python3 -m pytest -q 2>&1 | tail -8; git stash list >/dev/null; git diff --stat
╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌
 This command requires approval

 Do you want to proceed?
 ❯ 1. Yes
   2. Yes, and don't ask again for python3 -, \"sed -i 's/assert c.tax() == 82/assert
      c.tax() == 83  # 0.825 rounds half up, not to even/' test_cart.py\", and python3 -m
      pytest -q commands in /tmp/claude-0/worktrees/shop-906aa6cc/cents
   3. Yes, and switch to auto mode · auto mode handles these prompts for you
   4. No

 Esc to cancel · Tab to amend
";

    /// Dev's (51-where-am-i.png): three options, auto mode second.
    const THREE: &str = "Bash command
Tip: auto mode handles these prompts for you — choose \"switch to auto mode\" below
Show the test_cart.py conflict

│ D=/tmp/x/mergecheck
│ git -C \"$D\" diff test_cart.py
╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌
This command requires approval

Do you want to proceed?
❯ 1. Yes
  2. Yes, and switch to auto mode · auto mode handles these prompts for you
  3. No

Esc to cancel · Tab to amend";

    /// The trust prompt (07-lead-zoom-trust.png), no numbers.
    const TRUST: &str = "────────────────────────────────────────────────
Accessing workspace:

/tmp/claude-0/sim/run/dev/data/worktrees/shop-65026ce7/lead

Quick safety check: Is this a project you created or one you trust? (Like your own code,
a well-known open source project, or work from your team). If not, take a moment to
review what's in this folder first.

Claude Code'll be able to read, edit, and execute files here.

Security guide

❯ No, exit
  Yes, I trust this folder

Enter to confirm · Esc to cancel";

    #[test]
    fn reads_the_permission_prompts_the_testers_saw() {
        let Waits::Menu(m) = read(&screen_of(FOUR)) else { panic!() };
        assert_eq!(m.kind, MenuKind::Permission);
        assert_eq!(m.question, "Do you want to proceed?");
        assert_eq!(m.selected, 0);
        assert_eq!(m.options.len(), 4);
        assert_eq!(m.options[0].label, "Yes");
        assert!(m.options[1].label.starts_with("Yes, and don't ask again for python3 -,") && m.options[1].label.ends_with("shop-906aa6cc/cents"), "{:?}", m.options[1]);
        assert_eq!(m.options[2].label, "Yes, and switch to auto mode");
        assert_eq!(m.options[3], MenuOption { n: Some(4), label: "No".into() });
        assert_eq!(no_option(&m), Some(3));
        assert_eq!(m.footer, "Esc to cancel · Tab to amend");
        assert!(grants_more(&m.options[2].label).unwrap().contains("auto mode"));
        assert!(grants_more(&m.options[1].label).unwrap().contains("asking again"));
        // As Claude Code prints it, with a curly apostrophe (Maya's grant went through unchecked).
        assert!(grants_more("Yes, and don\u{2019}t ask again for: python3 -c '*'").unwrap().contains("asking again"));
        assert_eq!(grants_more("Yes"), None);
        assert_eq!(grants_more("No"), None);

        let Waits::Menu(m) = read(&screen_of(THREE)) else { panic!() };
        assert_eq!(m.labels(), ["Yes", "Yes, and switch to auto mode", "No"]);
        assert_eq!(no_option(&m), Some(2));
        assert!(m.header.iter().any(|l| l.contains("git -C")), "{:?}", m.header);
        // The same prompt rewrapped is the same prompt; another is not.
        let other = THREE.replace("diff test_cart.py", "diff cart.py");
        let Waits::Menu(o) = read(&screen_of(&other)) else { panic!() };
        assert_ne!(m.signature(), o.signature());
    }

    #[test]
    fn reads_the_trust_prompt_and_the_text_prompt() {
        let Waits::Menu(m) = read(&screen_of(TRUST)) else { panic!() };
        assert_eq!(m.kind, MenuKind::Trust);
        assert_eq!(m.labels(), ["No, exit", "Yes, I trust this folder"]);
        assert_eq!(m.selected, 0);
        assert_eq!(m.options[0].n, None);
        assert_eq!(no_option(&m), Some(0));

        let rule = "─".repeat(60);
        let idle = format!("● Both check out.\n  ⎿  Interrupted · What should Claude do instead?\n\n✻ Cogitated for 1m 11s · done 7:23 AM\n\n{rule}\n❯ \n{rule}\n  ⏵⏵ accept edits on (shift+tab to cycle)");
        assert_eq!(read(&screen_of(&idle)), Waits::Input { interrupted: true });
        let busy = format!("● Running tests\n\n✻ Shenaniganing… (26s · ↓ 2.2k tokens)\n\n{rule}\n❯ z\n{rule}");
        assert_eq!(read(&screen_of(&busy)), Waits::Busy);
        // A ❯ the agent printed above the prompt is not a menu.
        let printed = format!("❯ 1. Yes\n  2. No\n\n{rule}\n❯ \n{rule}");
        assert_eq!(read(&screen_of(&printed)), Waits::Input { interrupted: false });
    }

    #[test]
    fn the_emulator_follows_redraws() {
        let mut s = Screen::new(40, 6);
        s.feed("one\r\ntwo\r\nthree".as_bytes());
        assert_eq!(&s.lines()[..3], ["one", "two", "three"]);
        // Ink's redraw: erase the lines, go up, write again.
        s.feed(b"\x1b[2K\x1b[1A\x1b[2K\x1b[1A\x1b[2K\x1b[Gnew\r\n");
        assert_eq!(&s.lines()[..2], ["new", ""]);
        s.feed("\x1b[2J\x1b[H❯ 1. Yes\r\n  2. No".as_bytes());
        assert_eq!(&s.lines()[..2], ["❯ 1. Yes", "  2. No"]);
        // Scrolls at the bottom, wraps at the edge.
        let mut s = Screen::new(4, 2);
        s.feed(b"abcdef\r\ngh");
        assert_eq!(s.lines(), ["ef", "gh"]);
        s.feed(b"\x1b[?1h");
        assert!(s.app_cursor());
        s.feed(b"\x1b[?1049h\x1b]0;title\x07x");
        assert_eq!(s.lines(), ["", "  x"]);
        s.feed(b"\x1b[?1049l");
        assert_eq!(s.lines(), ["ef", "gh"]);
    }

    #[test]
    fn a_label_the_terminal_wrapped_is_read_whole() {
        // An option longer than the terminal is wide, printed with no
        // wrapping of its own: the terminal wraps it at the edge.
        let mut s = Screen::new(40, 12);
        let long = "2. Yes, and don't ask again for echo commands in /tmp/work/trees/repo-bb544fb3/freeship";
        s.feed(format!("\x1b[2J\x1b[H Do you want to proceed?\r\n ❯ 1. Yes\r\n   {long}\r\n   3. No\r\n\r\n Esc to cancel · Tab to amend\r\n").as_bytes());
        let m = menu(&s.lines()).expect("a menu");
        assert_eq!(m.labels(), ["Yes", &long[3..], "No"]);
        // The terminal made wider (a zoom) before the agent redraws: the
        // same prompt, the same labels.
        s.resize(60, 12);
        assert_eq!(menu(&s.lines()).expect("a menu").signature(), m.signature());
        // Or narrower (91 columns from 120 when zoomed, say).
        s.resize(30, 12);
        assert_eq!(menu(&s.lines()).expect("a menu").signature(), m.signature());
        // Redrawn shorter: the rows are erased and the wrap goes with them.
        s.feed(b"\x1b[2J\x1b[H ok\r\n");
        assert_eq!(s.lines().len(), 12);
    }
}
