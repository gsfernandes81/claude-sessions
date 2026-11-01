//! Claude's markdown, drawn as Claude Code 2.1.311 draws it — read from its bundled renderer
//! on 2026-11-01, since no mockup draws markdown and Claude Code's design comes first
//! (`docs/remote-view.md` § *The order of authority*).
//!
//! What it does, and so what this does:
//!
//! - **Headings** lose their `#`s: the first level bold, italic and underlined, the others
//!   bold, each followed by a blank line.
//! - **Inline code** and a code block with no language are lavender, the theme's
//!   `permission` colour; a block with one is plain, which is what its highlighter gives a
//!   language it does not know (this client has none). The fences are not drawn.
//! - **Lists** are `-` at every depth; numbered ones count `1.`, then `a.`, then `i.` as they
//!   nest. A wrapped item hangs under its text, not its marker.
//! - **Quotes** are drawn behind `▎` in italic.
//! - **Tables** are boxed in `┌─┬─┐`, the header centred, each row a rule apart, a column
//!   wrapped to the room it gets — the room shared out as Claude Code shares it — and every
//!   cell centred on its row's height. A table that would need more than four lines for a
//!   row, or still not fit, is drawn as `Header: value` lines instead, a short rule between
//!   rows.
//! - `---` is drawn as itself; `**`, `*`, `_`, `~~` and `` ` `` are styles, and a link is its
//!   text with the address after it when the two differ.
//!
//! **Pure**, and tolerant: anything it does not read as markdown is text.

use crate::convo::{GREY, LAVENDER, Line, PLAIN, St, cw, width};

/// Claude Code's own limits for a table: the narrowest a column is drawn, and the most lines
/// a row may wrap to before the table is drawn as lines instead.
const MIN_COL: usize = 3;
const MAX_ROW_LINES: usize = 4;

/// `text` drawn at `w` cells.
pub fn render(text: &str, w: usize) -> Vec<Line> {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out = Vec::new();
    blocks(&lines, w.max(1), 0, &mut out);
    while out.last().is_some_and(Vec::is_empty) {
        out.pop();
    }
    while out.first().is_some_and(Vec::is_empty) {
        out.remove(0);
    }
    out
}

fn blank(out: &mut Vec<Line>) {
    if out.last().is_some_and(|l| !l.is_empty()) {
        out.push(Vec::new());
    }
}

// ── blocks ──────────────────────────────────────────────────────────────────

fn indent_of(l: &str) -> usize {
    l.len() - l.trim_start_matches(' ').len()
}

fn fence(l: &str) -> Option<(char, usize, &str)> {
    if indent_of(l) > 3 {
        return None;
    }
    let t = l.trim_start();
    let c = t.chars().next()?;
    if c != '`' && c != '~' {
        return None;
    }
    let n = t.chars().take_while(|&x| x == c).count();
    (n >= 3).then(|| (c, n, t[n..].trim()))
}

fn heading(l: &str) -> Option<(usize, &str)> {
    if indent_of(l) > 3 {
        return None;
    }
    let t = l.trim_start();
    let n = t.chars().take_while(|&c| c == '#').count();
    if !(1..=6).contains(&n) {
        return None;
    }
    let rest = &t[n..];
    if !rest.is_empty() && !rest.starts_with(' ') {
        return None;
    }
    let rest = rest.trim();
    // A closing run of `#`s is not part of the heading.
    let rest = match rest.trim_end_matches('#') {
        r if r.is_empty() || r.ends_with(' ') => r.trim_end(),
        _ => rest,
    };
    Some((n, rest))
}

fn is_hr(l: &str) -> bool {
    if indent_of(l) > 3 {
        return false;
    }
    let t: String = l.chars().filter(|c| !c.is_whitespace()).collect();
    let Some(c) = t.chars().next() else {
        return false;
    };
    matches!(c, '-' | '*' | '_') && t.len() >= 3 && t.chars().all(|x| x == c)
}

/// A setext underline: `===` makes the line above a first-level heading, `---` a second.
fn setext(l: &str) -> Option<usize> {
    if indent_of(l) > 3 {
        return None;
    }
    let t = l.trim();
    if !t.is_empty() && t.chars().all(|c| c == '=') {
        return Some(1);
    }
    if !t.is_empty() && t.chars().all(|c| c == '-') {
        return Some(2);
    }
    None
}

fn quote(l: &str) -> Option<&str> {
    if indent_of(l) > 3 {
        return None;
    }
    let t = l.trim_start().strip_prefix('>')?;
    Some(t.strip_prefix(' ').unwrap_or(t))
}

/// A list item's start: its indent, whether it is numbered (and from what), and where its
/// text starts.
struct Item {
    indent: usize,
    number: Option<u64>,
    bullet: char,
    content: usize,
}

