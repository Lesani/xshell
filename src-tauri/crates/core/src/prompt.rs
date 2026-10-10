//! Permission Prompts read from an agent Terminal's screen (capability `agent.prompt`).
//!
//! Hooks and OSC 9 only say that an agent needs you; the options exist only on its screen.
//! [`ScreenModel`] is a headless terminal fed with the Terminal's output, and [`extract`]
//! reads the prompt off its visible rows. Pure: no I/O, no clock.
//!
//! Extraction is generic for Claude Code and Codex and deliberately strict, so a screen it
//! does not understand gives no buttons (a text-only prompt instead, see [`screen_tail`])
//! rather than wrong ones:
//!
//! 1. From the bottom, past blank rows, the dialog's frame and at most [`FOOTER_MAX`] of its
//!    whole key-hint rows (nothing else may follow the options), a block of
//!    numbered option rows (`1. Yes`), each optionally led by a focus marker; a label that
//!    wraps continues on rows indented past its number.
//! 2. The block is accepted only if it is numbered `1..n` with `2 <= n <= 9`, exactly one row
//!    carries the agent's focus marker, and no label carries a checkbox (a multi-select
//!    cannot be answered with one key).
//! 3. The agent's own dialog must be recognised too: Claude Code's question (`Do you want …?`)
//!    directly above the block, inside the dialog (its frame above, or `Esc to cancel`
//!    below); Codex's approval question (`Would you like to …?`) above the block and its
//!    key-hint footer (`… to confirm …`) below.
//!
//! The answer key of option `n` is its index digit, as shown.
//!
//! [`composer`] recognises the agent's chat input (capability `term.submit`): a reply is
//! typed only into a screen that ends with it, with the same strictness. See its
//! documentation.

use crate::agent_status::HookAgent;
use xshell_protocol::msg::{PROMPT_OPTIONS_MAX, PROMPT_OPTION_MAX_CHARS, PROMPT_TEXT_MAX_CHARS};

/// Key-hint rows below the option block that may belong to the dialog.
pub const FOOTER_MAX: usize = 3;
/// Rows above the option block read for the prompt's text (the dialog's frame is looked for
/// further up).
pub const TEXT_ROWS_MAX: usize = 15;
/// Non-blank bottom rows a text-only prompt shows.
pub const TAIL_ROWS: usize = 12;

/// A headless screen fed with a Terminal's output: the visible grid only (no scrollback).
pub struct ScreenModel {
    parser: vt100::Parser,
}

impl ScreenModel {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            parser: vt100::Parser::new(rows.max(1), cols.max(1), 0),
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    /// Follow the PTY's size (the screen reflows nothing; the TUI redraws on SIGWINCH).
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.parser.screen_mut().set_size(rows.max(1), cols.max(1));
    }

    /// `(cols, rows)`.
    pub fn size(&self) -> (u16, u16) {
        let (rows, cols) = self.parser.screen().size();
        (cols, rows)
    }

    /// Whether the program turned bracketed paste on (`CSI ? 2004 h`, off again with
    /// `CSI ? 2004 l` or a reset `ESC c`).
    pub fn bracketed_paste(&self) -> bool {
        self.parser.screen().bracketed_paste()
    }

    /// The visible rows, top first, trailing spaces trimmed.
    pub fn rows(&self) -> Vec<String> {
        let (_, cols) = self.parser.screen().size();
        self.parser
            .screen()
            .rows(0, cols)
            .map(|r| r.trim_end().to_string())
            .collect()
    }
}

/// One answer of a [`Found`] prompt: its label (capped, key hint removed) and the bytes that
/// select it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundOption {
    pub label: String,
    pub keys: Vec<u8>,
}

/// A prompt read off the screen. `text` and the labels are capped for the wire;
/// `fingerprint` identifies the prompt by its full, uncapped text and labels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub text: String,
    pub options: Vec<FoundOption>,
    pub fingerprint: u64,
}

/// Focus markers a selection list draws before the focused option.
fn is_marker(agent: HookAgent, c: char) -> bool {
    match agent {
        HookAgent::Claude => matches!(c, '❯' | '>'),
        HookAgent::Codex => matches!(c, '›' | '>'),
    }
}

fn any_marker(c: char) -> bool {
    matches!(c, '❯' | '›' | '>')
}

/// Box-drawing characters dialogs frame themselves with.
fn is_box(c: char) -> bool {
    ('\u{2500}'..='\u{257F}').contains(&c)
}

/// Checkboxes and radio marks: a list carrying them is a multi-select or a form.
const CHECKBOXES: &[&str] = &[
    "[ ]", "[x]", "[X]", "[✓]", "[✔]", "[•]", "☐", "☑", "☒", "◯", "◉", "○", "●", "◻", "◼", "✓ ",
];

/// Key hints a TUI appends to an option label; meaningless on a phone.
fn strip_key_hint(label: &str) -> &str {
    let t = label.trim_end();
    let Some(body) = t.strip_suffix(')') else {
        return t;
    };
    let Some(open) = body.rfind('(') else {
        return t;
    };
    let hint = &body[open + 1..];
    let key = |k: &str| {
        matches!(
            k,
            "esc" | "enter" | "tab" | "shift+tab" | "space" | "return" | "backspace"
        ) || (k.len() == 1 && k.chars().all(|c| c.is_ascii_alphanumeric()))
            || k.strip_prefix("ctrl+")
                .or_else(|| k.strip_prefix("alt+"))
                .is_some_and(|r| r.len() == 1 && r.chars().all(|c| c.is_ascii_alphanumeric()))
    };
    if !hint.is_empty() && key(&hint.to_ascii_lowercase()) {
        body[..open].trim_end()
    } else {
        t
    }
}

/// Control characters (C0, DEL, C1) become spaces, runs of whitespace one space, and the
/// ends are trimmed.
fn clean(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = false;
    for c in s.chars() {
        if c.is_whitespace() || c.is_control() {
            space = true;
        } else {
            if space && !out.is_empty() {
                out.push(' ');
            }
            space = false;
            out.push(c);
        }
    }
    out
}

