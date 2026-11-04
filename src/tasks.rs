//! The tasks view: claude's background shells, monitors, subagents and workflows, as Claude
//! Code 2.1.311 draws them — the pill on the footer's mode line, the `Background` list that
//! `/tasks` or Down then Enter opens, and a details screen per task (read off its screen on
//! 2026-11-04, `docs/remote-view.md` § *The tasks view*).
//!
//! **Pure**, as `convo.rs` is: the task list in (`Convo::tasks`, from the relay's `tasks` op),
//! the view's state from the client, lines out. A shell's output is asked of claude by the
//! client (`tout`) while its details are open, and arrives in `View::output`.

use crate::convo::{
    self, GREEN, GREY, LAVENDER, Line, PLAIN, St, Task, bold, cut, fg, line, rule, width,
};

/// Claude Code's cyan for the tasks view: its titles, the pill, a running status.
pub const CYAN: u32 = 0x00_cccc;

/// How many lines of a shell's output its details show at most, as Claude Code's box does.
pub const OUTPUT_LINES: usize = 9;

/// How wide a row's title may be, as Claude Code cuts it at 80 columns.
const TITLE_MAX: usize = 50;

/// The tasks view's state, kept by the client in its dialog.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct View {
    /// The row the cursor is on, into `listed`.
    pub sel: usize,
    /// The task whose details are open, by its id.
    pub open: Option<String>,
    /// Opened from the pill, as Claude Code's inline variant is: a cyan `─` rule and a
    /// two-column indent, where `/tasks` opens it with a lavender `▔` and three.
    pub inline: bool,
    /// Opened straight into the details of the only task, from the pill: `←` closes it
    /// rather than going back to a list nobody saw.
    pub direct: bool,
    /// The open shell's or monitor's output as claude last answered: its text, or why there
    /// is none. `None` until the first answer.
    pub output: Option<Result<String, String>>,
    /// How long the open task had run when its details were opened: Claude Code's
    /// `Runtime:` stays as it was then.
    pub runtime: String,
}

fn shell_like(t: &Task) -> bool {
    matches!(t.kind.as_str(), "shell" | "monitor")
}

/// Running shells and monitors, Claude Code's pill: `2 shells, 1 monitor`. `None` when
/// there are none, and no pill is drawn.
pub fn pill(tasks: &[Task]) -> Option<String> {
    let n = |k: &str| tasks.iter().filter(|t| t.running() && t.kind == k).count();
    let (shells, monitors) = (n("shell"), n("monitor"));
    let mut parts = Vec::new();
    if shells > 0 {
        parts.push(plural(shells, "shell"));
    }
    if monitors > 0 {
        parts.push(plural(monitors, "monitor"));
    }
    (!parts.is_empty()).then(|| parts.join(", "))
}

/// Whether a subagent or workflow runs in the background: Claude Code's mode line then says
/// `/tasks to see subagents`, since its pill counts only shells.
pub fn subagents_running(tasks: &[Task]) -> bool {
    tasks.iter().any(|t| t.running() && !shell_like(t))
}

/// What a turn's end line adds while shells run, `2 shells, 1 monitor still running`:
/// Claude Code rewrites every such line as tasks end, so it is drawn from the list as it is.
pub fn still_running(tasks: &[Task]) -> Option<String> {
    pill(tasks).map(|p| format!("{p} still running"))
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

/// The list's rows, by section in Claude Code's order: running shells and monitors, local
/// agents, workflows, anything else running, then the agents that finished. A shell or
/// workflow that ended leaves the list, as it does there.
pub fn listed(tasks: &[Task]) -> Vec<(&'static str, &Task)> {
    let mut out = Vec::new();
    let mut take = |name: &'static str, f: &dyn Fn(&Task) -> bool| {
        out.extend(tasks.iter().filter(|t| f(t)).map(|t| (name, t)));
    };
    take("Shells", &|t| t.running() && shell_like(t));
    take("Local agents", &|t| t.running() && t.kind == "agent");
    take("Dynamic workflows", &|t| {
        t.running() && t.kind == "workflow"
    });
    take("Remote agents", &|t| t.running() && t.kind == "remote");
    take("Other", &|t| {
        t.running()
            && !matches!(
                t.kind.as_str(),
                "shell" | "monitor" | "agent" | "workflow" | "remote"
            )
    });
    take("Completed", &|t| {
        t.kind == "agent" && t.status == "completed"
    });
    out
}