fn item(l: &str) -> Option<Item> {
    let indent = indent_of(l);
    let t = &l[indent..];
    let c = t.chars().next()?;
    let (number, marker_len, bullet) = if matches!(c, '-' | '*' | '+') {
        (None, 1, c)
    } else {
        let digits = t.chars().take_while(char::is_ascii_digit).count();
        if digits == 0 || digits > 9 {
            return None;
        }
        let after = t[digits..].chars().next()?;
        if after != '.' && after != ')' {
            return None;
        }
        (t[..digits].parse().ok(), digits + 1, after)
    };
    let rest = &t[marker_len..];
    if rest.is_empty() {
        return Some(Item {
            indent,
            number,
            bullet,
            content: indent + marker_len + 1,
        });
    }
    if !rest.starts_with(' ') {
        return None;
    }
    let gap = (rest.len() - rest.trim_start_matches(' ').len()).min(4);
    Some(Item {
        indent,
        number,
        bullet,
        content: indent + marker_len + gap,
    })
}

fn cells(l: &str) -> Vec<String> {
    let t = l.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut code = false;
    let mut chars = t.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                cur.push('|');
                chars.next();
            }
            '`' => {
                code = !code;
                cur.push(c);
            }
            '|' if !code => out.push(std::mem::take(&mut cur).trim().to_string()),
            _ => cur.push(c),
        }
    }
    out.push(cur.trim().to_string());
    out
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Align {
    Left,
    Centre,
    Right,
}

fn delimiter_row(l: &str) -> Option<Vec<Align>> {
    if !l.contains('-') {
        return None;
    }
    let cs = cells(l);
    cs.iter()
        .map(|c| {
            let inner = c.trim_start_matches(':').trim_end_matches(':');
            if inner.is_empty() || !inner.chars().all(|x| x == '-') {
                return None;
            }
            Some(match (c.starts_with(':'), c.ends_with(':')) {
                (true, true) => Align::Centre,
                (false, true) => Align::Right,
                _ => Align::Left,
            })
        })
        .collect()
}

fn table_start(lines: &[&str], i: usize) -> bool {
    lines[i].contains('|')
        && lines
            .get(i + 1)
            .and_then(|l| delimiter_row(l))
            .is_some_and(|a| a.len() == cells(lines[i]).len())
}

/// Whether a line starts a block other than a paragraph, so ends one.
fn interrupts(lines: &[&str], i: usize) -> bool {
    let l = lines[i];
    fence(l).is_some()
        || heading(l).is_some()
        || quote(l).is_some()
        || is_hr(l)
        || item(l).is_some_and(|it| it.number.is_none_or(|n| n == 1))
        || table_start(lines, i)
}

fn blocks(lines: &[&str], w: usize, depth: usize, out: &mut Vec<Line>) {
    let mut i = 0;
    while i < lines.len() {
        let l = lines[i];
        if l.trim().is_empty() {
            blank(out);
            i += 1;
            continue;
        }
        if let Some((c, n, lang)) = fence(l) {
            let pad = indent_of(l);
            i += 1;
            let style = if lang.is_empty() { fg(LAVENDER) } else { PLAIN };
            while i < lines.len() {
                if let Some((c2, n2, rest)) = fence(lines[i]) {
                    if c2 == c && n2 >= n && rest.is_empty() {
                        i += 1;
                        break;
                    }
                }
                let raw = lines[i];
                let raw = &raw[indent_of(raw).min(pad)..];
                code_line(raw, style, w, out);
                i += 1;
            }
            continue;
        }
        if let Some((level, text)) = heading(l) {
            let st = heading_style(level);
            out.extend(wrap_spans(&inline(text, st), w));
            out.push(Vec::new());
            i += 1;
            continue;
        }
        if table_start(lines, i) {
            let header = cells(l);
            let align = delimiter_row(lines[i + 1]).unwrap_or_default();
            i += 2;
            let mut rows = Vec::new();
            while i < lines.len() && !lines[i].trim().is_empty() && lines[i].contains('|') {
                let mut r = cells(lines[i]);
                r.resize(header.len(), String::new());
                rows.push(r);
                i += 1;
            }
            table(&header, &align, &rows, w, out);
            continue;
        }
        if is_hr(l) {
            out.push(vec![("---".into(), PLAIN)]);
            i += 1;
            continue;
        }
        if quote(l).is_some() {
            let mut inner = Vec::new();
            while i < lines.len() {
                match quote(lines[i]) {
                    Some(q) => inner.push(q),
                    None => break,
                }
                i += 1;
            }
            let mut body = Vec::new();
            blocks(&inner, w.saturating_sub(2).max(1), depth, &mut body);
            while body.last().is_some_and(Vec::is_empty) {
                body.pop();
            }
            for b in body {
                let mut l: Line = vec![("▎ ".into(), fg(GREY))];
                l.extend(b.into_iter().map(|(t, s)| (t, St { italic: true, ..s })));
                out.push(l);
            }
            continue;
        }
        if let Some(first) = item(l) {
            i = list(lines, i, first, w, depth, out);
            continue;
        }
        // A paragraph: up to a blank line or another block.
        let mut para = vec![l.trim()];
        i += 1;
        let mut level = None;
        while i < lines.len() && !lines[i].trim().is_empty() {
            if let Some(n) = setext(lines[i]) {
                level = Some(n);
                i += 1;
                break;
            }
            if interrupts(lines, i) {
                break;
            }
            para.push(lines[i].trim());
            i += 1;
        }
        let text = para.join("\n");
        match level {
            Some(n) => {
                out.extend(wrap_spans(&inline(&text, heading_style(n)), w));
                out.push(Vec::new());
            }
            None => out.extend(wrap_spans(&inline(&text, PLAIN), w)),
        }
    }
}