/// `s` cut to at most `max` characters; a cut ends in `…`.
fn cap(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// One screen row with the dialog's side frame (`│ … │`) removed.
struct Row {
    /// The row inside the frame.
    text: String,
    /// Leading spaces of `text`.
    indent: usize,
    blank: bool,
    /// Only box-drawing characters and spaces, with at least one of them.
    border: bool,
    opt: Option<Opt>,
}

/// An option row: `[marker] n. label`.
struct Opt {
    marker: Option<char>,
    number: u32,
    /// Column of the number's first digit.
    num_col: usize,
    label: String,
}

impl Row {
    fn new(raw: &str) -> Row {
        // A side frame: `│` (or another vertical box character) at either end.
        let vertical = |c: char| matches!(c, '│' | '┃' | '║' | '▏' | '▕');
        // A frame's top or bottom: box characters, not only its sides.
        let border = raw.chars().any(|c| is_box(c) && !vertical(c))
            && raw.chars().all(|c| c == ' ' || is_box(c));
        let mut s = raw.trim_end();
        if let Some(rest) = s.strip_suffix(vertical) {
            s = rest.trim_end();
        }
        let lead = s.len() - s.trim_start().len();
        let mut text = s.to_string();
        if s.trim_start().starts_with(vertical) {
            let rest: String = s.trim_start().chars().skip(1).collect();
            text = format!("{}{}", " ".repeat(lead), rest);
        }
        let indent = text.chars().take_while(|c| *c == ' ').count();
        let blank = text.trim().is_empty();
        let opt = if border || blank {
            None
        } else {
            parse_option(&text)
        };
        Row {
            text,
            indent,
            blank,
            border,
            opt,
        }
    }
}

fn parse_option(text: &str) -> Option<Opt> {
    let chars: Vec<char> = text.chars().collect();
    let mut i = chars.iter().take_while(|c| **c == ' ').count();
    let mut marker = None;
    if i < chars.len() && any_marker(chars[i]) {
        marker = Some(chars[i]);
        i += 1;
        while i < chars.len() && chars[i] == ' ' {
            i += 1;
        }
    }
    let num_col = i;
    let digits: String = chars[i..]
        .iter()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if digits.is_empty() || digits.len() > 2 {
        return None;
    }
    i += digits.len();
    if chars.get(i) != Some(&'.') || chars.get(i + 1) != Some(&' ') {
        return None;
    }
    let label: String = chars[i + 1..].iter().collect();
    if label.trim().is_empty() {
        return None;
    }
    Some(Opt {
        marker,
        number: digits.parse().ok()?,
        num_col,
        label: label.trim().to_string(),
    })
}

/// A row that continues the label of an option numbered at `num_col`.
fn continues(r: &Row, num_col: usize) -> bool {
    !r.blank && !r.border && r.opt.is_none() && r.indent > num_col
}

/// The key hints a Claude Code dialog's footer is made of, lowercased. A footer row is one
/// or more of them joined by ` · `, and nothing else.
const CLAUDE_HINTS: &[&str] = &[
    "esc to cancel",
    "tab to amend",
    "ctrl+e to explain",
    "enter to select",
    "enter to confirm",
    "↑/↓ to navigate",
    "↑↓ to navigate",
    "tab to add additional instructions",
];

/// The same for Codex: its key-hint lines, whole.
const CODEX_HINTS: &[&str] = &[
    "press enter to confirm or esc to cancel",
    "press enter to select or esc to cancel",
    "↑/↓ to navigate",
];

/// A whole key-hint row of `agent`'s dialog (`line` cleaned, frame removed): only known
/// hints, joined by ` · `. A hint inside other text (a command's prompt) is not one.
fn is_footer(agent: HookAgent, line: &str) -> bool {
    let hints = match agent {
        HookAgent::Claude => CLAUDE_HINTS,
        HookAgent::Codex => CODEX_HINTS,
    };
    let l = line.to_lowercase();
    !l.is_empty() && l.split(" · ").all(|seg| hints.contains(&seg.trim()))
}

/// FNV-1a over the parts, each followed by a separator byte.
fn fingerprint<'a>(parts: impl IntoIterator<Item = &'a str>) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for p in parts {
        for b in p.bytes().chain(std::iter::once(0xff)) {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    h
}

/// The lines of a prompt's text capped to [`PROMPT_TEXT_MAX_CHARS`] in all: the first lines
/// that fit, and always the last one (the question).
fn cap_text(lines: &[String]) -> String {
    let Some((last, first)) = lines.split_last() else {
        return String::new();
    };
    let last = cap(last, PROMPT_TEXT_MAX_CHARS);
    let mut left = PROMPT_TEXT_MAX_CHARS - last.chars().count();
    let mut kept: Vec<String> = Vec::new();
    for l in first {
        // The line and its newline.
        let need = l.chars().count() + 1;
        if need <= left {
            kept.push(l.clone());
            left -= need;
        } else {
            if left >= 2 {
                kept.push(cap(l, left - 1));
            }
            break;
        }
    }
    kept.push(last);
    kept.join("\n")
}

/// The Permission Prompt `agent`'s TUI shows on `rows` (a [`ScreenModel`]'s visible rows),
/// if it is one this module recognises (see the module documentation).
pub fn extract(agent: HookAgent, rows: &[String]) -> Option<Found> {
    let r: Vec<Row> = rows.iter().map(|s| Row::new(s)).collect();
    // The lowest option row, past blank rows and the footer (plus the rows its own label
    // may have wrapped onto).
    let mut lowest = None;
    let mut seen = 0;
    for i in (0..r.len()).rev() {
        if r[i].blank {
            continue;
        }
        if r[i].opt.is_some() {
            lowest = Some(i);
            break;
        }
        seen += 1;
        if seen > FOOTER_MAX + 3 {
            return None;
        }
    }
    let lowest = lowest?;
    let mut end = lowest;
    let lowest_col = r[lowest].opt.as_ref()?.num_col;
    while end + 1 < r.len() && continues(&r[end + 1], lowest_col) {
        end += 1;
    }
    // Below the block only the dialog's own key hints and frame may follow: any other output
    // (a command's, an input prompt) means the dialog is not what the screen ends with.
    let mut footer: Vec<String> = Vec::new();
    for row in r[end + 1..].iter().filter(|x| !x.blank) {
        if row.border {
            continue;
        }
        let line = clean(&row.text);
        if !is_footer(agent, &line) {
            return None;
        }
        footer.push(line);
    }
    if footer.len() > FOOTER_MAX {
        return None;
    }
    // Up to the first row that is neither an option nor a continuation; the block starts at
    // its first option row.
    let mut top = lowest;
    while top > 0 && !r[top - 1].blank && !r[top - 1].border {
        top -= 1;
    }
    while r[top].opt.is_none() {
        top += 1;
    }
    // Options, top first, with their wrapped rows joined.
    let mut opts: Vec<(Option<char>, u32, String)> = Vec::new();
    let mut col = 0;
    for row in &r[top..=end] {
        match &row.opt {
            Some(o) => {
                col = o.num_col;
                opts.push((o.marker, o.number, o.label.clone()));
            }
            None if continues(row, col) => {
                let last = opts.last_mut()?;
                last.2.push(' ');
                last.2.push_str(row.text.trim());
            }
            None => return None,
        }
    }
    let n = opts.len();
    if !(2..=PROMPT_OPTIONS_MAX.min(9)).contains(&n) {
        return None;
    }
    if opts
        .iter()
        .enumerate()
        .any(|(i, (_, num, _))| *num as usize != i + 1)
    {
        return None;
    }
    let marked: Vec<char> = opts.iter().filter_map(|o| o.0).collect();
    if marked.len() != 1 || !is_marker(agent, marked[0]) {
        return None;
    }
    if opts
        .iter()
        .any(|(_, _, l)| CHECKBOXES.iter().any(|c| l.contains(c)))
    {
        return None;
    }
    // The text: upward from the block to a border or two blank rows, the nearest
    // TEXT_ROWS_MAX rows of it. Whether a border (the dialog's frame) ends it is evidence.
    let mut text: Vec<(usize, String)> = Vec::new();
    let mut blanks = 0;
    let mut framed = false;
    let mut i = top;
    while i > 0 {
        i -= 1;
        let row = &r[i];
        if row.border {
            framed = true;
            break;
        }
        if row.blank {
            blanks += 1;
            if blanks >= 2 {
                break;
            }
            continue;
        }
        blanks = 0;
        let line = clean(&row.text.replace(is_box, " "));
        if !line.is_empty() && top - i <= TEXT_ROWS_MAX {
            text.push((i, line));
        }
    }
    text.reverse();
    if !evidence(agent, &text, top, framed, &footer) {
        return None;
    }
    let lines: Vec<String> = text.into_iter().map(|(_, l)| l).collect();
    let labels: Vec<String> = opts
        .iter()
        .map(|(_, _, l)| clean(strip_key_hint(&clean(l))))
        .collect();
    let fp = fingerprint(
        lines
            .iter()
            .map(String::as_str)
            .chain(std::iter::once("\u{0}options"))
            .chain(labels.iter().map(String::as_str)),
    );
    Some(Found {
        text: cap_text(&lines),
        options: labels
            .iter()
            .enumerate()
            .map(|(i, l)| FoundOption {
                label: cap(l, PROMPT_OPTION_MAX_CHARS),
                keys: vec![b'1' + i as u8],
            })
            .collect(),
        fingerprint: fp,
    })
}

/// The agent's own dialog around the block (module documentation, step 3). `text` holds the
/// rows above the block (row index, cleaned line), top first.
fn evidence(
    agent: HookAgent,
    text: &[(usize, String)],
    top: usize,
    framed: bool,
    footer: &[String],
) -> bool {
    // The paragraph a line starts: it and the lines right below it.
    let paragraph_from = |k: usize| -> String {
        let mut p = text[k].1.clone();
        let mut row = text[k].0;
        for (r, l) in &text[k + 1..] {
            if *r != row + 1 {
                break;
            }
            p.push(' ');
            p.push_str(l);
            row = *r;
        }
        p
    };
    match agent {
        HookAgent::Claude => {
            // The question directly above the block (one blank row between at most).
            let Some((last_row, _)) = text.last() else {
                return false;
            };
            if top - last_row > 2 {
                return false;
            }
            let Some(k) = (0..text.len()).rev().find(|&k| {
                text[k].1.starts_with("Do you want ")
                    && text[k..].windows(2).all(|w| w[1].0 == w[0].0 + 1)
            }) else {
                return false;
            };
            // Inside the dialog: its frame above, or its cancel hint below.
            let hint = footer
                .iter()
                .any(|f| f.to_ascii_lowercase().contains("esc to cancel"));
            paragraph_from(k).ends_with('?') && (framed || hint)
        }
        HookAgent::Codex => {
            let asks = (0..text.len()).any(|k| {
                text[k].1.starts_with("Would you like to ") && paragraph_from(k).contains('?')
            });
            asks && footer.iter().any(|f| f.contains(" to confirm"))
        }
    }
}

/// Rows below a chat composer that may be its footer (key hints, mode, status line).
pub const COMPOSER_FOOTER_MAX: usize = 4;
/// The shortest rule (or box top) a Claude Code composer is framed by.
const COMPOSER_RULE_MIN: usize = 20;

/// The prompt mark a composer's first row starts with.
fn is_composer_mark(agent: HookAgent, c: char) -> bool {
    match agent {
        HookAgent::Claude => matches!(c, '❯' | '>'),
        HookAgent::Codex => c == '›',
    }
}

/// A row made only of `─`, at least [`COMPOSER_RULE_MIN`] of them (leading spaces allowed).
fn is_rule(row: &str) -> bool {
    let t = row.trim();
    t.chars().count() >= COMPOSER_RULE_MIN && t.chars().all(|c| c == '─')
}

/// A round box's top or bottom: `╭─…─╮` or `╰─…─╯`.
fn is_box_edge(row: &str, top: bool) -> bool {
    let (l, r) = if top { ('╭', '╮') } else { ('╰', '╯') };
    let t = row.trim();
    let mut c = t.chars();
    c.next() == Some(l)
        && c.next_back() == Some(r)
        && t.chars().count() >= COMPOSER_RULE_MIN
        && c.all(|c| c == '─')
}

/// The composer's first row: the agent's prompt mark at most two columns in, then a space
/// or nothing (an empty input). Never an option row (`❯ 1. Yes` is a dialog's focus).
fn is_composer_head(agent: HookAgent, row: &str) -> bool {
    let lead = row.chars().take_while(|c| *c == ' ').count();
    let rest: Vec<char> = row.chars().skip(lead).collect();
    lead <= 2
        && rest.first().is_some_and(|c| is_composer_mark(agent, *c))
        && rest.get(1).is_none_or(|c| *c == ' ')
        && parse_option(row).is_none()
}

/// A row continuing a multi-line or wrapped input: blank, or indented by at least two
/// columns, and not an option row.
fn is_composer_continuation(row: &str) -> bool {
    row.trim().is_empty() || (row.starts_with("  ") && parse_option(row).is_none())
}

/// A composer footer row's parts: split at ` · ` and at runs of two or more spaces, cleaned
/// and lowercased.
fn footer_segments(row: &str) -> Vec<String> {
    let mut out = Vec::new();
    for part in row.split(" · ") {
        let mut cur = String::new();
        let mut spaces = 0;
        for c in part.chars() {
            if c == ' ' {
                spaces += 1;
                continue;
            }
            if spaces >= 2 && !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            } else if spaces == 1 && !cur.is_empty() {
                cur.push(' ');
            }
            spaces = 0;
            cur.push(c);
        }
        if !cur.is_empty() {
            out.push(cur);
        }
    }
    out.iter().map(|s| clean(s).to_lowercase()).collect()
}

