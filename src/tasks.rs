//! The tasks view: claude's background shells, monitors, subagents and workflows, as Claude
//! Code 2.1.311 draws them — the pill on the footer's mode line, the `Background` list that
//! `/tasks` or Down then Enter opens, and a details screen per task (read off its screen on
//! 2026-11-04, `docs/remote-view.md` § *The tasks view*).
//!
//! **Pure**, as `convo.rs` is: the task list in (`Convo::tasks`, from the relay's `tasks` op),
//! the view's state from the client, the wall clock passed in, lines out. A shell's output
//! is asked of claude by the client (`tout`) while its details are open, and arrives in
//! `View::output`; a workflow's phases and agents likewise (`wf`, into `View::wf`).
//!
//! Also here: the agent strip Claude Code draws under the footer (`strip`), and its workflow
//! browser, two panes at 60 columns and over, stacked under that.

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
    /// How many task notices had come when the view opened: those since are "waiting".
    pub notices_at: u64,
    /// The open workflow's phases and agents as the relay last sent them.
    pub wf: Option<Wf>,
    /// Where the workflow browser's cursor is: its phases, its agents, or one agent's pane.
    pub pane: Pane,
    pub phase: usize,
    pub agent: usize,
    /// The agents pane shows only agents in this state (`f filter`): empty for all.
    pub filter: String,
    /// `s save`'s form, open over the browser.
    pub save: Option<Save>,
}

/// Where the workflow browser's cursor is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Pane {
    #[default]
    Phases,
    Agents,
    /// One agent's own pane.
    Agent,
}

/// `s save`'s form: the name the file takes, the scope, and what the holder answered.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Save {
    pub name: String,
    /// User scope, `~/.claude/workflows`, rather than the project's.
    pub user: bool,
    pub said: Option<Result<String, String>>,
}

/// A workflow's phases and agents, from the relay's `wf`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Wf {
    pub phases: Vec<String>,
    pub agents: Vec<WfAgent>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct WfAgent {
    pub label: String,
    /// 1-based, into the phases.
    pub phase: u64,
    pub state: String,
    pub model: String,
    pub tokens: u64,
    pub tools: u64,
    pub started_ms: Option<u64>,
    pub duration_ms: Option<u64>,
    /// Its last call, `Read(/a/b)`.
    pub tool: String,
    pub error: String,
    /// Sent for the agent whose pane is open only.
    pub prompt: String,
    pub result: String,
}

/// The relay's `wf {ph, ag}`.
pub fn parse_wf(v: &crate::json::Value) -> Wf {
    use crate::json::Value;
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let n = |v: &Value, k: &str| v.get(k).and_then(Value::as_u64);
    Wf {
        phases: v
            .get("ph")
            .and_then(Value::as_arr)
            .unwrap_or(&[])
            .iter()
            .filter_map(|p| p.as_str().map(String::from))
            .collect(),
        agents: v
            .get("ag")
            .and_then(Value::as_arr)
            .unwrap_or(&[])
            .iter()
            .map(|a| WfAgent {
                label: s(a, "l"),
                phase: n(a, "ph").unwrap_or(0),
                state: s(a, "st"),
                model: s(a, "m"),
                tokens: n(a, "tk").unwrap_or(0),
                tools: n(a, "tc").unwrap_or(0),
                started_ms: n(a, "t0"),
                duration_ms: n(a, "ms"),
                tool: s(a, "tl"),
                error: s(a, "er"),
                prompt: s(a, "pp"),
                result: s(a, "rp"),
            })
            .collect(),
    }
}

/// `77.8k`, `1.2M`: tokens as Claude Code counts them.
pub fn tokens(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.1}k", n as f64 / 1000.0),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

fn secs(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m {}s", s / 60, s % 60),
        _ => format!("{}h {}m", s / 3600, (s % 3600) / 60),
    }
}