fn heading_style(level: usize) -> St {
    St {
        bold: true,
        italic: level == 1,
        underline: level == 1,
        ..PLAIN
    }
}

fn fg(c: u32) -> St {
    St {
        fg: Some(c),
        ..PLAIN
    }
}

/// A line of code: not word-wrapped, broken at the width as a terminal would; in lavender
/// from its first character to its last, with no language.
fn code_line(raw: &str, st: St, w: usize, out: &mut Vec<Line>) {
    let raw = raw.trim_end();
    if raw.is_empty() {
        out.push(Vec::new());
        return;
    }
    let mut rows: Vec<String> = vec![String::new()];
    let mut used = 0;
    for c in raw.chars() {
        let c = if c == '\t' { ' ' } else { c };
        if used + cw(c) > w {
            rows.push(String::new());
            used = 0;
        }
        rows.last_mut().expect("a row").push(c);
        used += cw(c);
    }
    for r in rows {
        let lead = r.len() - r.trim_start().len();
        let mut l: Line = Vec::new();
        if lead > 0 {
            l.push((r[..lead].to_string(), PLAIN));
        }
        l.push((r[lead..].to_string(), st));
        out.push(l);
    }
}

/// A list from line `i`; returns the line after it.
fn list(
    lines: &[&str],
    mut i: usize,
    first: Item,
    w: usize,
    depth: usize,
    out: &mut Vec<Line>,
) -> usize {
    let ordered = first.number.is_some();
    let start = first.number.unwrap_or(1);
    // Each item's lines, its content dedented, and whether a blank line came before it.
    let mut items: Vec<(Vec<String>, bool)> = Vec::new();
    let mut gap = false;
    while i < lines.len() {
        let l = lines[i];
        match item(l) {
            Some(it)
                if it.indent < first.content
                    && it.number.is_some() == ordered
                    && (ordered || it.bullet == first.bullet)
                    && it.indent + 3 >= first.indent =>
            {
                let text = l.get(it.content.min(l.len())..).unwrap_or("").to_string();
                items.push((vec![text], gap && !items.is_empty()));
                gap = false;
                i += 1;
                // The item's own lines: indented under its text, or carried on before a
                // blank line.
                while i < lines.len() {
                    let n = lines[i];
                    if n.trim().is_empty() {
                        // A blank line continues the item only if what follows is indented.
                        let next = lines[i + 1..].iter().find(|x| !x.trim().is_empty());
                        match next {
                            Some(x) if indent_of(x) >= it.content => {
                                items.last_mut().expect("an item").0.push(String::new());
                                i += 1;
                                continue;
                            }
                            _ => {
                                gap = true;
                                i += 1;
                                break;
                            }
                        }
                    }
                    if indent_of(n) >= it.content {
                        let cut = it.content.min(indent_of(n));
                        items
                            .last_mut()
                            .expect("an item")
                            .0
                            .push(n[cut..].to_string());
                        i += 1;
                        continue;
                    }
                    // Lazy continuation: a plain line that starts nothing else.
                    if item(n).is_none() && !interrupts(lines, i) {
                        items
                            .last_mut()
                            .expect("an item")
                            .0
                            .push(n.trim().to_string());
                        i += 1;
                        continue;
                    }
                    break;
                }
            }
            _ if l.trim().is_empty() => {
                gap = true;
                i += 1;
            }
            _ => break,
        }
    }
    let last = start + items.len() as u64 - 1;
    for (n, (body, gap_before)) in items.iter().enumerate() {
        if *gap_before {
            blank(out);
        }
        let marker = if ordered {
            format!("{}.", number(depth + 1, start + n as u64, start, last))
        } else {
            "-".to_string()
        };
        let cell = width(&marker) + 1;
        let refs: Vec<&str> = body.iter().map(String::as_str).collect();
        let mut inner = Vec::new();
        blocks(&refs, w.saturating_sub(cell).max(1), depth + 1, &mut inner);
        while inner.last().is_some_and(Vec::is_empty) {
            inner.pop();
        }
        if inner.is_empty() {
            inner.push(Vec::new());
        }
        for (k, l) in inner.into_iter().enumerate() {
            if l.is_empty() {
                out.push(l);
                continue;
            }
            let lead = if k == 0 {
                format!("{marker} ")
            } else {
                " ".repeat(cell)
            };
            let mut row: Line = vec![(lead, PLAIN)];
            row.extend(l);
            out.push(row);
        }
    }
    // A list that runs into a paragraph is set apart from it.
    if i < lines.len() && !lines[i].trim().is_empty() {
        blank(out);
    }
    i
}