/// `seg` is `<n>%<rest>` with a whole number `n` up to 100.
fn percent_then(seg: &str, rest: &str) -> bool {
    seg.strip_suffix(rest)
        .and_then(|n| n.strip_suffix('%'))
        .is_some_and(|n| {
            !n.is_empty()
                && n.len() <= 3
                && n.chars().all(|c| c.is_ascii_digit())
                && n.parse::<u32>().is_ok_and(|n| n <= 100)
        })
}

/// One part of Claude Code's footer under its input (2.1.296): the shortcuts hint, the
/// clear and exit hints, the permission mode, the context left.
fn is_claude_footer_segment(seg: &str) -> bool {
    const WHOLE: &[&str] = &[
        "? for shortcuts",
        "esc to clear",
        "esc again to clear",
        "press ctrl-c again to exit",
        "press ctrl+c again to exit",
        "run /compact to compact & continue",
    ];
    const MODES: &[&str] = &[
        "accept edits on",
        "plan mode on",
        "auto mode on",
        "bypass permissions on",
    ];
    if WHOLE.contains(&seg) {
        return true;
    }
    // `⏵⏵ accept edits on (shift+tab to cycle)`: the mode's symbol, its name, the hint.
    let mode = seg.trim_start_matches(|c: char| !c.is_ascii_alphanumeric() && c != ' ');
    let mode = mode.trim_start();
    let mode = mode.strip_suffix(" (shift+tab to cycle)").unwrap_or(mode);
    if MODES.contains(&mode) {
        return true;
    }
    if let Some(inner) = seg
        .strip_prefix("context low (")
        .and_then(|r| r.strip_suffix(" remaining)"))
    {
        return percent_then(inner, "");
    }
    percent_then(seg, " until auto-compact") || percent_then(seg, " context used")
}