/// A model id as Claude Code names it, `Haiku 5.5`.
fn model(id: &str) -> String {
    if id.is_empty() {
        String::new()
    } else {
        convo::model_name(id)
    }
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

/// What a row is called: a shell by its command, a workflow by its name, anything else by
/// its description.
fn title(t: &Task) -> String {
    let by_command = matches!(t.kind.as_str(), "shell" | "workflow") || t.description.is_empty();
    let s = if by_command && !t.detail.is_empty() {
        &t.detail
    } else {
        &t.description
    };
    s.lines().next().unwrap_or("").to_string()
}

/// A row's status: an agent's model beside it where there is room (Claude Code drops it at
/// 40 columns), a workflow's agents.
fn status(t: &Task, wide: bool) -> String {
    let word = match (t.kind.as_str(), t.status.as_str()) {
        ("agent", "completed") => "done".to_string(),
        ("workflow", "running") if t.agents > 0 => plural(t.agents as usize, "agent"),
        (_, s) => s.to_string(),
    };
    if wide && t.kind == "agent" && !t.model.is_empty() {
        format!("{word} · {}", model(&t.model))
    } else {
        word
    }
}

fn can_foreground(t: &Task) -> bool {
    t.kind == "agent" && t.running()
}

/// How long a task has run, or ran, at `now` (epoch milliseconds): `56s`, `4m 11s`.
pub fn runtime(t: &Task, now: u64) -> String {
    let Some(start) = t.started_ms else {
        return String::new();
    };
    let end = t.ended_ms.unwrap_or(now);
    secs(end.saturating_sub(start))
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

/// The view at `w` columns with `h` rows to draw in, at `now` (epoch milliseconds): the
/// list, or the open task's details.
pub fn draw(c: &convo::Convo, v: &View, w: usize, h: usize, now: u64) -> Vec<Line> {
    let open = v
        .open
        .as_ref()
        .and_then(|id| c.tasks.iter().find(|t| &t.id == id));
    let mut out = Vec::new();
    // Claude Code holds the notices back while its panel is open and says so above it; here
    // they are in the transcript behind it, and this says how many came.
    let waiting = c.notices.saturating_sub(v.notices_at);
    if waiting > 0 {
        let said = if waiting == 1 {
            "Background task update waiting while this panel is open".to_string()
        } else {
            format!("{waiting} background task updates waiting while this panel is open")
        };
        out.push(line(cut(&said, w), fg(GREY)));
    }
    let h = h.saturating_sub(out.len());
    out.extend(match open {
        Some(t) if shell_like(t) => shell_details(t, v, w, h),
        Some(t) if t.kind == "workflow" => workflow_details(t, v, w, h, now),
        Some(t) => agent_details(c, t, v, w, now),
        None => list(&c.tasks, v, w, h),
    });
    out
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
    let statuses: Vec<String> = rows.iter().map(|(_, t)| status(t, w >= 60)).collect();
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
    let mut segs = vec!["↑/↓ to select", "Enter to view"];
    if can_foreground(t) {
        segs.push("f to foreground");
    }
    if t.running() {
        segs.push("x to stop");
    }
    segs.push("Esc to close");
    out.extend(hint(&segs, ind, w));
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
    let mut segs = vec!["← to go back", "Esc/Enter/Space to close"];
    if t.running() {
        segs.push("x to stop");
    }
    if can_foreground(t) {
        segs.push("f to foreground");
    }
    hint(&segs, ind, w)
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

fn agent_details(c: &convo::Convo, t: &Task, v: &View, w: usize, now: u64) -> Vec<Line> {
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
    // `1m 39s · 237.5k tokens · 51 tools · Haiku 5.5`, live, as Claude Code's is.
    let mut meta = Vec::new();
    let run = runtime(t, now);
    if !run.is_empty() {
        meta.push(run);
    }
    if t.tokens > 0 {
        meta.push(format!("{} tokens", tokens(t.tokens)));
    }
    if t.tools > 0 {
        meta.push(plural(t.tools as usize, "tool"));
    }
    if !t.model.is_empty() {
        meta.push(model(&t.model));
    }
    for l in segments(&meta, w.saturating_sub(2 * width(ind))) {
        out.push(line(format!("{ind}{l}"), fg(GREY)));
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

/// The agents the browser lists: the chosen phase's, through the filter.
pub fn wf_agents<'a>(wf: &'a Wf, v: &View) -> Vec<(usize, &'a WfAgent)> {
    let phase = v.phase as u64 + 1;
    wf.agents
        .iter()
        .enumerate()
        .filter(|(_, a)| a.phase == phase || (wf.phases.is_empty() && v.phase == 0))
        .filter(|(_, a)| v.filter.is_empty() || state_word(&a.state) == v.filter)
        .collect()
}

/// An agent's state in a word: `running`, `done`, `failed`, `queued`.
fn state_word(state: &str) -> &'static str {
    match state {
        "done" => "done",
        "failed" | "error" => "failed",
        "queued" => "queued",
        _ => "running",
    }
}

/// The filters `f` steps through, all first.
pub const FILTERS: [&str; 4] = ["", "running", "done", "failed"];

fn glyph(state: &str) -> (&'static str, St) {
    match state_word(state) {
        "done" => ("✔", fg(GREEN)),
        "failed" => ("●", fg(convo::RED)),
        "queued" => ("●", fg(GREY)),
        _ => ("●", fg(0x50_5050)),
    }
}

/// A box's top edge for `n` cells: `─ title ───…`, the title cut to fit.
fn edge(title: &str, n: usize) -> String {
    let t = cut(title, n.saturating_sub(4));
    let used = 3 + width(&t);
    format!("─ {t} {}", "─".repeat(n.saturating_sub(used)))
}

/// Segments joined by ` · `, broken between whole ones, a line ending ` ·` where it breaks.
fn segments(parts: &[String], room: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    for p in parts {
        if cur.is_empty() {
            cur = p.clone();
        } else if width(&cur) + 3 + width(p) <= room {
            cur = format!("{cur} · {p}");
        } else {
            lines.push(format!("{cur} ·"));
            cur = p.clone();
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

/// Cut or pad to exactly `n` cells.
fn fit(s: &str, n: usize) -> String {
    let s = cut(s, n);
    let pad = n.saturating_sub(width(&s));
    format!("{s}{}", " ".repeat(pad))
}

/// One agent's row: `● label Haiku 5.5 · 112.3k tokens   42s`, `n` cells wide.
fn agent_row(a: &WfAgent, on: bool, n: usize, now: u64) -> Line {
    let (g, gst) = glyph(&a.state);
    let el = match (a.duration_ms, a.started_ms) {
        (Some(ms), _) => secs(ms),
        (None, Some(t0)) => secs(now.saturating_sub(t0)),
        _ => String::new(),
    };
    let mut what = a.label.clone();
    let m = model(&a.model);
    if !m.is_empty() {
        what.push(' ');
        what.push_str(&m);
    }
    if a.tokens > 0 {
        what.push_str(&format!(" · {} tokens", tokens(a.tokens)));
    }
    // The mark, the glyph and a space before; a space each side of the time after.
    let room = n.saturating_sub(4 + width(&el) + 2);
    let st = if on { fg(LAVENDER) } else { fg(GREY) };
    vec![
        (if on { " ❯" } else { "  " }.to_string(), fg(LAVENDER)),
        (g.to_string(), gst),
        (" ".into(), PLAIN),
        (fit(&what, room), st),
        (format!(" {el} "), fg(GREY)),
    ]
}

/// One agent's pane: its state and counts, prompt, last call and result.
fn agent_pane(a: &WfAgent, n: usize, now: u64) -> Vec<Line> {
    let mut out = vec![line(cut(&a.label, n), bold_plain())];
    let mut meta = vec![state_word(&a.state).to_string()];
    match (a.duration_ms, a.started_ms) {
        (Some(ms), _) => meta.push(secs(ms)),
        (None, Some(t0)) => meta.push(secs(now.saturating_sub(t0))),
        _ => {}
    }
    if a.tokens > 0 {
        meta.push(format!("{} tokens", tokens(a.tokens)));
    }
    meta.push(plural(a.tools as usize, "tool call"));
    if !a.model.is_empty() {
        meta.push(model(&a.model));
    }
    for l in segments(&meta, n) {
        out.push(line(l, fg(GREY)));
    }
    let section = |out: &mut Vec<Line>, title: &str, body: &str, st: St, most: usize| {
        if body.trim().is_empty() {
            return;
        }
        out.push(Vec::new());
        out.push(line(title, bold(GREY)));
        let mut rows: Vec<String> = body
            .split('\n')
            .flat_map(|p| convo::wrap(p.trim_end(), n))
            .collect();
        if rows.len() > most {
            rows.truncate(most);
            if let Some(l) = rows.last_mut() {
                *l = format!("{}…", cut(l, n.saturating_sub(1)).trim_end_matches('…'));
            }
        }
        out.extend(rows.into_iter().map(|r| line(r, st)));
    };
    section(&mut out, "Prompt", &a.prompt, PLAIN, 5);
    section(&mut out, "Activity", &a.tool, fg(GREY), 2);
    section(&mut out, "Error", &a.error, fg(convo::RED), 3);
    section(&mut out, "Result", &a.result, PLAIN, 6);
    out
}

/// Claude Code's workflow browser: the name and progress, then a box of phases beside the
/// chosen phase's agents (one agent's pane when open), or above them under 60 columns.
fn workflow_details(t: &Task, v: &View, w: usize, h: usize, now: u64) -> Vec<Line> {
    let ind = indent(v);
    let mut out = vec![top_rule(v, w), Vec::new()];
    let name = if t.detail.is_empty() {
        &t.description
    } else {
        &t.detail
    };
    out.push(line(cut(&format!("{ind}{name}"), w), bold(LAVENDER)));
    let wf = v.wf.clone().unwrap_or_default();
    let done = wf.agents.iter().filter(|a| a.state == "done").count();
    let mut tail = format!("{done}/{} agents", wf.agents.len());
    let run = runtime(t, now);
    if !run.is_empty() {
        tail.push_str(&format!(" · {run}"));
    }
    let room = w.saturating_sub(2 * width(ind) + width(&tail) + 3);
    let desc = if name == &t.description || t.description.is_empty() {
        String::new()
    } else {
        format!("{} · ", cut(&t.description, room))
    };
    out.push(line(format!("{ind}{desc}{tail}"), fg(GREY)));
    out.push(Vec::new());
    if let Some(form) = &v.save {
        out.extend(save_form(form, name, ind, w));
        return out;
    }
    let hints = browser_hint(t, v, ind, w);
    // The box takes what is left, three rows of the transcript kept above.
    let rows = h
        .saturating_sub(out.len() + 2 + hints.len() + 3)
        .clamp(3, 30);
    let inner = w.saturating_sub(2 * width(ind));
    let phases: Vec<String> = if wf.phases.is_empty() {
        vec!["Agents".into()]
    } else {
        wf.phases.clone()
    };
    let phase_line = |i: usize, p: &str| {
        let n = i as u64 + 1;
        let here: Vec<&WfAgent> = wf.agents.iter().filter(|a| a.phase == n).collect();
        let d = here.iter().filter(|a| a.state == "done").count();
        format!("{n} {p} {d}/{}", here.len())
    };
    let agents = wf_agents(&wf, v);
    let title = {
        let p = phases.get(v.phase).cloned().unwrap_or_default();
        let mut t = format!("{p} · {}", plural(agents.len(), "agent"));
        if !v.filter.is_empty() {
            t.push_str(&format!(" · {}", v.filter));
        }
        t
    };
    let right: Vec<Line> = |n: usize| -> Vec<Line> {
        if v.pane == Pane::Agent {
            if let Some((_, a)) = agents.get(v.agent) {
                return agent_pane(a, n.saturating_sub(2), now)
                    .into_iter()
                    .map(|mut l| {
                        l.insert(0, (" ".into(), PLAIN));
                        l
                    })
                    .collect();
            }
        }
        if agents.is_empty() {
            return vec![line(" No agents yet", fg(GREY))];
        }
        let sel = v.agent.min(agents.len() - 1);
        let top = (sel + 1).saturating_sub(rows);
        agents
            .iter()
            .enumerate()
            .skip(top)
            .map(|(i, (_, a))| agent_row(a, v.pane == Pane::Agents && i == sel, n, now))
            .collect()
    }(if w >= 60 {
        inner.saturating_sub(3 + left_width(&phases, &phase_line, inner))
    } else {
        inner.saturating_sub(2)
    });
    let border = PLAIN;
    if w >= 60 {
        let lw = left_width(&phases, &phase_line, inner);
        let rw = inner.saturating_sub(lw + 3);
        let head_l = edge("Phases", lw);
        let head_r = edge(&title, rw);
        out.push(line(format!("{ind}╭{head_l}┬{head_r}╮"), border));
        for r in 0..rows {
            let left = match phases.get(r) {
                Some(p) => {
                    let on = r == v.phase;
                    let mark = if on && v.pane == Pane::Phases {
                        "❯"
                    } else {
                        " "
                    };
                    let st = if on { fg(LAVENDER) } else { PLAIN };
                    vec![
                        (format!(" {mark} "), st),
                        (fit(&phase_line(r, p), lw - 3), st),
                    ]
                }
                None => vec![(" ".repeat(lw), PLAIN)],
            };
            let mut row: Line = vec![(format!("{ind}│"), border)];
            row.extend(left);
            row.push(("│".into(), border));
            let mut cells = right.get(r).cloned().unwrap_or_default();
            let used: usize = cells.iter().map(|(t, _)| width(t)).sum();
            if used > rw {
                cells = vec![(cut(&convo::plain(&cells), rw), PLAIN)];
            } else {
                cells.push((" ".repeat(rw - used), PLAIN));
            }
            row.extend(cells);
            row.push(("│".into(), border));
            out.push(row);
        }
        out.push(line(
            format!("{ind}╰{}┴{}╯", "─".repeat(lw), "─".repeat(rw)),
            border,
        ));
    } else {
        // Narrow: the phases as a list, then a box of the chosen phase's agents.
        for (i, p) in phases.iter().enumerate() {
            let on = i == v.phase;
            let mark = if on && v.pane == Pane::Phases {
                "❯ "
            } else {
                "  "
            };
            let st = if on { fg(LAVENDER) } else { PLAIN };
            out.push(line(
                cut(&format!("{ind}{mark}{}", phase_line(i, p)), w),
                st,
            ));
        }
        let rw = inner.saturating_sub(2);
        out.push(line(format!("{ind}╭{}╮", edge(&title, rw)), border));
        let rows = rows.saturating_sub(phases.len()).max(2);
        for r in 0..rows.min(right.len().max(1)) {
            let mut cells = right.get(r).cloned().unwrap_or_default();
            let used: usize = cells.iter().map(|(t, _)| width(t)).sum();
            if used > rw {
                cells = vec![(cut(&convo::plain(&cells), rw), PLAIN)];
            } else {
                cells.push((" ".repeat(rw - used), PLAIN));
            }
            let mut row: Line = vec![(format!("{ind}│"), border)];
            row.extend(cells);
            row.push(("│".into(), border));
            out.push(row);
        }
        out.push(line(format!("{ind}╰{}╯", "─".repeat(rw)), border));
    }
    out.extend(hints);
    out
}

fn left_width(
    phases: &[String],
    phase_line: &dyn Fn(usize, &str) -> String,
    inner: usize,
) -> usize {
    let longest = phases
        .iter()
        .enumerate()
        .map(|(i, p)| width(&phase_line(i, p)))
        .max()
        .unwrap_or(0);
    (longest + 4).clamp(12, inner / 3)
}

/// The browser's keys, by where its cursor is. Claude Code's pause, restart and skip are
/// not offered: headless claude has no request for them.
fn browser_hint(t: &Task, v: &View, ind: &str, w: usize) -> Vec<Line> {
    let filter = if v.filter.is_empty() {
        "f filter".to_string()
    } else {
        format!("f filter: {}", v.filter)
    };
    let mut segs: Vec<&str> = Vec::new();
    match v.pane {
        Pane::Phases => {
            segs.push("↑↓ select");
            if t.running() {
                segs.push("x stop workflow");
            }
        }
        Pane::Agents => {
            segs.push("↑↓ select");
            segs.push("Enter to view");
            segs.push(&filter);
        }
        Pane::Agent => {}
    }
    segs.push("esc back");
    if t.saveable {
        segs.push("s save");
    }
    hint(&segs, ind, w)
}

/// `s save`: the name, the scope and where the file goes; Tab switches the scope.
fn save_form(f: &Save, fallback: &str, ind: &str, w: usize) -> Vec<Line> {
    let mut out = vec![line(format!("{ind}Save workflow"), bold(LAVENDER))];
    out.push(vec![
        (format!("{ind}Name: "), bold_plain()),
        (f.name.clone(), PLAIN),
    ]);
    let slug = file_name(if f.name.trim().is_empty() {
        fallback
    } else {
        &f.name
    });
    let (scope, path) = if f.user {
        ("User", format!("~/.claude/workflows/{slug}.js"))
    } else {
        ("Project", format!(".claude/workflows/{slug}.js"))
    };
    let room = w.saturating_sub(2 * width(ind)).max(8);
    for l in convo::wrap(&format!("{scope} scope · {path}"), room) {
        out.push(line(format!("{ind}{l}"), fg(GREY)));
    }
    match &f.said {
        Some(Ok(p)) => {
            out.push(Vec::new());
            for l in convo::wrap(&format!("Saved to {p}"), room) {
                out.push(line(format!("{ind}{l}"), fg(GREEN)));
            }
        }
        Some(Err(e)) => {
            out.push(Vec::new());
            for l in convo::wrap(e, w.saturating_sub(width(ind))) {
                out.push(line(format!("{ind}{l}"), fg(convo::RED)));
            }
        }
        None => {}
    }
    out.push(Vec::new());
    out.extend(hint(
        &["Enter to save", "Tab to switch scope", "Esc to cancel"],
        ind,
        w,
    ));
    out
}

/// A workflow's name as its file is named, as the holder names it (`holder.rs`).
pub fn file_name(name: &str) -> String {
    let mut out = String::new();
    for c in name.trim().chars().flat_map(char::to_lowercase) {
        let keep = c.is_ascii_alphanumeric() || c == '_' || c == '-';
        let c = if keep { c } else { '-' };
        if !(c == '-' && out.ends_with('-')) {
            out.push(c);
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "workflow".into()
    } else {
        out
    }
}

// ── the agent strip ─────────────────────────────────────────────────────────

/// How long an ended agent stays in the strip.
const STRIP_KEEP_MS: u64 = 60_000;

/// The agents and workflows the strip lists under `main`: those running, one ended in the
/// last minute, and the one in view.
pub fn strip_tasks<'a>(tasks: &'a [Task], now: u64, viewing: Option<&str>) -> Vec<&'a Task> {
    tasks
        .iter()
        .filter(|t| matches!(t.kind.as_str(), "agent" | "workflow" | "remote"))
        .filter(|t| {
            t.running()
                || viewing == Some(t.id.as_str())
                || t.ended_ms
                    .is_some_and(|e| now.saturating_sub(e) < STRIP_KEEP_MS)
        })
        .collect()
}

/// Claude Code's strip under the footer while agents run: a blank row, `● main`, then a row
/// an agent or workflow, `❯` on the focused one (`focus`, 0 being `main`). The agent in view
/// is the bold one with a green `●`; `main` otherwise.
pub fn strip(
    tasks: &[Task],
    viewing: Option<&str>,
    focus: Option<usize>,
    w: usize,
    now: u64,
) -> Vec<Line> {
    let list = strip_tasks(tasks, now, viewing);
    if list.is_empty() {
        return Vec::new();
    }
    let mut out = vec![Vec::new()];
    let mark = |i: usize| if focus == Some(i) { "❯ " } else { "  " };
    if viewing.is_none() {
        out.push(line(format!("{}● main", mark(0)), bold_plain()));
    } else {
        out.push(line(format!("{}◯ main", mark(0)), fg(GREY)));
    }
    for (i, t) in list.iter().enumerate() {
        let here = viewing == Some(t.id.as_str());
        let (dot, dst) = if here {
            ("● ", fg(GREEN))
        } else if t.status == "completed" {
            ("◯ ", fg(GREEN))
        } else {
            ("◯ ", fg(GREY))
        };
        let name = if t.kind == "workflow" {
            if t.detail.is_empty() {
                t.description.clone()
            } else {
                t.detail.clone()
            }
        } else if t.detail.is_empty() {
            "agent".to_string()
        } else {
            t.detail.clone()
        };
        let mut right = Vec::new();
        if t.kind == "workflow" && t.agents > 0 {
            right.push(format!("{}/{}", t.agents_done, t.agents));
        }
        let run = runtime(t, now);
        if !run.is_empty() {
            right.push(run);
        }
        if t.tokens > 0 {
            right.push(format!("↓ {} tokens", tokens(t.tokens)));
        }
        let mut right = right.join(" · ");
        let lead = 2 + 2 + width(&name);
        // What goes between: a workflow's bar, an agent's latest call or its description.
        let middle = |room: usize| -> String {
            if t.kind == "workflow" {
                if room < 20 {
                    return String::new();
                }
                let filled = (t.agents_done * 20).checked_div(t.agents).unwrap_or(0) as usize;
                format!("{}{}", "▰".repeat(filled), "▱".repeat(20 - filled))
            } else {
                let what = if t.activity.is_empty() {
                    &t.description
                } else {
                    &t.activity
                };
                if room < 6 {
                    String::new()
                } else {
                    cut(what, room)
                }
            }
        };
        if lead + 2 + width(&right) > w {
            right = run_only(t, now);
        }
        let room = w.saturating_sub(lead + 2 + width(&right) + 1);
        let mid = middle(room);
        let gap = w
            .saturating_sub(lead + 2 + width(&mid) + width(&right))
            .max(1);
        let st = if here { bold_plain() } else { fg(GREY) };
        out.push(vec![
            (mark(i + 1).to_string(), PLAIN),
            (dot.to_string(), dst),
            (name, st),
            ("  ".into(), PLAIN),
            (mid, fg(GREY)),
            (" ".repeat(gap), PLAIN),
            (right, fg(GREY)),
        ]);
    }
    out.into_iter().map(|l| clip(l, w)).collect()
}

fn run_only(t: &Task, now: u64) -> String {
    runtime(t, now)
}

/// A line cut to `w` cells, no ellipsis.
fn clip(l: Line, w: usize) -> Line {
    let mut used = 0;
    let mut out = Vec::new();
    for (t, st) in l {
        let mut keep = String::new();
        for ch in t.chars() {
            let c = convo::cw(ch);
            if used + c > w {
                break;
            }
            used += c;
            keep.push(ch);
        }
        if !keep.is_empty() {
            out.push((keep, st));
        }
    }
    out
}

/// What the mode line says while the strip has the focus: the keys of the focused row.
pub fn strip_hint(tasks: &[Task], viewing: Option<&str>, focus: usize, now: u64) -> String {
    if focus == 0 {
        return if viewing.is_some() {
            "↑/↓ to select · Enter to view".into()
        } else {
            "↑/↓ to select".into()
        };
    }
    let list = strip_tasks(tasks, now, viewing);
    match list.get(focus - 1) {
        Some(t) if t.running() => "Enter to view · x to stop".into(),
        Some(_) => "Enter to view".into(),
        None => "↑/↓ to select".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convo::{Convo, plain};

    /// The wall clock the tests draw at: a minute after their tasks start.
    const NOW: u64 = 1_060_000;

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
            text(&draw(&c, &v, 80, 24, NOW)),
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
            text(&draw(&c, &inline, 40, 24, NOW)),
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
        let got = text(&draw(&c, &v, 40, 30, NOW));
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
            text(&draw(&empty, &View::default(), 80, 24, NOW))[3..],
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
        let got = text(&draw(&c, &v, 80, 40, NOW));
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
        let small = draw(&c, &v, 80, 24, NOW);
        assert!(small.len() <= 21, "{}", small.len());
        assert!(text(&small).last().unwrap().contains("x to stop"));
        // A monitor says Script, and a shell with nothing yet says so.
        let m = View {
            open: Some("m".into()),
            output: Some(Ok(String::new())),
            ..View::default()
        };
        let got = text(&draw(&c, &m, 40, 24, NOW));
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
        a.tokens = 237_500;
        a.tools = 51;
        a.model = "claude-haiku-5-5".into();
        c.tasks = vec![a];
        let v = View {
            open: Some("a".into()),
            runtime: "1m 39s".into(),
            ..View::default()
        };
        assert_eq!(
            text(&draw(&c, &v, 40, 24, NOW)),
            [
                "▔".repeat(40).as_str(),
                "   Explore › big doc survey",
                "   1m 0s · 237.5k tokens · 51 tools ·",
                "   Haiku 5.5",
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
                "   close · x to stop · f to foreground",
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

    fn workflow() -> (Convo, View) {
        let mut c = Convo::new("h", "s", "r");
        let mut w = task(
            "w",
            "workflow",
            "Two agents read copyright files of /usr/share/doc packages",
            "big-doc-license-survey",
            "running",
        );
        w.agents = 2;
        w.saveable = true;
        c.tasks = vec![w];
        let wf = parse_wf(&crate::json::parse(r#"{"ph":["Read licenses"],"ag":[{"l":"pkgs-161-210","ph":1,"st":"progress","m":"claude-haiku-5-5","tk":112300,"tc":5,"t0":1018000},{"l":"pkgs-211-260","ph":1,"st":"done","m":"claude-haiku-5-5","tk":146700,"tc":7,"t0":1018000,"ms":40000,"pp":"Read them","rp":"GPL"}]}"#).unwrap());
        let v = View {
            open: Some("w".into()),
            wf: Some(wf),
            ..View::default()
        };
        (c, v)
    }

    #[test]
    fn the_workflow_browser_puts_phases_beside_agents_and_stacks_them_when_narrow() {
        let (c, mut v) = workflow();
        let got = text(&draw(&c, &v, 80, 24, NOW));
        assert_eq!(
            got[..7],
            [
                "▔".repeat(80).as_str(),
                "",
                "   big-doc-license-survey",
                "   Two agents read copyright files of /usr/share/doc pa… · 1/2 agents · 1m 0s",
                "",
                "   ╭─ Phases ──────────────┬─ Read licenses · 2 agents ─────────────────────╮",
                "   │ ❯ 1 Read licenses 1/2 │  ● pkgs-161-210 Haiku 5.5 · 112.3k tokens  42s │",
            ]
        );
        assert_eq!(
            got[7],
            "   │                       │  ✔ pkgs-211-260 Haiku 5.5 · 146.7k tokens  40s │"
        );
        assert_eq!(
            got.last().unwrap(),
            "   ↑↓ select · x stop workflow · esc back · s save"
        );
        assert!(
            got.len() <= 21,
            "three rows of the transcript stay: {}",
            got.len()
        );
        // Under 60 columns the phases go above the agents.
        assert_eq!(
            text(&draw(&c, &v, 40, 24, NOW))[4..9],
            [
                "",
                "   ❯ 1 Read licenses 1/2",
                "   ╭─ Read licenses · 2 agents ─────╮",
                "   │  ● pkgs-161-210 Haiku 5.5… 42s │",
                "   │  ✔ pkgs-211-260 Haiku 5.5… 40s │",
            ]
        );
        // → then Down: the cursor in the agents; Enter: that agent's own pane.
        v.pane = Pane::Agents;
        v.agent = 1;
        let got = text(&draw(&c, &v, 80, 24, NOW));
        assert_eq!(
            got[7],
            "   │                       │ ❯✔ pkgs-211-260 Haiku 5.5 · 146.7k tokens  40s │"
        );
        assert_eq!(
            got.last().unwrap(),
            "   ↑↓ select · Enter to view · f filter · esc back · s save"
        );
        v.pane = Pane::Agent;
        let got = text(&draw(&c, &v, 80, 24, NOW));
        assert_eq!(
            got[6..14]
                .iter()
                .map(|l| l.split('│').nth(2).unwrap_or("").trim_end().to_string())
                .collect::<Vec<_>>(),
            [
                " pkgs-211-260",
                " done · 40s · 146.7k tokens · 7 tool calls ·",
                " Haiku 5.5",
                "",
                " Prompt",
                " Read them",
                "",
                " Result",
            ]
        );
        // A filter keeps the agents in one state.
        v.pane = Pane::Agents;
        v.filter = "done".into();
        let wf = v.wf.clone().unwrap();
        assert_eq!(wf_agents(&wf, &v).len(), 1);
        // `s save`'s form, at 40.
        v.save = Some(Save {
            name: "big-doc-license-survey".into(),
            ..Save::default()
        });
        assert_eq!(
            text(&draw(&c, &v, 40, 24, NOW))[5..],
            [
                "   Save workflow",
                "   Name: big-doc-license-survey",
                "   Project scope ·",
                "   .claude/workflows/big-doc-license-",
                "   survey.js",
                "",
                "   Enter to save · Tab to switch scope ·",
                "   Esc to cancel",
            ]
        );
        assert_eq!(file_name("Big Doc!"), "big-doc");
    }

    #[test]
    fn the_strip_lists_agents_and_workflows_under_main() {
        let (c, _) = workflow();
        let mut a = task("a", "agent", "big doc survey", "general-purpose", "running");
        a.activity = "Read(/usr/share/doc/libgnutls30t64/copyright)".into();
        a.tokens = 48_100;
        let mut wt = c.tasks[0].clone();
        wt.agents_done = 1;
        wt.tokens = 164_300;
        let mut gone = task("g", "agent", "old", "Explore", "completed");
        gone.ended_ms = Some(NOW - STRIP_KEEP_MS);
        let ts = vec![a, wt, gone];
        assert_eq!(
            text(&strip(&ts, None, Some(1), 78, NOW)),
            [
                "",
                "  ● main",
                "❯ ◯ general-purpose  Read(/usr/share/doc/libgnutls30t6… 1m 0s · ↓ 48.1k tokens",
                "  ◯ big-doc-license-survey  ▰▰▰▰▰▰▰▰▰▰▱▱▱▱▱▱▱▱▱▱ 1/2 · 1m 0s · ↓ 164.3k tokens",
            ]
        );
        // Narrow: what does not fit goes, the time last.
        assert_eq!(
            text(&strip(&ts, Some("a"), None, 38, NOW))[1..],
            [
                "  ◯ main",
                "  ● general-purpose  Read(/usr/… 1m 0s",
                "  ◯ big-doc-license-survey       1m 0s",
            ]
        );
        assert_eq!(strip_hint(&ts, None, 0, NOW), "↑/↓ to select");
        assert_eq!(
            strip_hint(&ts, Some("a"), 0, NOW),
            "↑/↓ to select · Enter to view"
        );
        assert_eq!(strip_hint(&ts, None, 1, NOW), "Enter to view · x to stop");
        assert!(strip(&[], None, None, 78, NOW).is_empty());
    }

    #[test]
    fn notices_while_the_view_is_open_are_counted_above_it() {
        let mut c = Convo::new("h", "s", "r");
        c.tasks = shells();
        c.notices = 3;
        let v = View {
            notices_at: 1,
            ..View::default()
        };
        assert_eq!(
            text(&draw(&c, &v, 80, 24, NOW))[0],
            "2 background task updates waiting while this panel is open"
        );
        let v = View {
            notices_at: 2,
            ..View::default()
        };
        assert_eq!(
            text(&draw(&c, &v, 40, 24, NOW))[0],
            "Background task update waiting while th…"
        );
    }
}