/// How Claude Code numbers an item at a depth: letters at the second, roman at the third.
fn number(depth: usize, n: u64, first: u64, last: u64) -> String {
    match depth {
        2 if first >= 1 => letters(n),
        3 if first >= 1 && last <= 3999 => roman(n),
        _ => n.to_string(),
    }
}

fn letters(mut n: u64) -> String {
    let mut out = Vec::new();
    while n > 0 {
        n -= 1;
        out.push((b'a' + (n % 26) as u8) as char);
        n /= 26;
    }
    out.iter().rev().collect()
}

fn roman(mut n: u64) -> String {
    const R: [(u64, &str); 13] = [
        (1000, "m"),
        (900, "cm"),
        (500, "d"),
        (400, "cd"),
        (100, "c"),
        (90, "xc"),
        (50, "l"),
        (40, "xl"),
        (10, "x"),
        (9, "ix"),
        (5, "v"),
        (4, "iv"),
        (1, "i"),
    ];
    let mut out = String::new();
    for (v, s) in R {
        while n >= v {
            out.push_str(s);
            n -= v;
        }
    }
    out
}

// ── tables ──────────────────────────────────────────────────────────────────

fn table(header: &[String], align: &[Align], rows: &[Vec<String>], w: usize, out: &mut Vec<Line>) {
    let k = header.len();
    let spans = |c: &str| inline(c, PLAIN);
    let text = |c: &str| plain_of(&spans(c));
    let all = || std::iter::once(header).chain(rows.iter().map(Vec::as_slice));
    // The longest word of each column, and its whole widest cell.
    let least: Vec<usize> = (0..k)
        .map(|c| {
            all()
                .map(|r| text(&r[c]).split_whitespace().map(width).max().unwrap_or(0))
                .max()
                .unwrap_or(0)
                .max(MIN_COL)
        })
        .collect();
    let ideal: Vec<usize> = (0..k)
        .map(|c| {
            all()
                .map(|r| width(&text(&r[c])))
                .max()
                .unwrap_or(0)
                .max(MIN_COL)
        })
        .collect();
    let room = w.saturating_sub(1 + 3 * k).max(k * MIN_COL);
    let (sum_least, sum_ideal) = (least.iter().sum::<usize>(), ideal.iter().sum::<usize>());
    let mut hard = false;
    let widths: Vec<usize> = if sum_ideal <= room {
        ideal.clone()
    } else if sum_least <= room {
        let spare = room - sum_least;
        let wants: Vec<usize> = (0..k).map(|c| ideal[c] - least[c]).collect();
        let total: usize = wants.iter().sum();
        (0..k)
            .map(|c| least[c] + (wants[c] * spare).checked_div(total).unwrap_or(0))
            .collect()
    } else {
        hard = true;
        (0..k)
            .map(|c| (least[c] * room / sum_least).max(MIN_COL))
            .collect()
    };
    let wrap_cell = |c: &str, cw: usize| -> Vec<Line> {
        let s = spans(c);
        if hard {
            hard_wrap(&s, cw)
        } else {
            wrap_spans(&s, cw)
        }
    };
    let tall = all().any(|r| (0..k).any(|c| wrap_cell(&r[c], widths[c]).len() > MAX_ROW_LINES));
    let drawn = 1 + widths.iter().map(|x| x + 3).sum::<usize>();
    if tall || drawn > w {
        vertical(header, rows, w, out);
        return;
    }
    let rule = |l: &str, m: &str, r: &str| -> Line {
        let mut s = l.to_string();
        for (c, cw) in widths.iter().enumerate() {
            s.push_str(&"─".repeat(cw + 2));
            s.push_str(if c + 1 < k { m } else { r });
        }
        vec![(s, PLAIN)]
    };
    let row = |r: &[String], head: bool, out: &mut Vec<Line>| {
        let wrapped: Vec<Vec<Line>> = (0..k).map(|c| wrap_cell(&r[c], widths[c])).collect();
        let tallest = wrapped.iter().map(Vec::len).max().unwrap_or(1).max(1);
        for y in 0..tallest {
            let mut l: Line = vec![("│".into(), PLAIN)];
            for c in 0..k {
                let top = (tallest - wrapped[c].len()) / 2;
                let cell = y
                    .checked_sub(top)
                    .and_then(|j| wrapped[c].get(j))
                    .cloned()
                    .unwrap_or_default();
                let used = width(&plain_of(&cell));
                let spare = widths[c].saturating_sub(used);
                let how = if head {
                    Align::Centre
                } else {
                    align.get(c).copied().unwrap_or(Align::Left)
                };
                let (before, after) = match how {
                    Align::Left => (0, spare),
                    Align::Right => (spare, 0),
                    Align::Centre => (spare / 2, spare - spare / 2),
                };
                l.push((format!(" {}", " ".repeat(before)), PLAIN));
                l.extend(cell);
                l.push((format!("{} │", " ".repeat(after)), PLAIN));
            }
            out.push(l);
        }
    };
    out.push(rule("┌", "┬", "┐"));
    row(header, true, out);
    out.push(rule("├", "┼", "┤"));
    for (n, r) in rows.iter().enumerate() {
        row(r, false, out);
        if n + 1 < rows.len() {
            out.push(rule("├", "┼", "┤"));
        }
    }
    out.push(rule("└", "┴", "┘"));
}