/// What a row is called: a shell by its command, anything else by its description.
fn title(t: &Task) -> String {
    let by_command = t.kind == "shell" || t.description.is_empty();
    let s = if by_command && !t.detail.is_empty() {
        &t.detail
    } else {
        &t.description
    };
    s.lines().next().unwrap_or("").to_string()
}

fn status(t: &Task) -> String {
    match (t.kind.as_str(), t.status.as_str()) {
        ("agent", "completed") => "done".into(),
        (_, s) => s.to_string(),
    }
}

/// How long a task has run, or ran, at `now` (epoch milliseconds): `56s`, `4m 11s`.
pub fn runtime(t: &Task, now: u64) -> String {
    let Some(start) = t.started_ms else {
        return String::new();
    };
    let end = t.ended_ms.unwrap_or(now);
    let s = end.saturating_sub(start) / 1000;
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m {}s", s / 60, s % 60),
        _ => format!("{}h {}m", s / 3600, (s % 3600) / 60),
    }
}

fn italic_grey() -> St {
    St {
        italic: true,
        ..fg(GREY)
    }
}

/// Claude Code's hint line here, grey and italic, broken between whole segments.
fn hint(segs: &[&str], ind: &str, w: usize) -> Vec<Line> {
    let room = w.saturating_sub(width(ind)).max(1);
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    for seg in segs {
        let next = if cur.is_empty() {
            seg.to_string()
        } else {
            format!("{cur} · {seg}")
        };
        if cur.is_empty() || width(&next) <= room {
            cur = next;
            continue;
        }
        // Claude Code wraps the joined hint as text, at words, not at its segments.
        lines.push(cur);
        cur = seg.to_string();
    }
    lines.push(cur);
    let joined = lines.join(" · ");
    convo::wrap(&joined, room)
        .into_iter()
        .map(|l| vec![(ind.to_string(), PLAIN), (l, italic_grey())])
        .collect()
}

fn indent(v: &View) -> &'static str {
    if v.inline { "  " } else { "   " }
}

fn top_rule(v: &View, w: usize) -> Line {
    if v.inline {
        rule(w, "─", fg(CYAN))
    } else {
        rule(w, "▔", fg(LAVENDER))
    }
}

/// The view at `w` columns with `h` rows to draw in: the list, or the open task's details.
pub fn draw(c: &convo::Convo, v: &View, w: usize, h: usize) -> Vec<Line> {
    let open = v
        .open
        .as_ref()
        .and_then(|id| c.tasks.iter().find(|t| &t.id == id));
    match open {
        Some(t) if shell_like(t) => shell_details(t, v, w, h),
        Some(t) if t.kind == "workflow" => workflow_details(c, t, v, w),
        Some(t) => agent_details(c, t, v, w),
        None => list(&c.tasks, v, w, h),
    }
}