/// One part of Codex's footer under its composer (0.154): `<key> <hint>` for its known
/// hints (`? for shortcuts`, `tab to queue message`, `esc again to edit previous message`,
/// `⏎ send`…), and the context left.
fn is_codex_footer_segment(seg: &str) -> bool {
    const KEYS: &[&str] = &[
        "?",
        "tab",
        "esc",
        "esc esc",
        "enter",
        "⏎",
        "/",
        "!",
        "@",
        "ctrl + c",
        "ctrl+c",
        "⌃c",
        "ctrl + j",
        "ctrl+j",
        "⌃j",
        "ctrl + t",
        "ctrl+t",
        "⌃t",
        "ctrl + g",
        "ctrl+g",
        "⌃g",
        "shift + enter",
        "shift+enter",
        "shift + tab",
        "shift+tab",
    ];
    const HINTS: &[&str] = &[
        "for shortcuts",
        "to edit previous message",
        "to queue message",
        "to queue",
        "to submit message",
        "to interrupt",
        "for commands",
        "for shell commands",
        "for newline",
        "for file paths",
        "to edit in external editor",
        "to view transcript",
        "to change mode",
        "to quit",
        "send",
        "newline",
        "transcript",
        "quit",
    ];
    if percent_then(seg, " context left") || percent_then(seg, " context used") {
        return true;
    }
    HINTS.iter().any(|h| {
        seg.strip_suffix(h)
            .and_then(|k| k.strip_suffix(' '))
            .map(|k| k.strip_suffix(" again").unwrap_or(k))
            .is_some_and(|k| KEYS.contains(&k))
    })
}

/// A whole row of `agent`'s composer footer: made only of its known parts. Anything else
/// below the input (a dialog's hints, a question, a list, a command's output, a custom
/// status line) means the screen is not just the composer.
fn is_composer_footer(agent: HookAgent, row: &str) -> bool {
    let segs = footer_segments(row);
    !segs.is_empty()
        && segs.iter().all(|s| match agent {
            HookAgent::Claude => is_claude_footer_segment(s),
            HookAgent::Codex => is_codex_footer_segment(s),
        })
}

/// Whether `agent`'s chat composer is what the screen (a [`ScreenModel`]'s rows) ends with,
/// so typed text and Enter reach the chat input and nothing else. Strict like [`extract`]:
/// any screen it does not understand is not a composer.
///
/// - **Claude Code** (2.x): the input between two equal rules of `─` (its box without
///   sides), or, as 1.x drew it, inside a round box `╭─╮ │ … │ ╰─╯`. The row right below the
///   top edge starts with the prompt mark `❯` (or `>`), the rows down to the bottom edge
///   continue it (blank or indented), and below the bottom edge come at most
///   [`COMPOSER_FOOTER_MAX`] footer rows. A dialog has its title under its top edge, the
///   bash mode shows `!` instead of the mark.
/// - **Codex**: the lowest row starting with the prompt mark `›`, with the composer's blank
///   padding row directly above it, continued by indented rows; below it only blank rows
///   and one to [`COMPOSER_FOOTER_MAX`] footer rows, after a blank row. An approval or a
///   selection list puts its focus (`› 1. …`) lowest, so it is never one.
///
/// A footer row must be made only of the agent's own footer parts (its shortcut and mode
/// hints, the context left: see `is_claude_footer_segment` and `is_codex_footer_segment`);
/// any other row below the input (a dialog's or picker's hints, a `[y/N]` question, a
/// custom status line) refuses. Unknown is never the composer.
///
/// Neither accepts an option row anywhere in the input, or anything that is not on the
/// screen's last rows.
pub fn composer(agent: HookAgent, rows: &[String]) -> bool {
    composer_input(agent, rows).is_some()
}

/// The input rows of `agent`'s chat composer when [`composer`] recognises it (the prompt
/// mark's row first, without a box's sides), else `None`.
pub fn composer_input(agent: HookAgent, rows: &[String]) -> Option<Vec<String>> {
    let r: Vec<&str> = rows.iter().map(|s| s.trim_end()).collect();
    let last = r.iter().rposition(|s| !s.is_empty())?;
    match agent {
        HookAgent::Claude => claude_composer(&r[..=last]),
        HookAgent::Codex => codex_composer(&r[..=last]),
    }
}

/// The attachment chip both agents put in their composer for an attached image:
/// `[Image #n]`.
pub const IMAGE_CHIP: &str = "[Image #";

/// How many images are attached in `agent`'s chat composer: the [`IMAGE_CHIP`]s in its
/// input rows. `None` when the screen does not end with the composer, such as while Claude
/// Code still reads a pasted image (its footer then says `Pasting…`, no footer hint).
pub fn composer_images(agent: HookAgent, rows: &[String]) -> Option<usize> {
    composer_input(agent, rows).map(|input| {
        input
            .iter()
            .map(|row| row.matches(IMAGE_CHIP).count())
            .sum()
    })
}