/// A table too wide or too tall for boxes: each row as `Header: value` lines, a rule of up
/// to 40 between rows.
fn vertical(header: &[String], rows: &[Vec<String>], w: usize, out: &mut Vec<Line>) {
    let bold = St {
        bold: true,
        ..PLAIN
    };
    let sep: Line = vec![("─".repeat(w.saturating_sub(1).min(40)), PLAIN)];
    let body: Vec<&[String]> = if rows.is_empty() {
        vec![header]
    } else {
        rows.iter().map(Vec::as_slice).collect()
    };
    let mut first = true;
    for r in body {
        let mut lines: Vec<Line> = Vec::new();
        for (c, cell) in r.iter().enumerate() {
            let h = if rows.is_empty() {
                ""
            } else {
                header.get(c).map_or("", |h| h.trim())
            };
            let value = cell.split_whitespace().collect::<Vec<_>>().join(" ");
            if h.is_empty() && value.is_empty() {
                continue;
            }
            let mut spans: Vec<(String, St)> = Vec::new();
            if !h.is_empty() {
                spans.push((format!("{}:", plain_of(&inline(h, PLAIN))), bold));
                spans.push((" ".into(), PLAIN));
            }
            spans.extend(inline(&value, PLAIN));
            lines.extend(wrap_spans(&spans, w.saturating_sub(1).max(1)));
        }
        if lines.is_empty() {
            continue;
        }
        if !first {
            out.push(sep.clone());
        }
        first = false;
        out.extend(lines);
    }
}

// ── inline ──────────────────────────────────────────────────────────────────

fn plain_of(spans: &[(String, St)]) -> String {
    spans.iter().map(|(t, _)| t.as_str()).collect()
}

fn push(out: &mut Vec<(String, St)>, text: &str, st: St) {
    if text.is_empty() {
        return;
    }
    match out.last_mut() {
        Some((t, s)) if *s == st => t.push_str(text),
        _ => out.push((text.to_string(), st)),
    }
}

/// `s`'s inline markdown as styled runs, on top of `base`.
pub fn inline(s: &str, base: St) -> Vec<(String, St)> {
    let mut out = Vec::new();
    inline_into(s, base, &mut out);
    out
}

fn inline_into(s: &str, base: St, out: &mut Vec<(String, St)>) {
    let b = s.as_bytes();
    let mut i = 0;
    let mut text = String::new();
    let flush = |text: &mut String, out: &mut Vec<(String, St)>| {
        push(out, text, base);
        text.clear();
    };
    while i < s.len() {
        let c = b[i];
        // An escaped punctuation mark is itself.
        if c == b'\\' && i + 1 < s.len() && b[i + 1].is_ascii_punctuation() {
            text.push(b[i + 1] as char);
            i += 2;
            continue;
        }
        if c == b'`' {
            let n = s[i..].bytes().take_while(|&x| x == b'`').count();
            let tick = &s[i..i + n];
            if let Some(end) = s[i + n..].find(tick) {
                flush(&mut text, out);
                let code = &s[i + n..i + n + end];
                let code = if code.len() > 2 && code.starts_with(' ') && code.ends_with(' ') {
                    &code[1..code.len() - 1]
                } else {
                    code
                };
                push(
                    out,
                    &code.replace('\n', " "),
                    St {
                        fg: Some(LAVENDER),
                        ..base
                    },
                );
                i += n + end + n;
                continue;
            }
            text.push_str(tick);
            i += n;
            continue;
        }
        if c == b'[' {
            if let Some((label, url, len)) = link(&s[i..]) {
                flush(&mut text, out);
                inline_into(label, base, out);
                let shown = plain_of(&inline(label, base));
                if !url.is_empty() && shown != url {
                    push(out, &format!(" ({url})"), base);
                }
                i += len;
                continue;
            }
        }
        if let Some((delim, st)) = emphasis_at(s, i, base) {
            if let Some(end) = closer(s, i + delim.len(), delim) {
                flush(&mut text, out);
                inline_into(&s[i + delim.len()..end], st, out);
                i = end + delim.len();
                continue;
            }
        }
        let ch = s[i..].chars().next().expect("a char");
        text.push(ch);
        i += ch.len_utf8();
    }
    flush(&mut text, out);
}