fn list(tasks: &[Task], v: &View, w: usize, h: usize) -> Vec<Line> {
    let ind = indent(v);
    let mut out = vec![top_rule(v, w), line(format!("{ind}Background"), bold(CYAN))];
    let rows = listed(tasks);
    let shells = rows.iter().filter(|(s, _)| *s == "Shells").count();
    let agents = rows.iter().filter(|(s, _)| *s == "Local agents").count();
    // `3 active shells · 1 active agent`, as Claude Code words it; workflows not counted.
    let active = |n: usize, kind: &str| {
        let s = if n == 1 { "" } else { "s" };
        format!("{n} active {kind}{s}")
    };
    let mut sub = Vec::new();
    if shells > 0 {
        sub.push(active(shells, "shell"));
    }
    if agents > 0 {
        sub.push(active(agents, "agent"));
    }
    if !sub.is_empty() {
        out.push(line(format!("{ind}{}", sub.join(" · ")), fg(GREY)));
    }
    out.push(Vec::new());
    if rows.is_empty() {
        out.push(line(format!("{ind}No tasks currently running"), fg(GREY)));
        out.push(Vec::new());
        out.extend(hint(
            &["↑/↓ to select", "Enter to view", "Esc to close"],
            ind,
            w,
        ));
        return out;
    }
    let sel = v.sel.min(rows.len() - 1);
    let sections = {
        let mut s: Vec<&str> = rows.iter().map(|(s, _)| *s).collect();
        s.dedup();
        s.len() > 1
    };
    let statuses: Vec<String> = rows.iter().map(|(_, t)| status(t)).collect();
    let sw = statuses.iter().map(|s| width(s)).max().unwrap_or(0);
    let lead = width(ind) + 4;
    let tw = w
        .saturating_sub(lead + 3 + sw + width(ind))
        .clamp(4, TITLE_MAX);
    // What fits: the hint takes two rows on a phone, and the transcript keeps a few.
    let room = h.saturating_sub(out.len() + 3 + 4).max(3);
    let mut body: Vec<(Line, bool)> = Vec::new();
    let mut last = "";
    for (i, ((section, t), st)) in rows.iter().zip(&statuses).enumerate() {
        if sections && *section != last {
            if !last.is_empty() {
                body.push((Vec::new(), false));
            }
            let n = rows.iter().filter(|(s, _)| s == section).count();
            body.push((
                vec![
                    (format!("{ind}  {section}"), bold(GREY)),
                    (format!(" ({n})"), fg(GREY)),
                ],
                false,
            ));
            last = section;
        }
        let on = i == sel;
        let mark = if on { "❯ " } else { "  " };
        let (glyph, gst) = if t.status == "completed" && t.kind == "agent" {
            ("✔", fg(GREEN))
        } else {
            ("●", fg(GREY))
        };
        let name = cut(&title(t), tw);
        let pad = " ".repeat(tw.saturating_sub(width(&name)));
        body.push((
            vec![
                (ind.to_string(), PLAIN),
                (mark.to_string(), fg(LAVENDER)),
                (glyph.to_string(), gst),
                (" ".into(), PLAIN),
                (name, if on { fg(LAVENDER) } else { PLAIN }),
                (format!("{pad}   "), PLAIN),
                (st.clone(), fg(GREY)),
            ],
            on,
        ));
    }
    // A long list scrolls to keep the cursor in view.
    let at = body.iter().position(|(_, on)| *on).unwrap_or(0);
    let top = (at + 1).saturating_sub(room);
    out.extend(body.into_iter().skip(top).take(room).map(|(l, _)| l));
    out.push(Vec::new());
    let (_, t) = rows[sel];
    let segs: &[&str] = if t.running() {
        &[
            "↑/↓ to select",
            "Enter to view",
            "x to stop",
            "Esc to close",
        ]
    } else {
        &["↑/↓ to select", "Enter to view", "Esc to close"]
    };
    out.extend(hint(segs, ind, w));
    out
}

/// A label and its value, the value from column `ind + 10` and wrapped there.
fn field(ind: &str, label: &str, value: &str, st: St, w: usize) -> Vec<Line> {
    let col = width(ind) + 10;
    let room = w.saturating_sub(col + width(ind)).max(8);
    let mut out = Vec::new();
    for (i, piece) in convo::wrap(value, room).into_iter().enumerate() {
        if i == 0 {
            let pad = " ".repeat(10usize.saturating_sub(width(label)));
            out.push(vec![
                (ind.to_string(), PLAIN),
                (label.to_string(), bold_plain()),
                (pad, PLAIN),
                (piece, st),
            ]);
        } else {
            out.push(line(format!("{}{piece}", " ".repeat(col)), st));
        }
    }
    out
}