fn claude_composer(r: &[&str]) -> Option<Vec<String>> {
    let agent = HookAgent::Claude;
    // The bottom edge, past the footer.
    let mut footer = 0;
    let mut bottom = r.len();
    loop {
        if bottom == 0 {
            return None;
        }
        bottom -= 1;
        let row = r[bottom];
        if is_rule(row) || is_box_edge(row, false) {
            break;
        }
        if !row.is_empty() {
            footer += 1;
            if footer > COMPOSER_FOOTER_MAX || !is_composer_footer(agent, row) {
                return None;
            }
        }
    }
    let boxed = is_box_edge(r[bottom], false);
    // A boxed input's rows without their sides.
    let inner = |row: &str| -> Option<String> {
        if !boxed {
            return Some(row.to_string());
        }
        let t = row.trim();
        let t = t.strip_prefix('│')?.strip_suffix('│')?;
        Some(t.trim_end().to_string())
    };
    // The top edge, up through the input's rows.
    let mut top = bottom;
    loop {
        if top == 0 {
            return None;
        }
        top -= 1;
        let row = r[top];
        let edge = if boxed {
            is_box_edge(row, true)
        } else {
            is_rule(row)
        };
        if edge {
            break;
        }
    }
    if top + 1 >= bottom {
        return None;
    }
    if !boxed && r[top].trim() != r[bottom].trim() {
        return None;
    }
    if boxed && r[top].trim().chars().count() != r[bottom].trim().chars().count() {
        return None;
    }
    let head = inner(r[top + 1])?;
    let head = if boxed {
        head.trim_start_matches(' ').to_string()
    } else {
        head
    };
    if !is_composer_head(agent, &head) {
        return None;
    }
    let mut input = vec![head];
    for row in &r[top + 2..bottom] {
        let s = inner(row)?;
        let s = if boxed {
            s.strip_prefix(' ').unwrap_or(&s).to_string()
        } else {
            s
        };
        if !is_composer_continuation(&s) {
            return None;
        }
        input.push(s);
    }
    Some(input)
}

fn codex_composer(r: &[&str]) -> Option<Vec<String>> {
    let agent = HookAgent::Codex;
    let head = r.iter().rposition(|row| {
        let t = row.trim_start();
        t.chars().next().is_some_and(|c| is_composer_mark(agent, c))
    })?;
    if !is_composer_head(agent, r[head]) {
        return None;
    }
    // The composer's top padding.
    if head == 0 || !r[head - 1].is_empty() {
        return None;
    }
    // Its continuation rows, then a blank row and the footer.
    let mut i = head + 1;
    while i < r.len() && !r[i].is_empty() {
        if !is_composer_continuation(r[i]) {
            return None;
        }
        i += 1;
    }
    // Codex's frame is weaker evidence than Claude Code's rules: its footer must be there.
    let footer: Vec<&&str> = r[i..].iter().filter(|row| !row.is_empty()).collect();
    (!footer.is_empty()
        && footer.len() <= COMPOSER_FOOTER_MAX
        && footer.iter().all(|row| is_composer_footer(agent, row)))
    .then(|| r[head..i].iter().map(|row| row.to_string()).collect())
}