/// The emphasis that opens at `i`, if one can: its delimiter and the style inside.
fn emphasis_at(s: &str, i: usize, base: St) -> Option<(&'static str, St)> {
    let rest = &s[i..];
    let (delim, st) = if rest.starts_with("**") {
        ("**", St { bold: true, ..base })
    } else if rest.starts_with("__") {
        ("__", St { bold: true, ..base })
    } else if rest.starts_with("~~") {
        (
            "~~",
            St {
                strike: true,
                ..base
            },
        )
    } else if rest.starts_with('*') {
        (
            "*",
            St {
                italic: true,
                ..base
            },
        )
    } else if rest.starts_with('_') {
        (
            "_",
            St {
                italic: true,
                ..base
            },
        )
    } else {
        return None;
    };
    // An opener is followed by text, and `_` does not open inside a word.
    let next = rest[delim.len()..].chars().next()?;
    if next.is_whitespace() {
        return None;
    }
    if delim.starts_with('_')
        && s[..i]
            .chars()
            .next_back()
            .is_some_and(char::is_alphanumeric)
    {
        return None;
    }
    Some((delim, st))
}

/// Where the emphasis opened before `from` closes: after text, and for `_` not inside a word.
fn closer(s: &str, from: usize, delim: &str) -> Option<usize> {
    let mut at = from;
    while let Some(k) = s[at..].find(delim) {
        let k = at + k;
        let before = s[..k].chars().next_back();
        let after = s[k + delim.len()..].chars().next();
        let single = delim.len() == 1;
        // A `*` that is half of a `**` belongs to that.
        let doubled = single && after == Some(delim.chars().next().expect("a delimiter"));
        let ok = k > from
            && before.is_some_and(|c| !c.is_whitespace())
            && !(delim.starts_with('_') && after.is_some_and(char::is_alphanumeric))
            && !doubled;
        if ok {
            return Some(k);
        }
        at = k + delim.len().max(if doubled { 2 } else { 1 });
        if at > s.len() {
            return None;
        }
    }
    None
}

/// `[label](url)` at the start of `s`: the label, the address and the length of the whole.
fn link(s: &str) -> Option<(&str, &str, usize)> {
    let close = s.find("](")?;
    let label = &s[1..close];
    if label.contains('\n') {
        return None;
    }
    let rest = &s[close + 2..];
    let end = rest.find(')')?;
    let url = rest[..end].trim();
    let url = url.split_whitespace().next().unwrap_or("");
    Some((label, url, close + 2 + end + 1))
}

// ── wrapping styled runs ────────────────────────────────────────────────────

/// Styled runs word-wrapped to `w`, a newline in them a break: greedy, a break eats the space
/// it falls on, a word longer than a line is broken — as `convo::wrap` does plain text.
pub fn wrap_spans(spans: &[(String, St)], w: usize) -> Vec<Line> {
    let w = w.max(1);
    let mut lines: Vec<Line> = Vec::new();
    for para in split_lines(spans) {
        // Words: runs of non-space characters, each a list of styled pieces.
        let mut words: Vec<Vec<(String, St)>> = vec![Vec::new()];
        let mut space_st: Vec<St> = Vec::new();
        for (t, st) in &para {
            for ch in t.chars() {
                if ch == ' ' {
                    words.push(Vec::new());
                    space_st.push(*st);
                } else {
                    let word = words.last_mut().expect("a word");
                    match word.last_mut() {
                        Some((x, s)) if s == st => x.push(ch),
                        _ => word.push((ch.to_string(), *st)),
                    }
                }
            }
        }
        let mut cur: Line = Vec::new();
        let mut cur_w = 0;
        let mut started = false;
        for (n, word) in words.into_iter().enumerate() {
            let ww = width(&plain_of(&word));
            if started && cur_w + 1 + ww <= w {
                let st = space_st[n - 1];
                append(&mut cur, " ", st);
                for (t, s) in &word {
                    append(&mut cur, t, *s);
                }
                cur_w += 1 + ww;
                continue;
            }
            if started {
                lines.push(std::mem::take(&mut cur));
            }
            started = true;
            cur_w = 0;
            for (t, s) in word {
                for ch in t.chars() {
                    if cur_w + cw(ch) > w {
                        lines.push(std::mem::take(&mut cur));
                        cur_w = 0;
                    }
                    append(&mut cur, &ch.to_string(), s);
                    cur_w += cw(ch);
                }
            }
        }
        lines.push(cur);
    }
    lines
}