fn bold_plain() -> St {
    St {
        bold: true,
        ..PLAIN
    }
}

fn status_style(t: &Task) -> St {
    match t.status.as_str() {
        "running" => fg(CYAN),
        "completed" => fg(GREEN),
        "failed" => fg(convo::RED),
        _ => fg(GREY),
    }
}

fn back_hint(t: &Task, ind: &str, w: usize) -> Vec<Line> {
    if t.running() {
        hint(
            &["← to go back", "Esc/Enter/Space to close", "x to stop"],
            ind,
            w,
        )
    } else {
        hint(&["← to go back", "Esc/Enter/Space to close"], ind, w)
    }
}

fn shell_details(t: &Task, v: &View, w: usize, h: usize) -> Vec<Line> {
    let ind = indent(v);
    let (what, label) = if t.kind == "monitor" {
        ("Monitor details", "Script:")
    } else {
        ("Shell details", "Command:")
    };
    let mut out = vec![
        top_rule(v, w),
        line(format!("{ind}{what}"), bold(CYAN)),
        Vec::new(),
    ];
    out.extend(field(ind, "Status:", &t.status, status_style(t), w));
    if !v.runtime.is_empty() {
        out.extend(field(ind, "Runtime:", &v.runtime, PLAIN, w));
    }
    let script = if t.detail.is_empty() {
        &t.description
    } else {
        &t.detail
    };
    out.extend(field(ind, label, script, PLAIN, w));
    out.push(Vec::new());
    out.push(vec![
        (ind.to_string(), PLAIN),
        ("Output:".into(), bold_plain()),
    ]);
    let tail = back_hint(t, ind, w);
    match &v.output {
        None => out.push(line(format!("{ind}Reading output…"), fg(GREY))),
        Some(Err(_)) => out.push(line(format!("{ind}No output available"), fg(GREY))),
        Some(Ok(text)) if text.trim().is_empty() => {
            out.push(line(format!("{ind}No output available"), fg(GREY)))
        }
        Some(Ok(text)) => {
            // The box fits the screen with its hint, rather than running off the bottom
            // as Claude Code's does at 24 rows; nine lines where there is room.
            // Around the lines: the box's edges, its blank row, `Showing`, a blank and the
            // hint; and three rows of the transcript left above.
            let fixed = out.len() + 6 + tail.len() + 3;
            let shown = h.saturating_sub(fixed).clamp(1, OUTPUT_LINES);
            let all: Vec<&str> = text
                .strip_suffix('\n')
                .unwrap_or(text)
                .split('\n')
                .collect();
            let lines = &all[all.len().saturating_sub(shown)..];
            let bw = w.saturating_sub(2 * width(ind) + 2);
            out.push(line(format!("{ind}╭{}╮", "─".repeat(bw)), PLAIN));
            let row = |s: &str| {
                let s = cut(&strip_controls(s), bw.saturating_sub(2));
                let pad = " ".repeat(bw.saturating_sub(1 + width(&s)));
                line(format!("{ind}│ {s}{pad}│"), PLAIN)
            };
            for l in lines {
                out.push(row(l));
            }
            out.push(row(""));
            out.push(line(format!("{ind}╰{}╯", "─".repeat(bw)), PLAIN));
            out.push(line(
                format!("{ind}Showing {}", plural(lines.len(), "line")),
                italic_grey(),
            ));
        }
    }
    out.push(Vec::new());
    out.extend(tail);
    out
}