/// The text of a text-only prompt: the bottom [`TAIL_ROWS`] non-blank rows, box-drawing
/// characters removed, whitespace collapsed, capped to [`PROMPT_TEXT_MAX_CHARS`] keeping
/// the bottom.
pub fn screen_tail(rows: &[String]) -> String {
    let mut lines: Vec<String> = rows
        .iter()
        .rev()
        .map(|r| clean(&r.replace(is_box, " ")))
        .filter(|l| !l.is_empty())
        .take(TAIL_ROWS)
        .collect();
    lines.reverse();
    let mut kept: Vec<String> = Vec::new();
    let mut left = PROMPT_TEXT_MAX_CHARS;
    for l in lines.iter().rev() {
        let n = l.chars().count() + usize::from(!kept.is_empty());
        if n <= left {
            kept.push(l.clone());
            left -= n;
        } else {
            if kept.is_empty() {
                // One line longer than the cap: its end.
                let tail: String = l.chars().skip(l.chars().count() - (left - 1)).collect();
                kept.push(format!("…{tail}"));
            }
            break;
        }
    }
    kept.reverse();
    kept.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(s: &str) -> Vec<String> {
        s.lines().map(str::to_string).collect()
    }

    const CLAUDE: &str = "\
────────────────────────────────────────
 Bash command

   ls -la /tmp
   List files in /tmp

 Do you want to proceed?
 ❯ 1. Yes
   2. Yes, and don't ask again for ls commands in /home/u/proj
   3. No, and tell Claude what to do differently (esc)

 Esc to cancel";

    const CODEX: &str = "\
  Would you like to run the following command?

  $ ls -la /tmp

› 1. Yes, proceed (y)
  2. Yes, and don't ask again for this command in this session (a)
  3. No, and tell Codex what to do differently (esc)

  Press enter to confirm or esc to cancel";

    fn labels(f: &Found) -> Vec<&str> {
        f.options.iter().map(|o| o.label.as_str()).collect()
    }

    #[test]
    fn claude_dialog_is_read() {
        let f = extract(HookAgent::Claude, &rows(CLAUDE)).expect("a prompt");
        assert_eq!(
            labels(&f),
            [
                "Yes",
                "Yes, and don't ask again for ls commands in /home/u/proj",
                "No, and tell Claude what to do differently"
            ]
        );
        assert_eq!(
            f.text,
            "Bash command\nls -la /tmp\nList files in /tmp\nDo you want to proceed?"
        );
        let keys: Vec<&[u8]> = f.options.iter().map(|o| o.keys.as_slice()).collect();
        assert_eq!(keys, [b"1", b"2", b"3"]);
        // Codex's markers and evidence are not Claude's.
        assert_eq!(extract(HookAgent::Codex, &rows(CLAUDE)), None);
    }

    #[test]
    fn codex_dialog_is_read() {
        let f = extract(HookAgent::Codex, &rows(CODEX)).expect("a prompt");
        assert_eq!(
            labels(&f),
            [
                "Yes, proceed",
                "Yes, and don't ask again for this command in this session",
                "No, and tell Codex what to do differently"
            ]
        );
        assert!(f
            .text
            .starts_with("Would you like to run the following command?"));
        assert!(f.text.contains("$ ls -la /tmp"));
        assert_eq!(extract(HookAgent::Claude, &rows(CODEX)), None);
        // Without its key-hint footer it is not Codex's dialog.
        let cut = CODEX.rsplit_once("\n\n").unwrap().0;
        assert_eq!(extract(HookAgent::Codex, &rows(cut)), None);
    }

    #[test]
    fn framed_dialog_and_wrapped_labels() {
        let s = "\
╭──────────────────────────────────────╮
│ Edit file                            │
│ src/main.rs                          │
│                                      │
│ Do you want to make this edit to     │
│ main.rs?                             │
│ ❯ 1. Yes                             │
│   2. Yes, and don't ask again this   │
│      session (shift+tab)             │
│   3. No, and tell Claude what to do  │
│      differently (esc)               │
╰──────────────────────────────────────╯";
        let f = extract(HookAgent::Claude, &rows(s)).expect("a prompt");
        assert_eq!(
            labels(&f),
            [
                "Yes",
                "Yes, and don't ask again this session",
                "No, and tell Claude what to do differently"
            ]
        );
        assert_eq!(
            f.text,
            "Edit file\nsrc/main.rs\nDo you want to make this edit to\nmain.rs?"
        );
    }

    #[test]
    fn labels_drop_key_hints() {
        for (raw, want) in [
            ("No (esc)", "No"),
            ("Yes, proceed (y)", "Yes, proceed"),
            ("Always (a)", "Always"),
            ("No (n)", "No"),
            ("Auto-accept (shift+tab)", "Auto-accept"),
            ("Next (tab)", "Next"),
            ("Explain (ctrl+e)", "Explain"),
            ("Run it (ESC)", "Run it"),
            ("Use (npm run build)", "Use (npm run build)"),
            ("Open (README.md)", "Open (README.md)"),
            ("Pick (1)", "Pick"),
            ("()", "()"),
            ("Yes", "Yes"),
        ] {
            assert_eq!(strip_key_hint(raw), want, "{raw}");
        }
    }

    /// Screens that must never give buttons, written without the extractor's templates: a
    /// shell, prose, lists printed by commands, forms and multi-selects.
    #[test]
    fn unknown_screens_are_not_prompts() {
        let numbered_prose = "\
⏺ Here is the plan:

  1. Read the config
  2. Fix the parser
  3. Run the tests

> ";
        let marked_prose = "\
 I can do one of these. Do you want me to continue?
 > 1. Refactor the module
   2. Leave it as it is
";
        let command_list = "\
$ cat steps.txt
Do you want to proceed?
❯ 1. build
  2. test
  3. deploy
$ ";
        let two_markers = CLAUDE.replace("   2. Yes,", " ❯ 2. Yes,");
        let gap = CLAUDE.replace("3. No,", "4. No,");
        let from_zero = CLAUDE
            .replace("1. Yes\n", "0. Yes\n")
            .replace("2. Yes,", "1. Yes,")
            .replace("3. No,", "2. No,");
        let one_option = " Do you want to proceed?\n ❯ 1. Yes\n\n Esc to cancel";
        let multi = "\
 Which checks should run?
 ❯ 1. [ ] Lint
   2. [x] Tests
   3. [ ] Build

 Enter to select · ↑/↓ to navigate · Esc to cancel";
        let radio = "\
 Do you want to enable these?
 ❯ 1. ◉ Lint
   2. ◯ Tests

 Esc to cancel";
        let elicitation = "\
╭──────────────────────────────────────────╮
│ MCP server \"tracker\" requests input      │
│                                          │
│ Project name: █                          │
│ Visibility:   ❯ public  private          │
│                                          │
│ Enter to submit · Esc to decline         │
╰──────────────────────────────────────────╯";
        let old_dialog = format!(
            "{CLAUDE}\n⏺ Bash(ls -la /tmp)\n  ⎿ total 0\n  ⎿ drwx  2 u u .\n  ⎿ drwx 20 u u ..\n> "
        );
        let shell = "$ ls\n1. notes.txt\n2. todo.txt\n$ ";
        let no_question = CLAUDE.replace("Do you want to proceed?", "Choose wisely.");
        for (name, s) in [
            ("numbered prose", numbered_prose.to_string()),
            ("marked prose", marked_prose.to_string()),
            ("command list", command_list.to_string()),
            ("two markers", two_markers),
            ("gap", gap),
            ("from zero", from_zero),
            ("one option", one_option.to_string()),
            ("multi-select", multi.to_string()),
            ("radio", radio.to_string()),
            ("elicitation", elicitation.to_string()),
            ("old dialog", old_dialog),
            ("shell", shell.to_string()),
            ("no question", no_question),
            ("idle", "> \n? for shortcuts".to_string()),
            ("empty", String::new()),
        ] {
            for agent in [HookAgent::Claude, HookAgent::Codex] {
                assert_eq!(extract(agent, &rows(&s)), None, "{name} ({agent:?})");
            }
        }
        // A Codex-looking list printed by a command, without Codex's footer.
        let printed = "\
Would you like to run the following command?
› 1. Yes
  2. No
$ ";
        assert_eq!(extract(HookAgent::Codex, &rows(printed)), None);
        // Key hints below the dialog still count; anything else after it does not.
        let footer3 = format!("{CLAUDE}\n Tab to amend\n ctrl+e to explain");
        assert!(extract(HookAgent::Claude, &rows(&footer3)).is_some());
        let joined = CLAUDE.replace(" Esc to cancel", " Esc to cancel · Tab to amend");
        assert!(extract(HookAgent::Claude, &rows(&joined)).is_some());
        // Another agent's hint line is not this one's.
        let crossed = CLAUDE.replace(" Esc to cancel", " Press enter to confirm or esc to cancel");
        assert_eq!(extract(HookAgent::Claude, &rows(&crossed)), None);
        let codex_ok = CODEX.replace("\n\n  Press", "\n  ↑/↓ to navigate\n\n  Press");
        assert!(extract(HookAgent::Codex, &rows(&codex_ok)).is_some());
        for (agent, dialog) in [(HookAgent::Claude, CLAUDE), (HookAgent::Codex, CODEX)] {
            for tail in [
                "\n$ ",
                "\n> ",
                "\nbuild finished",
                "\n⏺ Bash(ls)\n  ⎿ total 0",
                "\n\nsome output\nmore output",
            ] {
                let s = format!("{dialog}{tail}");
                assert_eq!(extract(agent, &rows(&s)), None, "{agent:?} + {tail:?}");
            }
            // Every hint fragment inside other output: a command's or a later input prompt.
            for hint in CLAUDE_HINTS.iter().chain(CODEX_HINTS) {
                for row in [
                    format!("Build finished; {hint} >"),
                    format!("> {hint}"),
                    format!("{hint} later"),
                    format!("Esc to cancel · {hint} or not"),
                ] {
                    let s = format!("{dialog}\n{row}");
                    assert_eq!(extract(agent, &rows(&s)), None, "{agent:?} + {row:?}");
                }
            }
            let s = format!("{dialog}\nBuild finished; enter a number to select a target >");
            assert_eq!(extract(agent, &rows(&s)), None);
            // A row between the options and the hints too.
            let (head, hint) = dialog.rsplit_once("\n\n").unwrap();
            let s = format!("{head}\nstray output\n\n{hint}");
            assert_eq!(extract(agent, &rows(&s)), None, "{agent:?} stray row");
        }
    }

    #[test]
    fn caps_and_no_control_chars() {
        let long = "x\u{1D11E}".repeat(400);
        let mut s = String::new();
        for i in 0..12 {
            s.push_str(&format!(" line {i} {long}\u{7}\u{9b}\n"));
        }
        s.push_str(&format!(" Do you want to proceed? {}?\n", "x".repeat(150)));
        for i in 1..=9 {
            let m = if i == 1 { "❯" } else { " " };
            s.push_str(&format!(" {m} {i}. option {i} {long}\u{1}\n"));
        }
        s.push_str("\n Esc to cancel");
        let f = extract(HookAgent::Claude, &rows(&s)).expect("a prompt");
        assert_eq!(f.options.len(), 9);
        assert!(f.text.chars().count() <= PROMPT_TEXT_MAX_CHARS);
        let question = f.text.lines().last().unwrap();
        assert!(
            question.starts_with("Do you want to proceed? x"),
            "the question is kept"
        );
        assert!(f.text.starts_with("line "), "the first lines are kept");
        for o in &f.options {
            assert!(o.label.chars().count() <= PROMPT_OPTION_MAX_CHARS);
            assert!(o.label.ends_with('…'));
        }
        let all = format!(
            "{}{}",
            f.text,
            f.options
                .iter()
                .map(|o| o.label.as_str())
                .collect::<String>()
        );
        assert!(!all.chars().any(|c| c.is_control() && c != '\n'), "{all:?}");
        // Ten options are more than one key can pick.
        let ten = s.replace("\n Esc to cancel", "   10. ten\n\n Esc to cancel");
        assert_eq!(extract(HookAgent::Claude, &rows(&ten)), None);
    }

    /// Two prompts that look alike once capped are still told apart.
    #[test]
    fn fingerprint_is_uncapped() {
        let base = "a".repeat(PROMPT_OPTION_MAX_CHARS + 20);
        let one = CLAUDE.replace("2. Yes, and", &format!("2. {base}1 Yes, and"));
        let two = CLAUDE.replace("2. Yes, and", &format!("2. {base}2 Yes, and"));
        let (a, b) = (
            extract(HookAgent::Claude, &rows(&one)).unwrap(),
            extract(HookAgent::Claude, &rows(&two)).unwrap(),
        );
        assert_eq!(a.options, b.options);
        assert_eq!(a.text, b.text);
        assert_ne!(a.fingerprint, b.fingerprint);
        // Same screen, same fingerprint; another question, another one.
        let again = extract(HookAgent::Claude, &rows(&one)).unwrap();
        assert_eq!(a.fingerprint, again.fingerprint);
        let other = extract(HookAgent::Claude, &rows(&CLAUDE.replace("/tmp", "/var"))).unwrap();
        assert_ne!(
            other.fingerprint,
            extract(HookAgent::Claude, &rows(CLAUDE))
                .unwrap()
                .fingerprint
        );
    }

    #[test]
    fn screen_tail_strips_boxes_and_caps() {
        let s = "\
╭────────────╮
│ MCP server │
│  wants  a  │
│ value: █   │
╰────────────╯";
        assert_eq!(screen_tail(&rows(s)), "MCP server\nwants a\nvalue: █");
        let many: Vec<String> = (0..30).map(|i| format!("row {i}")).collect();
        let t = screen_tail(&many);
        assert_eq!(t.lines().count(), TAIL_ROWS);
        assert!(t.ends_with("row 29") && t.starts_with("row 18"));
        let wide: Vec<String> = (0..12)
            .map(|i| format!("{i} {}", "\u{1D11E}".repeat(90)))
            .collect();
        let t = screen_tail(&wide);
        assert!(t.chars().count() <= PROMPT_TEXT_MAX_CHARS);
        assert!(t.ends_with(&"\u{1D11E}".repeat(90)));
        let one = vec!["y".repeat(2000)];
        let t = screen_tail(&one);
        assert_eq!(t.chars().count(), PROMPT_TEXT_MAX_CHARS);
        assert!(t.starts_with('…'));
        assert_eq!(screen_tail(&rows("\u{7}\n \t \n")), "");
    }

    fn rule(n: usize) -> String {
        "─".repeat(n)
    }

    #[test]
    fn claude_composer_is_recognised() {
        let r = rule(60);
        let ok = [
            format!("⏺ Done.\n\n{r}\n❯ \n{r}\n  ? for shortcuts"),
            format!("⏺ Done.\n\n{r}\n❯\n{r}"),
            format!("✻ Thinking… (esc to interrupt)\n\n{r}\n❯ \n{r}\n  ⏵⏵ accept edits on (shift+tab to cycle)\n  12% until auto-compact\n\n"),
            format!("{r}\n❯ \n{r}\n  ⏸ plan mode on · Context low (8% remaining) · Run /compact to compact & continue"),
            format!("{r}\n❯ x\n{r}\n  Esc again to clear                     30% context used"),
            format!("{r}\n❯ a draft that wraps\n  onto a second row\n\n  and a third\n{r}\n  ? for shortcuts"),
            format!("{r}\n> \n{r}"),
            // Claude Code 1.x: a round box with sides.
            format!("╭{r}╮\n│ > a draft{}│\n│   more   {}│\n╰{r}╯\n  ? for shortcuts", " ".repeat(51), " ".repeat(51)),
        ];
        for s in &ok {
            assert!(composer(HookAgent::Claude, &rows(s)), "{s}");
            assert!(!composer(HookAgent::Codex, &rows(s)), "{s}");
        }
        let no = [
            // The permission dialog, the model picker: a top rule only, options, key hints.
            CLAUDE.to_string(),
            format!("{r}\n Select model\n Switch models.\n\n ❯ 1. Default\n   2. Opus\n\n Enter to confirm · Esc to exit"),
            // An option list framed like the input.
            format!("{r}\n❯ 1. Yes\n  2. No\n{r}"),
            format!("{r}\n❯ Yes\n  2. No\n{r}"),
            // A dialog's title under the top edge, the bash mode, another mark.
            format!("{r}\n Do you want to proceed?\n❯ Yes\n{r}"),
            format!("{r}\n! ls\n{r}\n  ! for shell mode"),
            format!("{r}\n› \n{r}"),
            format!("{r}\n❯x\n{r}"),
            format!("{r}\n   ❯ \n{r}"),
            // Unequal or short rules, a missing edge.
            format!("{r}\n❯ \n{}", rule(59)),
            format!("{}\n❯ \n{}", rule(10), rule(10)),
            format!("❯ \n{r}"),
            format!("{r}\n❯ "),
            format!("{r}\n{r}"),
            // Too much below it, a dialog's hint below it, an option row below it.
            format!("{r}\n❯ \n{r}\n a\n b\n c\n d\n e"),
            format!("{r}\n❯ \n{r}\n Esc to cancel"),
            format!("{r}\n❯ \n{r}\n ❯ 1. build"),
            // A continuation that is not indented: other output inside the edges.
            format!("{r}\n❯ fix it\n⏺ Done.\n{r}"),
            // A boxed dialog (Claude Code 1.x Edit, the welcome banner).
            format!("╭{r}╮\n│ Edit file{}│\n╰{r}╯", " ".repeat(51)),
            format!("╭{r}╮\n│ > x{}│\n╰{}╯", " ".repeat(57), rule(61)),
            String::new(),
            "> ".to_string(),
            "$ ".to_string(),
            // Unknown rows below the input: a question, a list, a command's prompt, a
            // custom status line, another agent's or a dialog's hints, a hint inside text.
            format!("{r}\n❯ \n{r}\nContinue? [y/N] "),
            format!("{r}\n❯ \n{r}\n  ? for shortcuts\nContinue? [y/N] "),
            format!("{r}\n❯ \n{r}\n› auto\n  gpt-5"),
            format!("{r}\n❯ \n{r}\n  ❯ Opus\n    Sonnet"),
            format!("{r}\n❯ \n{r}\n> "),
            format!("{r}\n❯ \n{r}\n  main • 3 files changed"),
            format!("{r}\n❯ \n{r}\n  ? for shortcuts    tab to queue message"),
            format!("{r}\n❯ \n{r}\n  Enter to confirm · Esc to exit"),
            format!("{r}\n❯ \n{r}\n  Press ? for shortcuts now"),
            format!("{r}\n❯ \n{r}\n  ⏵⏵ accept edits on (shift+tab to cycle) and more"),
            format!("{r}\n❯ \n{r}\n  101% until auto-compact"),
            format!("{r}\n❯ \n{r}\n  x% context used"),
        ];
        for s in &no {
            assert!(!composer(HookAgent::Claude, &rows(s)), "{s}");
        }
    }

    #[test]
    fn codex_composer_is_recognised() {
        let ok = [
            "› fix the bug\n\n• Fixed it.\n\n› Ask Codex to do anything\n\n  ? for shortcuts    100% context left",
            "\n› \n\n  ? for shortcuts",
            "• Working (3s • esc to interrupt)\n\n› a draft\n  second line\n\n  tab to queue message",
            "\n›\n\n  esc again to edit previous message\n  ctrl + c again to quit\n\n",
            "\n› x\n\n  ⏎ send   ⌃J newline   ⌃T transcript   ⌃C quit",
            "\n› \n\n  ? for shortcuts · 42% context left",
        ];
        for s in ok {
            assert!(composer(HookAgent::Codex, &rows(s)), "{s}");
            assert!(!composer(HookAgent::Claude, &rows(s)), "{s}");
        }
        let no = [
            CODEX,
            // A selection list: its focus is the lowest mark.
            "\n› Ask Codex\n\n  Select Model\n\n› 1. auto\n  2. gpt-5\n\n  Press enter to confirm or esc to go back",
            "\n› 1. auto\n  2. gpt-5",
            // No padding row above it, or something else right below it.
            "• Working\n› ",
            "› ",
            "\n› fix it\n• Fixed it.",
            // Too much below it, a dialog's hint below it, a footer without the gap.
            "\n› \n\n a\n b\n c\n d\n e",
            "\n› \n\n  Press enter to confirm or esc to cancel",
            "\n› x\n  ? for shortcuts\nmore",
            "\n› \n\n  2. No",
            "\n›x",
            "\n   › ",
            "",
            // No footer: the frame alone is not enough.
            "\n› ",
            "\n›\n\n\n",
            // An unnumbered picker with the `›` focus, its own footer or none.
            "\n  Select Model\n  Pick a quick auto mode or browse all models.\n\n› auto\n  gpt-5\n\n  Press enter to confirm or esc to go back",
            "  Select Model\n\n› auto\n  gpt-5\n  gpt-5-codex",
            "\n› auto\n  gpt-5\n\n  Press enter to select or esc to cancel",
            // A question, a command's prompt, unknown text or a status line below it.
            "\n› \n\n  Continue? [y/N]",
            "\n› \n\n  ? for shortcuts\n> ",
            "\n› \n\n  main • 3 files changed",
            "\n› \n\n  ? for shortcuts    Esc to cancel",
            "\n› \n\n  ? for shortcuts later",
            "\n› \n\n  maybe tab to queue message",
            "\n› \n\n  ? for shortcuts\n  ? for shortcuts\n  ? for shortcuts\n  ? for shortcuts\n  ? for shortcuts",
        ];
        for s in no {
            assert!(!composer(HookAgent::Codex, &rows(s)), "{s}");
        }
    }

    #[test]
    fn bracketed_paste_mode() {
        let mut m = ScreenModel::new(20, 5);
        assert!(!m.bracketed_paste());
        m.feed(b"\x1b[?2004h");
        assert!(m.bracketed_paste(), "on");
        m.feed(b"\x1b[?2004l");
        assert!(!m.bracketed_paste(), "off");
        // Split across reads.
        m.feed(b"\x1b[?20");
        assert!(!m.bracketed_paste());
        m.feed(b"04h");
        assert!(m.bracketed_paste(), "split");
        m.feed(b"\x1b[?2004l\x1b[?1049;2004h");
        assert!(m.bracketed_paste(), "combined parameters");
        // A reset turns it off.
        m.feed(b"\x1bc");
        assert!(!m.bracketed_paste(), "RIS");
        // Other modes, and the non-private 2004, leave it alone.
        m.feed(b"\x1b[?1049h\x1b[?25l\x1b[2004h\x1b[?2003h");
        assert!(!m.bracketed_paste(), "other modes");
        m.feed(b"\x1b[?2004h\x1b[?1049l\x1b[?25h");
        assert!(m.bracketed_paste());
        // Garbage keeps the parser's state bounded and recovers.
        let junk: Vec<u8> = (0..200_000u32).map(|i| (i * 7919 % 251) as u8).collect();
        m.feed(&junk);
        m.feed(b"\x18\x1b[?2004l");
        assert!(!m.bracketed_paste(), "recovers after garbage");
        // A resize keeps it.
        m.feed(b"\x1b[?2004h");
        m.resize(40, 10);
        assert!(m.bracketed_paste());
    }

    #[test]
    fn screen_model_follows_resize() {
        let mut m = ScreenModel::new(20, 5);
        assert_eq!(m.size(), (20, 5));
        m.feed(b"hello\r\n\x1b[1mworld\x1b[0m   ");
        assert_eq!(m.rows(), ["hello", "world", "", "", ""]);
        m.resize(10, 3);
        assert_eq!(m.size(), (10, 3));
        assert_eq!(m.rows().len(), 3);
        m.feed(b"\x1b[2J\x1b[H0123456789abc");
        assert_eq!(m.rows(), ["0123456789", "abc", ""]);
        m.resize(0, 0);
        assert_eq!(m.size(), (1, 1));
    }
}