/// Styled runs broken at exactly `w`, as a table's columns are when even their longest words
/// do not fit.
fn hard_wrap(spans: &[(String, St)], w: usize) -> Vec<Line> {
    let w = w.max(1);
    let mut lines: Vec<Line> = Vec::new();
    for para in split_lines(spans) {
        let mut cur: Line = Vec::new();
        let mut used = 0;
        for (t, st) in para {
            for ch in t.chars() {
                if used + cw(ch) > w {
                    lines.push(std::mem::take(&mut cur));
                    used = 0;
                }
                append(&mut cur, &ch.to_string(), st);
                used += cw(ch);
            }
        }
        lines.push(cur);
    }
    lines
}

fn split_lines(spans: &[(String, St)]) -> Vec<Vec<(String, St)>> {
    let mut out: Vec<Vec<(String, St)>> = vec![Vec::new()];
    for (t, st) in spans {
        for (n, piece) in t.split('\n').enumerate() {
            if n > 0 {
                out.push(Vec::new());
            }
            if !piece.is_empty() {
                out.last_mut()
                    .expect("a line")
                    .push((piece.to_string(), *st));
            }
        }
    }
    out
}

fn append(l: &mut Line, t: &str, st: St) {
    match l.last_mut() {
        Some((x, s)) if *s == st => x.push_str(t),
        _ => l.push((t.to_string(), st)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convo::plain;

    fn text(md: &str, w: usize) -> Vec<String> {
        render(md, w)
            .iter()
            .map(|l| plain(l).trim_end().to_string())
            .collect()
    }

    fn style_of(md: &str, w: usize, word: &str) -> St {
        for l in render(md, w) {
            for (t, s) in l {
                if t.contains(word) {
                    return s;
                }
            }
        }
        panic!("{word} is not drawn")
    }

    #[test]
    fn plain_text_is_drawn_as_it_was_before() {
        assert_eq!(text("one\ntwo", 40), ["one", "two"]);
        assert_eq!(text("a bb ccc", 4), ["a bb", "ccc"]);
        assert_eq!(text("2 * 3 * 4 and a_b_c", 40), ["2 * 3 * 4 and a_b_c"]);
    }

    #[test]
    fn emphasis_and_code_are_styles_not_marks() {
        assert_eq!(
            text("a **bold** and *it* and `x = 1`", 40),
            ["a bold and it and x = 1"]
        );
        assert!(style_of("a **bold** b", 40, "bold").bold);
        assert!(style_of("an *it* b", 40, "it").italic);
        assert!(style_of("an _it_ b", 40, "it").italic);
        assert!(style_of("~~gone~~", 40, "gone").strike);
        assert_eq!(style_of("run `cargo test`", 40, "cargo").fg, Some(LAVENDER));
        assert!(
            style_of("**bold `code`**", 40, "code").bold,
            "code inside bold is bold"
        );
        assert_eq!(text(r"\*not\*", 40), ["*not*"]);
        assert_eq!(
            text(
                "[docs](https://x.dev) and [https://y.dev](https://y.dev)",
                60
            ),
            ["docs (https://x.dev) and https://y.dev"]
        );
    }

    #[test]
    fn a_styled_word_wraps_with_its_style() {
        let got = render("aaa **bbb ccc** ddd", 7);
        let lines: Vec<String> = got.iter().map(plain).collect();
        assert_eq!(lines, ["aaa bbb", "ccc ddd"]);
        assert!(got[1][0].1.bold, "ccc kept bold: {:?}", got[1]);
        assert!(!got[1].last().unwrap().1.bold);
    }

    #[test]
    fn headings_lose_their_marks_and_take_a_line_after() {
        assert_eq!(
            text("# Title\ntext\n## Sub ##\nmore", 40),
            ["Title", "", "text", "Sub", "", "more"]
        );
        let h1 = style_of("# Title", 40, "Title");
        assert!(h1.bold && h1.italic && h1.underline);
        let h2 = style_of("## Sub", 40, "Sub");
        assert!(h2.bold && !h2.italic && !h2.underline);
        assert_eq!(text("Title\n=====\nbody", 40), ["Title", "", "body"]);
        assert_eq!(text("#hashtag", 40), ["#hashtag"]);
    }

    #[test]
    fn code_blocks_drop_their_fences_and_are_not_wrapped() {
        let md = "before\n\n```\nlet x = 1;\n  indented\n```\n\nafter";
        assert_eq!(
            text(md, 40),
            ["before", "", "let x = 1;", "  indented", "", "after"]
        );
        assert_eq!(style_of(md, 40, "let").fg, Some(LAVENDER));
        assert_eq!(style_of("```rust\nfn main() {}\n```", 40, "fn").fg, None);
        assert_eq!(text("```\nabcdefgh\n```", 5), ["abcde", "fgh"]);
        assert_eq!(
            text("```\n# not a heading\n- nor a list\n```", 40),
            ["# not a heading", "- nor a list"]
        );
    }

    #[test]
    fn lists_hang_under_their_text_and_number_as_claude_code_does() {
        assert_eq!(
            text("- one\n- two words here", 12),
            ["- one", "- two words", "  here"]
        );
        assert_eq!(text("* star\n+ plus", 40), ["- star", "", "- plus"]);
        assert_eq!(
            text(
                "1. first\n2. second\n   1. inner\n   2. again\n      1. deep\n3. third",
                40
            ),
            [
                "1. first",
                "2. second",
                "   a. inner",
                "   b. again",
                "      i. deep",
                "3. third"
            ]
        );
        assert_eq!(text("- a\n  - b\n    - c", 40), ["- a", "  - b", "    - c"]);
        assert_eq!(
            text("intro:\n- a\n- b\n\nafter", 40),
            ["intro:", "- a", "- b", "", "after"]
        );
        assert_eq!(text("- a\n\n- b", 40), ["- a", "", "- b"]);
        assert_eq!(text("5. five\n6. six", 40), ["5. five", "6. six"]);
        assert_eq!(
            text("- [x] done\n- [ ] not", 40),
            ["- [x] done", "- [ ] not"]
        );
    }

    #[test]
    fn quotes_and_rules() {
        assert_eq!(
            text("> said\n> twice\n\n---\nend", 40),
            ["▎ said", "▎ twice", "", "---", "end"]
        );
        assert!(style_of("> said", 40, "said").italic);
    }

    #[test]
    fn a_table_is_boxed_with_its_header_centred() {
        let md = "| Name | Size |\n|------|-----:|\n| a | 1 |\n| bbbb | 22 |";
        assert_eq!(
            text(md, 40),
            [
                "┌──────┬──────┐",
                "│ Name │ Size │",
                "├──────┼──────┤",
                "│ a    │    1 │",
                "├──────┼──────┤",
                "│ bbbb │   22 │",
                "└──────┴──────┘",
            ]
        );
        assert!(
            text("|a|b|\n|:-:|---|\n|x|y|", 40)[3].starts_with("│  x  │"),
            "centred column"
        );
    }

    #[test]
    fn a_table_wraps_its_columns_in_the_room_and_centres_rows() {
        let md = "| Option | What it does |\n|---|---|\n| relay | Keeps the log on the hub between reconnects |";
        let got = text(md, 34);
        assert!(got.iter().all(|l| width(l) <= 34), "{got:#?}");
        assert_eq!(got[0].chars().next(), Some('┌'));
        // The short cell sits on the middle line of the tall one.
        let rows: Vec<&String> = got.iter().filter(|l| l.starts_with('│')).skip(1).collect();
        assert!(rows.len() >= 2, "{got:#?}");
        let mid = rows.len() / 2 - usize::from(rows.len() % 2 == 0);
        assert!(rows[mid].contains("relay"), "{got:#?}");
    }

    #[test]
    fn a_table_too_wide_for_boxes_is_drawn_as_lines() {
        let md = "| Field | Meaning | Default |\n|---|---|---|\n| keep_until | when the offloader may take it | three minutes ahead |\n| driver | holder or terminal | terminal |";
        let got = text(md, 24);
        assert_eq!(got[0], "Field: keep_until");
        assert!(got.contains(&"Default: terminal".to_string()), "{got:#?}");
        assert!(got.contains(&"─".repeat(23)), "rows a rule apart: {got:#?}");
        assert!(style_of(md, 24, "Field:").bold);
    }

    #[test]
    fn a_pipe_in_prose_is_not_a_table() {
        assert_eq!(text("a | b\nc", 40), ["a | b", "c"]);
    }

    #[test]
    fn half_streamed_markdown_draws_what_it_has() {
        assert_eq!(text("```\nlet x", 40), ["let x"]);
        assert_eq!(text("some **bold", 40), ["some **bold"]);
        assert_eq!(text("| a | b |\n|---|", 40), ["| a | b |", "|---|"]);
    }
}