/// A shell's output as text the box can hold: no escapes, tabs as spaces.
fn strip_controls(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => {
                if chars.peek() == Some(&'[') {
                    chars.next();
                    for d in chars.by_ref() {
                        if ('@'..='~').contains(&d) {
                            break;
                        }
                    }
                } else {
                    chars.next();
                }
            }
            '\t' => out.push_str("    "),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// The lines the transcript draws under a subagent's or workflow's call.
fn progress_of<'a>(c: &'a convo::Convo, t: &Task) -> &'a [String] {
    t.item
        .and_then(|id| c.entry(id))
        .and_then(|e| match e {
            convo::Entry::Agent { lines, .. } => Some(lines.as_slice()),
            _ => None,
        })
        .unwrap_or(&[])
}

fn agent_details(c: &convo::Convo, t: &Task, v: &View, w: usize) -> Vec<Line> {
    let ind = indent(v);
    let kind = if t.detail.is_empty() {
        "agent"
    } else {
        &t.detail
    };
    let mut out = vec![top_rule(v, w)];
    out.push(line(
        cut(&format!("{ind}{kind} › {}", t.description), w),
        bold(CYAN),
    ));
    if !v.runtime.is_empty() {
        out.push(line(format!("{ind}{}", v.runtime), fg(GREY)));
    }
    out.push(Vec::new());
    let progress = progress_of(c, t);
    if !progress.is_empty() {
        out.push(line(format!("{ind}Progress"), bold(GREY)));
        for p in progress {
            out.push(line(
                cut(&format!("{ind}  {p}"), w.saturating_sub(width(ind))),
                fg(GREY),
            ));
        }
        out.push(Vec::new());
    }
    if !t.prompt.is_empty() {
        out.push(line(format!("{ind}Prompt"), bold(GREY)));
        let room = w.saturating_sub(2 * width(ind)).max(8);
        let mut rows: Vec<String> = t
            .prompt
            .split('\n')
            .flat_map(|p| convo::wrap(p.trim_end(), room))
            .collect();
        // Five lines, the last cut with `…` when there is more, as Claude Code shows it.
        if rows.len() > 5 {
            rows.truncate(5);
            let last = rows.pop().unwrap_or_default();
            let keep: String = last.chars().take(room.saturating_sub(1).min(10)).collect();
            rows.push(format!("{keep}…"));
        }
        for r in rows {
            out.push(line(format!("{ind}{r}"), PLAIN));
        }
        out.push(Vec::new());
    }
    out.extend(back_hint(t, ind, w));
    out
}

fn workflow_details(c: &convo::Convo, t: &Task, v: &View, w: usize) -> Vec<Line> {
    let ind = indent(v);
    let mut out = vec![top_rule(v, w), Vec::new()];
    out.push(line(
        cut(&format!("{ind}{}", t.description), w),
        bold(LAVENDER),
    ));
    if !v.runtime.is_empty() {
        out.push(line(format!("{ind}{}", v.runtime), fg(GREY)));
    }
    out.push(Vec::new());
    for p in progress_of(c, t) {
        out.push(line(
            cut(&format!("{ind}{p}"), w.saturating_sub(width(ind))),
            fg(GREY),
        ));
    }
    out.push(Vec::new());
    out.extend(back_hint(t, ind, w));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convo::{Convo, plain};

    fn task(id: &str, kind: &str, desc: &str, detail: &str, status: &str) -> Task {
        Task {
            id: id.into(),
            kind: kind.into(),
            description: desc.into(),
            detail: detail.into(),
            status: status.into(),
            started_ms: Some(1_000_000),
            ..Task::default()
        }
    }

    fn text(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| plain(l).trim_end().to_string())
            .collect()
    }

    fn shells() -> Vec<Task> {
        vec![
            task(
                "m",
                "monitor",
                "Print 200 monitor ticks three seconds apart",
                "for i in $(seq 200); do echo mon $i; sleep 3; done",
                "running",
            ),
            task(
                "t",
                "shell",
                "Print ticks",
                "for i in $(seq 100); do echo tick $i; sleep 2; done",
                "running",
            ),
            task("s", "shell", "Sleep", "sleep 300; echo done", "running"),
        ]
    }

    #[test]
    fn the_pill_counts_shells_and_monitors_as_claude_code_does() {
        let t = shells();
        assert_eq!(pill(&t).as_deref(), Some("2 shells, 1 monitor"));
        assert_eq!(pill(&t[2..]).as_deref(), Some("1 shell"));
        assert_eq!(pill(&t[..1]).as_deref(), Some("1 monitor"));
        assert_eq!(pill(&[]), None);
        let mut ended = t.clone();
        for e in &mut ended {
            e.status = "completed".into();
        }
        assert_eq!(pill(&ended), None, "ended ones are not counted");
        assert_eq!(
            still_running(&t).as_deref(),
            Some("2 shells, 1 monitor still running")
        );
        assert!(!subagents_running(&t));
        let mut more = t.clone();
        more.push(task("a", "agent", "doc survey", "Explore", "running"));
        assert!(subagents_running(&more));
    }

    #[test]
    fn the_list_at_80_and_40_as_claude_code_draws_it() {
        let mut c = Convo::new("h", "s", "r");
        c.tasks = shells();
        let v = View::default();
        assert_eq!(
            text(&draw(&c, &v, 80, 24)),
            [
                "▔".repeat(80).as_str(),
                "   Background",
                "   3 active shells",
                "",
                "   ❯ ● Print 200 monitor ticks three seconds apart          running",
                "     ● for i in $(seq 100); do echo tick $i; sleep 2; do…   running",
                "     ● sleep 300; echo done                                 running",
                "",
                "   ↑/↓ to select · Enter to view · x to stop · Esc to close",
            ]
        );
        let inline = View {
            inline: true,
            sel: 1,
            ..View::default()
        };
        c.tasks = shells()[..2].to_vec();
        assert_eq!(
            text(&draw(&c, &inline, 40, 24)),
            [
                "─".repeat(40).as_str(),
                "  Background",
                "  2 active shells",
                "",
                "    ● Print 200 monitor tic…   running",
                "  ❯ ● for i in $(seq 100); …   running",
                "",
                "  ↑/↓ to select · Enter to view · x to",
                "  stop · Esc to close",
            ]
        );
    }

    #[test]
    fn mixed_kinds_get_sections_and_a_finished_agent_is_completed() {
        let mut c = Convo::new("h", "s", "r");
        c.tasks = shells()[2..].to_vec();
        c.tasks.push(task(
            "a",
            "agent",
            "big doc survey",
            "general-purpose",
            "running",
        ));
        c.tasks.push(task(
            "w",
            "workflow",
            "big-doc-license-survey",
            "",
            "running",
        ));
        c.tasks
            .push(task("d", "agent", "doc survey", "Explore", "completed"));
        c.tasks
            .push(task("x", "shell", "gone", "true", "completed"));
        let v = View {
            sel: 3,
            ..View::default()
        };
        let got = text(&draw(&c, &v, 40, 30));
        assert_eq!(
            got,
            [
                "▔".repeat(40).as_str(),
                "   Background",
                "   1 active shell · 1 active agent",
                "",
                "     Shells (1)",
                "     ● sleep 300; echo done   running",
                "",
                "     Local agents (1)",
                "     ● big doc survey         running",
                "",
                "     Dynamic workflows (1)",
                "     ● big-doc-license-sur…   running",
                "",
                "     Completed (1)",
                "   ❯ ✔ doc survey             done",
                "",
                "   ↑/↓ to select · Enter to view · Esc",
                "   to close",
            ]
        );
        let empty = Convo::new("h", "s", "r");
        assert_eq!(
            text(&draw(&empty, &View::default(), 80, 24))[3..],
            [
                "   No tasks currently running",
                "",
                "   ↑/↓ to select · Enter to view · Esc to close"
            ]
        );
    }

    #[test]
    fn a_shells_details_show_its_last_lines_in_a_box() {
        let mut c = Convo::new("h", "s", "r");
        c.tasks = shells();
        let out: String = (1..=30).map(|n| format!("tick {n}\n")).collect();
        let v = View {
            open: Some("t".into()),
            output: Some(Ok(out)),
            runtime: "56s".into(),
            ..View::default()
        };
        let got = text(&draw(&c, &v, 80, 40));
        let mut want = vec![
            "▔".repeat(80),
            "   Shell details".into(),
            "".into(),
            "   Status:   running".into(),
            "   Runtime:  56s".into(),
            "   Command:  for i in $(seq 100); do echo tick $i; sleep 2; done".into(),
            "".into(),
            "   Output:".into(),
            format!("   ╭{}╮", "─".repeat(72)),
        ];
        for n in 22..=30 {
            want.push(format!("   │ tick {n:<width$}│", width = 71 - 5));
        }
        want.push(format!("   │{}│", " ".repeat(72)));
        want.push(format!("   ╰{}╯", "─".repeat(72)));
        want.push("   Showing 9 lines".into());
        want.push("".into());
        want.push("   ← to go back · Esc/Enter/Space to close · x to stop".into());
        assert_eq!(got, want);
        // At 24 rows the box shrinks so the hint stays on screen.
        let small = draw(&c, &v, 80, 24);
        assert!(small.len() <= 21, "{}", small.len());
        assert!(text(&small).last().unwrap().contains("x to stop"));
        // A monitor says Script, and a shell with nothing yet says so.
        let m = View {
            open: Some("m".into()),
            output: Some(Ok(String::new())),
            ..View::default()
        };
        let got = text(&draw(&c, &m, 40, 24));
        assert_eq!(got[1], "   Monitor details");
        assert_eq!(got[4], "   Script:   for i in $(seq 200); do");
        assert!(
            got.contains(&"   No output available".to_string()),
            "{got:?}"
        );
    }

    #[test]
    fn an_agents_details_show_its_progress_and_prompt() {
        let mut c = Convo::new("h", "s", "r");
        c.apply(
            &crate::json::parse(
                r#"{"t":"add","id":4,"k":"agent","n":"Explore","a":"big doc survey"}"#,
            )
            .unwrap(),
            std::time::Instant::now(),
        );
        c.apply(
            &crate::json::parse(r#"{"t":"prog","id":4,"p":["Read(/a)","Read(/b)","+3 more tool uses"],"st":"running"}"#).unwrap(),
            std::time::Instant::now(),
        );
        let mut a = task("a", "agent", "big doc survey", "Explore", "running");
        a.item = Some(4);
        a.prompt = "List /usr/share/doc and read each copyright file.".into();
        c.tasks = vec![a];
        let v = View {
            open: Some("a".into()),
            runtime: "1m 39s".into(),
            ..View::default()
        };
        assert_eq!(
            text(&draw(&c, &v, 40, 24)),
            [
                "▔".repeat(40).as_str(),
                "   Explore › big doc survey",
                "   1m 39s",
                "",
                "   Progress",
                "     Read(/a)",
                "     Read(/b)",
                "     +3 more tool uses",
                "",
                "   Prompt",
                "   List /usr/share/doc and read each",
                "   copyright file.",
                "",
                "   ← to go back · Esc/Enter/Space to",
                "   close · x to stop",
            ]
        );
    }

    #[test]
    fn runtime_reads_as_claude_code_writes_it() {
        let mut t = task("a", "shell", "", "", "running");
        assert_eq!(runtime(&t, 1_056_000), "56s");
        assert_eq!(runtime(&t, 1_251_000), "4m 11s");
        t.ended_ms = Some(1_010_000);
        assert_eq!(runtime(&t, 9_999_999), "10s", "an ended one stops counting");
        t.started_ms = None;
        assert_eq!(runtime(&t, 1), "");
    }
}
