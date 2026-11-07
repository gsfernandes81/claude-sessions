//! The holder: what keeps a headless claude alive in a remote-view slot, and the tap that
//! reaches it (`docs/remote-view.md` § *The pieces*).
//!
//! `claude-sessions holder [args…]` is a zmx session's program, as claude is a normal slot's:
//! zmx keeps it alive, lists it, and gives the offloader its pid tree. It starts claude with
//! the headless flags (`HEADLESS`) on pipes, so claude is its child:
//! `zmx` → `claude-sessions holder` → `claude`, which `bind.rs` admits as the slot's own.
//!
//! **Every line claude writes is numbered and appended to `<registry>/<slot>.log`**, raw, so
//! a later, better reducer can read it again. Only each line's offset is kept in memory: a long
//! session's tool results run to megabytes, and the container's RAM is the hard limit.
//!
//! **Taps connect on `<registry>/<slot>.sock`.** A tap says where it got to (`{"from":N,
//! "run":R}`); the holder answers with its own run and next number, sends every line from
//! there as `N<TAB>line`, then follows as claude writes. A run is one holder's life: numbers
//! restart with it, so a tap that names an older run is sent everything again. Lines a tap
//! sends are claude's input; each must be one JSON object, and they reach claude whole and one
//! at a time, whichever tap they came from.
//!
//! **A resumed conversation's history is not copied into the log.** The holder notes how long
//! the transcript was when it started (claude appends to the same file from there) and serves
//! it backwards on request, whole turns at a time: a tap's `{"type":"_older","before":B,
//! "want":N}` is answered, never passed to claude, with one line `H<TAB>{"type":"_older",
//! "before":B2,"more":…,"lines":[…]}` holding at least N of its prompt and reply lines before
//! byte B, back to the prompt that starts a turn. The header says `"older":true` when there
//! is history to ask for (`docs/remote-view.md` § *Lazy history*).
//!
//! A tap's `{"type":"_save_workflow","name","scope","script"}` is the holder's too: the script
//! written where Claude Code's workflow browser saves one, answered `H<TAB>{"type":"_saved",
//! "path"|"error"}` (`save_workflow`).
//!
//! **A model a view picks becomes the default for new sessions**, as Claude Code's own
//! `/model` makes it in a terminal (owner, 2026-11-06). A headless claude only switches the
//! session, so once claude accepts a view's `set_model` the holder writes it as `model` in
//! `$CLAUDE_CONFIG_DIR/settings.json` (`save_default_model`).
//!
//! **What the zmx session shows**: one plain line at start and at each change of what claude
//! is doing, never on a timer, so attaching from the old menu explains the slot rather than
//! showing a blank screen. Whatever is typed into the session is read and dropped, since
//! nothing typed there is meant for claude, which reads the tap instead.
//!
//! The holder takes no registry lock: the tap's writes to the slot's record do, with a timeout
//! (`Viewing`): `keep_until` re-stamped while a view is connected, and `last_attach` when it
//! connects and when it goes.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::FileExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::clock::{self, Millis};
use crate::json::{self, Value};
use crate::lockfile::{INTERACTIVE_WAIT, SlotLock};
use crate::registry;
use crate::sdk::{self, Msg};

/// The flags that make claude headless and let a host drive it, measured on 2.1.311
/// (`docs/remote-view.md` § *The protocol, measured*). `--permission-prompt-tool stdio` is
/// what sends permission prompts and `AskUserQuestion` to the host instead of denying them;
/// `--replay-user-messages` echoes each prompt back, so every view sees what any sent.
pub const HEADLESS: [&str; 10] = [
    "-p",
    "--input-format",
    "stream-json",
    "--output-format",
    "stream-json",
    "--verbose",
    "--include-partial-messages",
    "--permission-prompt-tool",
    "stdio",
    "--replay-user-messages",
];

/// What claude is told it is run by: Claude Desktop's own name for a claude it drives over ssh,
/// which is what this is. Headless claude leaves the Artifact tools out for `sdk-cli`, the
/// entrypoint `-p` gives itself, and offers them for this one (2.1.311, tried 2026-11-02:
/// `Artifact`, `ArtifactComments` and `ArtifactData` listed, and `list` answered); the owner
/// found artifacts missing from remote sessions.
pub const ENTRYPOINT: &str = "ssh-remote";

/// How long a tap has to say where it got to before the holder drops it.
const HELLO_WAIT: Duration = Duration::from_secs(5);

pub struct Paths {
    pub sock: PathBuf,
    pub log: PathBuf,
}

/// `<registry>/<slot>.sock` and `<registry>/<slot>.log`.
pub fn paths(slot: &str) -> Paths {
    let dir = registry::dir();
    Paths {
        sock: dir.join(format!("{slot}.sock")),
        log: dir.join(format!("{slot}.log")),
    }
}

/// How many bytes the transcript is read backwards at a time.
const BACK_BLOCK: u64 = 64 * 1024;

/// A resumed conversation's transcript lines before byte `before`, oldest first: its prompts
/// and replies, at least `want` of them when there are, back to a prompt that starts a turn,
/// so a tool call is never cut from its result. Returns them, the byte the first one starts at,
/// and whether anything is left before it.
pub fn older(transcript: &Path, before: u64, want: usize) -> io::Result<(Vec<Value>, u64, bool)> {
    let f = File::open(transcript)?;
    let before = before.min(f.metadata()?.len());
    let mut lines: Vec<Value> = Vec::new();
    let mut pos = before;
    // The bytes after `pos` not yet split off as a whole line: the start of a line whose
    // beginning is further back.
    let mut tail: Vec<u8> = Vec::new();
    loop {
        let from = pos.saturating_sub(BACK_BLOCK);
        let mut block = vec![0u8; (pos - from) as usize];
        f.read_exact_at(&mut block, from)?;
        block.extend_from_slice(&tail);
        tail.clear();
        // Every line that ends in this block, newest first; the first piece, unless the file
        // starts here, is a line still going further back.
        let mut end = block.len();
        loop {
            let start = match block[..end].iter().rposition(|&b| b == b'\n') {
                Some(nl) => nl + 1,
                None if from == 0 => 0,
                None => {
                    tail = block[..end].to_vec();
                    break;
                }
            };
            let at = from + start as u64;
            if let Ok(v) = std::str::from_utf8(&block[start..end])
                .map_err(|_| ())
                .and_then(|t| json::parse(t.trim()).map_err(|_| ()))
            {
                if matches!(
                    v.get("type").and_then(Value::as_str),
                    Some("user" | "assistant")
                ) {
                    let turn = sdk::starts_turn(&v);
                    lines.push(v);
                    if turn && lines.len() >= want {
                        lines.reverse();
                        return Ok((lines, at, at > 0));
                    }
                }
            }
            if start == 0 {
                break;
            }
            end = start - 1;
        }
        if from == 0 {
            lines.reverse();
            return Ok((lines, 0, false));
        }
        pos = from;
    }
}

/// Whether a holder is serving `slot` now: its socket answers. What the record's `driver`
/// says, checked where it matters: a release from before the holder rewrites a record
/// without that field, and a socket answering cannot be left over.
pub fn serving(slot: &str) -> bool {
    UnixStream::connect(paths(slot).sock).is_ok()
}

/// The log as the holder keeps it: where each line starts, and whether claude has gone.
struct Log {
    file: File,
    /// `offsets[i]` is where line `i + 1` starts; `end` is where the next one will.
    offsets: Vec<u64>,
    end: u64,
    closed: bool,
    /// The request ids a view has answered: a second answer to one is dropped, so the first
    /// answer wins whichever view sent it (`docs/remote-view.md` § *Decisions*).
    answered: HashSet<String>,
    /// The prompts and questions claude is waiting on, by request id: the amber `!` is
    /// cleared once a view has answered the last of them (`asked`, `settled`).
    open: HashSet<String>,
    /// The models a view asked claude to switch to, by request id, until claude answers:
    /// one it accepts is saved as the default (`save_default_model`).
    models: HashMap<String, String>,
}

impl Log {
    /// Append one line, newline-ended, and number it.
    fn append(&mut self, line: &[u8]) -> io::Result<()> {
        self.file.write_all(line)?;
        if line.last() != Some(&b'\n') {
            self.file.write_all(b"\n")?;
        }
        let len = line.len() as u64 + u64::from(line.last() != Some(&b'\n'));
        self.offsets.push(self.end);
        self.end += len;
        Ok(())
    }

    /// Note what claude wrote: a prompt or question opens, a withdrawal or the end of the
    /// turn settles. True when that leaves nothing open that was open before.
    fn asked(&mut self, msg: &Msg) -> bool {
        match msg {
            Msg::Permission { request_id, .. } | Msg::Question { request_id, .. } => {
                self.open.insert(request_id.clone());
                false
            }
            Msg::Cancelled { request_id } => self.settled(request_id),
            // The turn's end is claude's `Stop`, which clears it already.
            Msg::Result { .. } => {
                self.open.clear();
                false
            }
            _ => false,
        }
    }

    /// Note a view's `set_model`, to save once claude accepts it. Only so many outstanding:
    /// claude answers each, but a view that asks and asks while claude is stuck must not
    /// grow this.
    fn model_asked(&mut self, v: &Value) {
        if let Some((id, model)) = set_model(v) {
            if self.models.len() < 64 {
                self.models.insert(id, model);
            }
        }
    }

    /// The model to save as the default, when `msg` is claude accepting a view's `set_model`.
    fn model_answered(&mut self, msg: &Msg) -> Option<String> {
        match msg {
            Msg::Response { request_id, ok, .. } => self.models.remove(request_id).filter(|_| *ok),
            _ => None,
        }
    }

    /// A request answered or withdrawn: true when it was open and was the last one.
    fn settled(&mut self, request_id: &str) -> bool {
        self.open.remove(request_id) && self.open.is_empty()
    }
}

struct Shared {
    log: Mutex<Log>,
    /// A resumed conversation's transcript, and how long it was when this holder started:
    /// what came before, served backwards (`older`).
    history: Option<(PathBuf, u64)>,
    grew: Condvar,
    log_path: PathBuf,
    run: String,
    claude: Mutex<ChildStdin>,
    slot: String,
}

/// Run claude headless under this holder until it exits, serving taps meanwhile. Returns
/// claude's exit status. `say` is given each line for the session's screen. `history` is the
/// transcript of a conversation resumed (`CLAUDE_SESSIONS_HISTORY`, read by the caller: a
/// test run from a remote-view session has it set to that session's own).
pub fn serve(
    program: &Path,
    args: &[String],
    paths: &Paths,
    history: Option<&Path>,
    slot: &str,
    say: &mut dyn FnMut(&str),
) -> io::Result<i32> {
    if let Some(dir) = paths.log.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let log_file = File::create(&paths.log)?;
    // A socket left by a holder that died is in the way; one that is live cannot be, since a
    // slot's name is free when it starts.
    let _ = std::fs::remove_file(&paths.sock);
    let listener = UnixListener::bind(&paths.sock)?;
    let mut log = Log {
        file: log_file,
        offsets: Vec::new(),
        end: 0,
        closed: false,
        answered: HashSet::new(),
        open: HashSet::new(),
        models: HashMap::new(),
    };
    // A resume: the conversation so far is its transcript up to here, since claude sends none
    // of it again; claude appends to the same file from now on.
    let history = history.and_then(|p| {
        let len = std::fs::metadata(p).ok()?.len();
        (len > 0).then(|| (p.to_path_buf(), len))
    });

    // The model and effort claude starts with, as it decides them, first in the log: claude's
    // own lines never say the alias or the effort, so `/model` and `/effort` could not mark
    // the session's own without it.
    let cwd = std::env::current_dir().unwrap_or_default();
    let (model, effort) = configured(args, &|k| std::env::var(k).ok(), &settings_layers(&cwd));
    {
        let mut start = Value::obj();
        start.set("type", Value::string("_cfg"));
        start.set("model", Value::string(model));
        start.set("effort", Value::string(effort));
        let mut l = json::to_line(&start).into_bytes();
        l.push(b'\n');
        let _ = log.append(&l);
    }
    let mut child: Child = Command::new(program)
        .args(HEADLESS)
        .args(args)
        .env("CLAUDE_CODE_ENTRYPOINT", ENTRYPOINT)
        // A short summary of each step's tool calls (`tool_use_summary`), which the view
        // draws in place of counting them, as Claude's own SDK hosts do (owner, 2026-11-06).
        .env("CLAUDE_CODE_EMIT_TOOL_USE_SUMMARIES", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    let stdout = child.stdout.take().expect("piped");
    let stdin = child.stdin.take().expect("piped");
    let run = format!("{}-{}", std::process::id(), crate::clock::now());
    let shared = Arc::new(Shared {
        log: Mutex::new(log),
        history,
        grew: Condvar::new(),
        log_path: paths.log.clone(),
        run,
        claude: Mutex::new(stdin),
        slot: slot.to_string(),
    });

    // Typed into the session: read, and dropped.
    std::thread::spawn(|| {
        let _ = io::copy(&mut io::stdin().lock(), &mut io::sink());
    });
    {
        let shared = Arc::clone(&shared);
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(conn) = conn else { continue };
                let shared = Arc::clone(&shared);
                std::thread::spawn(move || {
                    let _ = serve_tap(conn, &shared);
                });
            }
        });
    }

    say(&format!("{slot}: open in the remote view; starting"));
    let mut said = String::from("starting");
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        let msg = std::str::from_utf8(&line)
            .ok()
            .and_then(|l| json::parse(l.trim()).ok())
            .map(|v| sdk::classify(&v));
        let (settled, model) = {
            let mut log = shared.log.lock().unwrap_or_else(|e| e.into_inner());
            log.append(&line)?;
            let model = msg.as_ref().and_then(|m| log.model_answered(m));
            (msg.as_ref().is_some_and(|m| log.asked(m)), model)
        };
        shared.grew.notify_all();
        if let Some(model) = model {
            // Best effort, as the session has switched either way: a settings file this
            // cannot read is left alone rather than written over.
            let _ = save_default_model(&crate::live::config_dir().join("settings.json"), &model);
        }
        if settled {
            prompt_answered(&shared.slot);
        }
        if let Some(doing) = next_said(&said, msg.as_ref()) {
            if doing != said {
                say(&format!("{slot}: open in the remote view; {doing}"));
                said = doing.to_string();
            }
        }
    }
    shared.log.lock().unwrap_or_else(|e| e.into_inner()).closed = true;
    shared.grew.notify_all();
    let status = child.wait()?;
    let _ = std::fs::remove_file(&paths.sock);
    say(&format!("{slot}: claude has ended"));
    Ok(status.code().unwrap_or(1))
}

/// What a line says claude is doing, for the session's screen, when it says anything.
/// What the zmx session's line says after `line`, if that changes. Anything claude writes
/// means it has started: a resumed claude says nothing more until it is prompted, and
/// "starting" stood on the screen until then.
fn next_said(said: &str, msg: Option<&Msg>) -> Option<&'static str> {
    msg.and_then(doing)
        .or((said == "starting").then_some("idle"))
}

fn doing(msg: &Msg) -> Option<&'static str> {
    match msg {
        Msg::Prompt(_) | Msg::Status(Some(_)) => Some("working"),
        Msg::Permission { .. } | Msg::Question { .. } => Some("waiting for you"),
        Msg::Result { .. } => Some("idle"),
        _ => None,
    }
}

/// One tap: its hello, then the log from where it said, then whatever claude writes; and its
/// lines, meanwhile, to claude.
fn serve_tap(conn: UnixStream, shared: &Arc<Shared>) -> io::Result<()> {
    conn.set_read_timeout(Some(HELLO_WAIT))?;
    let mut input = BufReader::new(conn.try_clone()?);
    let mut hello = String::new();
    input.read_line(&mut hello)?;
    conn.set_read_timeout(None)?;
    let hello = json::parse(hello.trim()).unwrap_or(Value::Null);
    let same_run = hello.get("run").and_then(Value::as_str) == Some(shared.run.as_str());
    let from = if same_run {
        hello
            .get("from")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .max(1)
    } else {
        1
    };
    // Written to by this thread and by the one answering `_older`: a line at a time.
    let out = Arc::new(Mutex::new(conn.try_clone()?));
    {
        let next = shared
            .log
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .offsets
            .len()
            + 1;
        let mut header = Value::obj();
        header.set("run", Value::string(shared.run.clone()));
        header.set("next", Value::num(next as f64));
        header.set("older", Value::Bool(shared.history.is_some()));
        lock(&out).write_all(format!("{}\n", json::to_line(&header)).as_bytes())?;
    }

    {
        let shared = Arc::clone(shared);
        let out = Arc::clone(&out);
        std::thread::spawn(move || {
            let mut line = String::new();
            loop {
                line.clear();
                match input.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                // Only a whole JSON object reaches claude: anything else would end its input.
                let Ok(v @ Value::Obj(_)) = json::parse(line.trim()) else {
                    continue;
                };
                // The holder's own, not claude's: history before a point.
                if v.get("type").and_then(Value::as_str) == Some("_older") {
                    if answer_older(&v, &shared, &out).is_err() {
                        break;
                    }
                    continue;
                }
                // And a workflow's script saved, as Claude Code's `s save` saves it.
                if v.get("type").and_then(Value::as_str) == Some("_save_workflow") {
                    let home = std::env::var_os("HOME").map(PathBuf::from);
                    let here = std::env::current_dir().ok();
                    let reply = save_workflow(&v, here.as_deref(), home.as_deref());
                    let line = format!("H\t{}\n", json::to_line(&reply));
                    if lock(&out).write_all(line.as_bytes()).is_err() {
                        break;
                    }
                    continue;
                }
                if from_view(&v, &shared).is_err() {
                    break;
                }
            }
        });
    }

    let log_file = File::open(&shared.log_path)?;
    let mut sent = (from - 1) as usize;
    loop {
        let (range, closed) = {
            let mut log = shared.log.lock().unwrap_or_else(|e| e.into_inner());
            while sent >= log.offsets.len() && !log.closed {
                log = shared.grew.wait(log).unwrap_or_else(|e| e.into_inner());
            }
            let range: Vec<(u64, u64)> = (sent..log.offsets.len())
                .map(|i| {
                    let start = log.offsets[i];
                    let end = log.offsets.get(i + 1).copied().unwrap_or(log.end);
                    (start, end)
                })
                .collect();
            (range, log.closed)
        };
        for (start, end) in range {
            let mut buf = format!("{}\t", sent + 1).into_bytes();
            let head = buf.len();
            buf.resize(head + (end - start) as usize, 0);
            log_file.read_exact_at(&mut buf[head..], start)?;
            sent += 1;
            lock(&out).write_all(&buf)?;
        }
        if closed {
            // Both ways: the thread reading this tap's lines holds the socket too, and the tap
            // must see the end now, not when it next writes.
            let _ = lock(&out).shutdown(std::net::Shutdown::Both);
            return Ok(());
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A workflow's name as a file name, as Claude Code makes one: lower case, letters, digits,
/// `-` and `_`, anything else a `-`; `workflow` when nothing is left.
fn workflow_file_name(name: &str) -> String {
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

/// A view's `{"type":"_save_workflow","name","scope","script"}`: the script written to
/// `.claude/workflows/<name>.js` under claude's directory (`project`), or under the home
/// (`user`), where Claude Code's `s save` puts it, never over a file already there. Answered
/// `{"type":"_saved","path"}` or `{"type":"_saved","error"}`.
fn save_workflow(v: &Value, here: Option<&Path>, home: Option<&Path>) -> Value {
    let mut reply = Value::obj();
    reply.set("type", Value::string("_saved"));
    let get = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("");
    let base = if get("scope") == "user" { home } else { here };
    let script = get("script");
    let result = (|| -> Result<PathBuf, String> {
        let base = base.ok_or("no directory to save it in")?;
        if script.trim().is_empty() {
            return Err("the workflow has no script".into());
        }
        let dir = base.join(".claude").join("workflows");
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let path = dir.join(format!("{}.js", workflow_file_name(get("name"))));
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| match e.kind() {
                io::ErrorKind::AlreadyExists => format!("{} already exists", path.display()),
                _ => e.to_string(),
            })?;
        f.write_all(script.as_bytes()).map_err(|e| e.to_string())?;
        Ok(path)
    })();
    match result {
        Ok(p) => reply.set("path", Value::string(p.display().to_string())),
        Err(e) => reply.set("error", Value::string(e)),
    }
    reply
}

/// How many transcript lines a tap may ask for at once.
const OLDER_MAX: usize = 2000;

/// A tap's `{"type":"_older","before":B,"want":N}`: the history before byte B (before the end
/// of what this holder started with, without B), as one `H<TAB>` line.
fn answer_older(v: &Value, shared: &Shared, out: &Mutex<UnixStream>) -> io::Result<()> {
    let mut reply = Value::obj();
    reply.set("type", Value::string("_older"));
    match &shared.history {
        Some((path, len)) => {
            let before = v.get("before").and_then(Value::as_u64).unwrap_or(*len);
            let want = v
                .get("want")
                .and_then(Value::as_u64)
                .unwrap_or(40)
                .clamp(1, OLDER_MAX as u64) as usize;
            match older(path, before.min(*len), want) {
                Ok((lines, at, more)) => {
                    reply.set("before", Value::num(at as f64));
                    reply.set("more", Value::Bool(more));
                    reply.set("lines", Value::Arr(lines));
                }
                Err(e) => {
                    reply.set("more", Value::Bool(false));
                    reply.set("lines", Value::Arr(Vec::new()));
                    reply.set("error", Value::string(e.to_string()));
                }
            }
        }
        None => {
            reply.set("more", Value::Bool(false));
            reply.set("lines", Value::Arr(Vec::new()));
        }
    }
    lock(out).write_all(format!("H\t{}\n", json::to_line(&reply)).as_bytes())
}

/// A line a view sent: to claude, and into the log as `{"type":"_in","line":…}`, so every
/// view, and a relay reading the log again after a reconnect, sees what was answered and
/// asked, ahead of anything claude does with it. Serialised by claude's input lock, so the
/// log's order of them is the order claude was given them; the log's own lock is never held while writing to claude, whose output
/// the holder must keep reading meanwhile. An answer to a request already answered is
/// dropped, from claude and the log alike.
fn from_view(v: &Value, shared: &Shared) -> io::Result<()> {
    let mut claude = shared.claude.lock().unwrap_or_else(|e| e.into_inner());
    let mut settled = false;
    if v.get("type").and_then(Value::as_str) == Some("control_response") {
        let id = v
            .get("response")
            .and_then(|r| r.get("request_id"))
            .and_then(Value::as_str);
        if let Some(id) = id {
            let mut log = shared.log.lock().unwrap_or_else(|e| e.into_inner());
            if !log.answered.insert(id.to_string()) {
                return Ok(());
            }
            settled = log.settled(id);
        }
    }
    shared
        .log
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .model_asked(v);
    // Logged first, so whatever claude does with it comes after it in the log.
    let mut logged = Value::obj();
    logged.set("type", Value::string("_in"));
    logged.set("line", v.clone());
    shared
        .log
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .append(json::to_line(&logged).as_bytes())?;
    shared.grew.notify_all();
    claude.write_all(format!("{}\n", json::to_line(v)).as_bytes())?;
    drop(claude);
    if settled {
        prompt_answered(&shared.slot);
    }
    Ok(())
}

/// Claude Code's settings files that can set `model` or `effortLevel`, lowest precedence
/// first, each with whether it is the user's own: the user's, the project's, the project's
/// local one, then the managed file and its drop-ins in name order (managed outranks the rest,
/// local outranks project outranks user). A file that is missing or is not JSON is skipped.
fn settings_layers(cwd: &Path) -> Vec<(bool, Value)> {
    let mut paths = vec![
        crate::live::config_dir().join("settings.json"),
        cwd.join(".claude/settings.json"),
        cwd.join(".claude/settings.local.json"),
        PathBuf::from("/etc/claude-code/managed-settings.json"),
    ];
    let mut drop_ins: Vec<PathBuf> = std::fs::read_dir("/etc/claude-code/managed-settings.d")
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("json"))
        .collect();
    drop_ins.sort();
    paths.extend(drop_ins);
    paths
        .iter()
        .enumerate()
        .filter_map(|(i, p)| Some((i == 0, std::fs::read_to_string(p).ok()?)))
        .filter_map(|(user, t)| Some((user, json::parse(&t).ok()?)))
        .collect()
}

/// The model alias and effort claude starts with, decided as it decides them, measured
/// against 2.1.311 (2026-11-07, held by `tests/claude_model.rs`), which decides the two
/// differently:
///
/// - **model**: `--model`, then `ANTHROPIC_MODEL`, then the highest settings layer that sets
///   `model`, else `default`;
/// - **effort**: `CLAUDE_CODE_EFFORT_LEVEL`, then `--effort`, then the highest layer but the
///   user's that sets `effortLevel` (headless claude ignores the user's), else empty: the
///   model's own.
///
/// `layers` are lowest first, each with whether it is the user's (`settings_layers`).
fn configured(
    args: &[String],
    env: &dyn Fn(&str) -> Option<String>,
    layers: &[(bool, Value)],
) -> (String, String) {
    let flag = |name: &str| -> Option<String> {
        let mut it = args.iter();
        while let Some(a) = it.next() {
            if a == name {
                return it.next().cloned();
            }
            if let Some(v) = a.strip_prefix(&format!("{name}=")) {
                return Some(v.to_string());
            }
        }
        None
    };
    let setting = |key: &str, with_user: bool| -> Option<String> {
        layers
            .iter()
            .rev()
            .filter(|(user, _)| with_user || !user)
            .find_map(|(_, l)| l.get(key).and_then(Value::as_str).map(str::to_string))
    };
    let nonempty = |v: Option<String>| v.filter(|v| !v.trim().is_empty());
    let model = nonempty(flag("--model"))
        .or_else(|| nonempty(env("ANTHROPIC_MODEL")))
        .or_else(|| nonempty(setting("model", true)))
        .unwrap_or_else(|| "default".into());
    let effort = nonempty(env("CLAUDE_CODE_EFFORT_LEVEL"))
        .or_else(|| nonempty(flag("--effort")))
        .or_else(|| nonempty(setting("effortLevel", false)))
        .unwrap_or_default();
    (model, effort)
}

/// A view's `set_model` request: its id and the model it names.
fn set_model(v: &Value) -> Option<(String, String)> {
    let req = v.get("request")?;
    if v.get("type").and_then(Value::as_str) != Some("control_request")
        || req.get("subtype").and_then(Value::as_str) != Some("set_model")
    {
        return None;
    }
    let id = v.get("request_id").and_then(Value::as_str)?;
    let model = req.get("model").and_then(Value::as_str).unwrap_or_default();
    Some((id.to_string(), model.to_string()))
}

/// Save `model` as the default for new sessions: `model` in Claude Code's user settings
/// file, where its own `/model` writes it. `default`, or none, takes the key out, as picking
/// Default there does. The other settings are kept; a file that is not a JSON object is an
/// error and is not touched. Written whole to a temporary file and renamed over, so claude
/// never reads half of it. Our writer sorts keys, so the file's keys may come back reordered.
pub(crate) fn save_default_model(path: &Path, model: &str) -> io::Result<()> {
    // A linked file is written where it points, so the link stays a link.
    let path = &std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mut settings = match std::fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => Value::obj(),
        Ok(text) => json::parse(&text).map_err(io::Error::other)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => Value::obj(),
        Err(e) => return Err(e),
    };
    let Value::Obj(map) = &mut settings else {
        return Err(io::Error::other("settings are not a JSON object"));
    };
    let changed = if model.is_empty() || model == "default" {
        map.remove("model").is_some()
    } else {
        map.insert("model".into(), Value::string(model)) != Some(Value::string(model))
    };
    if !changed {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(&tmp, json::to_string_pretty(&settings))?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// The last prompt or question claude was waiting on is answered: clear the slot's amber `!`
/// now rather than at the turn's end. Answering fires no hook in claude, so until this the `!`
/// stood through the rest of the turn, which the owner read as the question still asked. Read
/// as pi's `PromptAnswered` (`events.rs`), stamped now, so a prompt claude asks later wins.
/// On its own thread, skipped when the lock is not had or the slot has no record: what claude
/// is told never waits on the registry.
fn prompt_answered(slot: &str) {
    let fired = clock::Moment::now();
    let slot = slot.to_string();
    std::thread::spawn(move || {
        let Ok(_lock) = SlotLock::acquire(&registry::lock_path(&slot), INTERACTIVE_WAIT) else {
            return;
        };
        let Ok(Some(mut rec)) = registry::load(&slot) else {
            return;
        };
        let ev = crate::events::Event::parse(r#"{"hook_event_name":"PromptAnswered"}"#)
            .expect("a fixed payload");
        let own = crate::events::Binding::Own;
        if crate::events::apply(&mut rec, &ev, fired, own, None, None)
            == crate::events::Outcome::Changed
        {
            rec.updated_ms = clock::now();
            let _ = registry::store(&rec);
        }
    });
}

/// `claude-sessions tap <slot> [from] [run]`: what the hub runs over `ssh -T` to reach a
/// remote slot. Its stdin goes to the holder and the holder's lines come out on stdout, so ssh
/// carries them; it ends when either side does.
pub fn tap(slot: &str, from: u64, run: &str) -> io::Result<()> {
    let p = paths(slot);
    let conn = UnixStream::connect(&p.sock)?;
    let _viewing = Viewing::start(slot);
    let mut hello = Value::obj();
    hello.set("from", Value::num(from as f64));
    hello.set("run", Value::string(run));
    (&conn).write_all(format!("{}\n", json::to_line(&hello)).as_bytes())?;
    let mut to_holder = conn.try_clone()?;
    // The end of input is the relay gone, and with it the view: the tap goes too, both ways,
    // or it would keep the slot loaded for nobody (`Viewing`).
    std::thread::spawn(move || {
        let _ = io::copy(&mut io::stdin().lock(), &mut to_holder);
        let _ = to_holder.shutdown(std::net::Shutdown::Both);
    });
    let mut from_holder = conn;
    let mut buf = [0u8; 16 * 1024];
    let mut stdout = io::stdout().lock();
    loop {
        let n = match from_holder.read(&mut buf) {
            Ok(n) => n,
            // Shut down from the input side while a read was waiting.
            Err(e) if e.kind() == io::ErrorKind::NotConnected => 0,
            Err(e) => return Err(e),
        };
        if n == 0 {
            return Ok(());
        }
        stdout.write_all(&buf[..n])?;
        stdout.flush()?;
    }
}

/// How long past its last stamp a connected view keeps the slot from being offloaded, and how
/// often it stamps again: a tap killed outright leaves at most this much keep-alive behind.
const KEEP_FOR: Millis = 3 * 60 * 1000;
const RESTAMP: Duration = Duration::from_secs(60);

/// A view connected to a slot, as the registry hears of it (`docs/remote-view.md` §
/// *Decisions*: a connected view keeps the slot loaded). zmx never counts a remote slot as
/// attached, so while a tap runs it re-stamps `keep_until` as `keepalive` would, and
/// `last_attach` is written when it connects and when it goes, which keeps unread right:
/// whatever stopped while it was connected, the owner saw.
struct Viewing {
    stop: Arc<(Mutex<bool>, Condvar)>,
    slot: String,
}

impl Viewing {
    fn start(slot: &str) -> Viewing {
        stamp(slot, true);
        let stop = Arc::new((Mutex::new(false), Condvar::new()));
        let (s, theirs) = (slot.to_string(), Arc::clone(&stop));
        std::thread::spawn(move || {
            let (done, cv) = &*theirs;
            let mut done = done.lock().unwrap_or_else(|e| e.into_inner());
            while !*done {
                done = cv
                    .wait_timeout(done, RESTAMP)
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
                if !*done {
                    stamp(&s, false);
                }
            }
        });
        Viewing {
            stop,
            slot: slot.to_string(),
        }
    }
}

impl Drop for Viewing {
    fn drop(&mut self) {
        let (done, cv) = &*self.stop;
        *done.lock().unwrap_or_else(|e| e.into_inner()) = true;
        cv.notify_all();
        stamp(&self.slot, true);
    }
}

/// Keep the slot for `KEEP_FOR` from now, and, with `attach`, record the owner as having seen
/// it now. Under the slot's lock with the interactive timeout; a lock not had or a record not
/// there is skipped, never waited on, since the stream matters more than the stamp.
pub(crate) fn stamp(slot: &str, attach: bool) {
    let Ok(_lock) = SlotLock::acquire(&registry::lock_path(slot), INTERACTIVE_WAIT) else {
        return;
    };
    let Ok(Some(mut rec)) = registry::load(slot) else {
        return;
    };
    let now = clock::now();
    let keep = now + KEEP_FOR;
    // A longer keep-alive asked for from inside the slot stands.
    if rec.keep_until_ms.is_none_or(|k| k < keep) {
        rec.keep_until_ms = Some(keep);
    }
    if attach {
        rec.last_attach_ms = now;
    }
    rec.updated_ms = now;
    let _ = registry::store(&rec);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// A stand-in claude: echoes each line it is given back as an assistant-shaped line, and
    /// ends on `{"quit":1}`. Ignores the headless flags, as a stand-in may.
    fn stand_in(dir: &Path) -> PathBuf {
        let p = dir.join("claude");
        std::fs::write(
            &p,
            "#!/bin/sh\n\
             while IFS= read -r l; do\n\
               case \"$l\" in *quit*) exit 3;; \
                 *entrypoint*) printf '{\"ep\":\"%s\"}\\n' \"$CLAUDE_CODE_ENTRYPOINT\"; continue;; esac\n\
               printf '%s\\n' \"$l\"\n\
             done\n",
        )
        .unwrap();
        let mut perm = std::fs::metadata(&p).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o755);
        std::fs::set_permissions(&p, perm).unwrap();
        p
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cs-holder-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_workflow_is_saved_where_claude_code_saves_it_and_never_over_another() {
        let d = scratch("save-wf");
        let (proj, home) = (d.join("proj"), d.join("home"));
        let req = |name: &str, scope: &str| {
            json::parse(&format!(
                r#"{{"type":"_save_workflow","name":"{name}","scope":"{scope}","script":"export const meta = {{}}\n"}}"#
            ))
            .unwrap()
        };
        let r = save_workflow(&req("Two Readers!", "project"), Some(&proj), Some(&home));
        let want = proj.join(".claude/workflows/two-readers.js");
        assert_eq!(
            r.get("path").and_then(Value::as_str),
            Some(want.display().to_string().as_str())
        );
        assert_eq!(
            std::fs::read_to_string(&want).unwrap(),
            "export const meta = {}\n"
        );
        let again = save_workflow(&req("two-readers", "project"), Some(&proj), Some(&home));
        assert!(
            again
                .get("error")
                .and_then(Value::as_str)
                .is_some_and(|e| e.ends_with("already exists")),
            "{}",
            json::to_line(&again)
        );
        let user = save_workflow(&req("../x", "user"), Some(&proj), Some(&home));
        assert!(
            home.join(".claude/workflows/x.js").exists(),
            "{}",
            json::to_line(&user)
        );
        assert_eq!(workflow_file_name("  "), "workflow");
        let _ = std::fs::remove_dir_all(&d);
    }

    fn connect(sock: &Path, from: u64, run: &str) -> (BufReader<UnixStream>, UnixStream) {
        for _ in 0..200 {
            if let Ok(c) = UnixStream::connect(sock) {
                c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                (&c).write_all(format!("{{\"from\":{from},\"run\":\"{run}\"}}\n").as_bytes())
                    .unwrap();
                return (BufReader::new(c.try_clone().unwrap()), c);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the holder never listened");
    }

    fn line(r: &mut BufReader<UnixStream>) -> String {
        let mut s = String::new();
        r.read_line(&mut s).expect("a line before the timeout");
        s.trim_end().to_string()
    }

    #[test]
    fn taps_get_every_line_numbered_and_a_late_one_catches_up_from_where_it_says() {
        let dir = scratch("taps");
        let program = stand_in(&dir);
        let paths = Paths {
            sock: dir.join("s.sock"),
            log: dir.join("s.log"),
        };
        let (said_tx, said_rx) = mpsc::channel();
        let sock = paths.sock.clone();
        let log = paths.log.clone();
        let holder = std::thread::spawn(move || {
            let mut say = |s: &str| {
                let _ = said_tx.send(s.to_string());
            };
            serve(&program, &[], &paths, None, "claude-9", &mut say).unwrap()
        });

        let (mut a, mut a_out) = connect(&sock, 0, "");
        let header = json::parse(&line(&mut a)).unwrap();
        assert_eq!(header.get("next").and_then(Value::as_u64), Some(2));
        let run = header
            .get("run")
            .and_then(Value::as_str)
            .unwrap()
            .to_string();
        // The holder's own first line: the model and effort claude was started with.
        let cfg = line(&mut a);
        assert!(
            cfg.starts_with("1\t") && cfg.contains(r#""type":"_cfg""#),
            "{cfg}"
        );

        // As the holder writes a line on: one JSON object, its keys in the writer's order.
        let canon = |l: &str| json::to_line(&json::parse(l).unwrap());
        let prompt =
            &canon(r#"{"type":"user","message":{"role":"user","content":"hi"},"isReplay":true}"#);
        let inp = |l: &str| canon(&format!(r#"{{"type":"_in","line":{l}}}"#));
        // One at a time: claude answers each in its own time, after the next could be logged.
        a_out.write_all(format!("{prompt}\n").as_bytes()).unwrap();
        a_out
            .write_all(b"not json, never reaches claude\n")
            .unwrap();
        assert_eq!(
            line(&mut a),
            format!("2\t{}", inp(prompt)),
            "what a view sent, first"
        );
        assert_eq!(
            line(&mut a),
            format!("3\t{prompt}"),
            "then claude's own line"
        );
        let done = &canon(r#"{"type":"result","subtype":"success","session_id":"s"}"#);
        a_out.write_all(format!("{done}\n").as_bytes()).unwrap();
        assert_eq!(
            line(&mut a),
            format!("4\t{}", inp(done)),
            "the line that was not JSON was dropped"
        );
        assert_eq!(line(&mut a), format!("5\t{done}"));

        // A second tap of the same run, from line 5, gets 5 and then what follows.
        let (mut b, _b_out) = connect(&sock, 5, &run);
        assert_eq!(
            json::parse(&line(&mut b))
                .unwrap()
                .get("next")
                .and_then(Value::as_u64),
            Some(6)
        );
        assert_eq!(line(&mut b), format!("5\t{done}"));
        // A tap naming another run is sent everything from the start.
        let (mut c, mut c_out) = connect(&sock, 5, "an-older-run");
        line(&mut c);
        line(&mut c);
        assert_eq!(line(&mut c), format!("2\t{}", inp(prompt)));

        // Two views answer one request: the first reaches claude, the second nothing.
        let yes = &canon(
            r#"{"type":"control_response","response":{"subtype":"success","request_id":"r1","response":{"behavior":"allow"}}}"#,
        );
        let no = r#"{"type":"control_response","response":{"subtype":"success","request_id":"r1","response":{"behavior":"deny"}}}"#;
        a_out.write_all(format!("{yes}\n").as_bytes()).unwrap();
        assert_eq!(line(&mut a), format!("6\t{}", inp(yes)));
        assert_eq!(line(&mut a), format!("7\t{yes}"));
        c_out.write_all(format!("{no}\n").as_bytes()).unwrap();

        a_out.write_all(b"{\"quit\":1}\n").unwrap();
        assert_eq!(holder.join().unwrap(), 3, "claude's own exit status");
        assert!(!sock.exists(), "the socket goes with the holder");
        let quit = &canon(r#"{"quit":1}"#);
        // First the holder's `_cfg` line, whose values are this machine's settings.
        let written = std::fs::read_to_string(&log).unwrap();
        let (cfg, rest) = written.split_once('\n').unwrap();
        assert!(cfg.contains(r#""type":"_cfg""#), "{cfg}");
        assert_eq!(
            rest,
            format!(
                "{}\n{prompt}\n{}\n{done}\n{}\n{yes}\n{}\n",
                inp(prompt),
                inp(done),
                inp(yes),
                inp(quit)
            ),
            "claude's lines raw, each view's line before it, and the late answer nowhere"
        );
        let said: Vec<String> = said_rx.try_iter().collect();
        assert_eq!(
            said,
            [
                "claude-9: open in the remote view; starting",
                "claude-9: open in the remote view; working",
                "claude-9: open in the remote view; idle",
                "claude-9: claude has ended",
            ],
            "one line per change, none repeated"
        );
        // The tap still connected is sent the rest, and then ends with the holder.
        let mut rest = String::new();
        while b.read_line(&mut rest).unwrap() > 0 {}
        assert!(rest.ends_with(&format!("\t{}\n", inp(quit))), "{rest}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_model_claude_accepts_from_a_view_is_saved_and_one_it_refuses_is_not() {
        let mut log = Log {
            file: File::create(scratch("models").join("l")).unwrap(),
            offsets: Vec::new(),
            end: 0,
            closed: false,
            answered: HashSet::new(),
            open: HashSet::new(),
            models: HashMap::new(),
        };
        let v = |l: &str| json::parse(l).unwrap();
        let msg = |l: &str| sdk::classify(&v(l));
        let ok = |id: &str| {
            msg(&format!(
                r#"{{"type":"control_response","response":{{"subtype":"success","request_id":"{id}"}}}}"#
            ))
        };
        // As the relay writes them (`sdk::set_model`).
        log.model_asked(&sdk::set_model("c1", "opus"));
        log.model_asked(&sdk::set_model("c2", "nonsense"));
        log.model_asked(&sdk::set_effort("c3", Some("high")));
        assert_eq!(
            log.model_answered(&ok("c0")),
            None,
            "calibration: not asked"
        );
        assert_eq!(log.model_answered(&ok("c3")), None, "not a model");
        assert_eq!(log.model_answered(&ok("c1")).as_deref(), Some("opus"));
        assert_eq!(log.model_answered(&ok("c1")), None, "answered once");
        let refused = msg(
            r#"{"type":"control_response","response":{"subtype":"error","request_id":"c2","error":"no such model"}}"#,
        );
        assert_eq!(log.model_answered(&refused), None);
        assert!(log.models.is_empty(), "nothing left waiting");
    }

    #[test]
    fn the_model_and_effort_claude_starts_with_are_decided_as_it_decides_them() {
        let none = |_: &str| None;
        let layers = |l: &[(bool, &str)]| -> Vec<(bool, Value)> {
            l.iter()
                .map(|(u, t)| (*u, json::parse(t).unwrap()))
                .collect()
        };
        let a = |v: &[&str]| -> Vec<String> { v.iter().map(|s| s.to_string()).collect() };
        // Nothing set: Claude Code's default model, and the model's own effort.
        assert_eq!(
            configured(&[], &none, &[]),
            ("default".into(), String::new())
        );
        // The user's settings choose the model but not the effort, which headless claude
        // takes only from the other layers.
        let user = layers(&[(true, r#"{"model":"haiku","effortLevel":"high"}"#)]);
        assert_eq!(
            configured(&[], &none, &user),
            ("haiku".into(), String::new())
        );
        // The highest layer that sets it wins.
        let l = layers(&[
            (true, r#"{"model":"haiku","effortLevel":"high"}"#),
            (false, r#"{"model":"sonnet","effortLevel":"low"}"#),
            (false, r#"{"other":1}"#),
        ]);
        assert_eq!(configured(&[], &none, &l), ("sonnet".into(), "low".into()));
        // The environment over every file; a flag over the environment for the model, and
        // the environment over a flag for the effort.
        let env = |k: &str| match k {
            "ANTHROPIC_MODEL" => Some("opus".to_string()),
            "CLAUDE_CODE_EFFORT_LEVEL" => Some("max".to_string()),
            _ => None,
        };
        assert_eq!(configured(&[], &env, &l), ("opus".into(), "max".into()));
        assert_eq!(
            configured(
                &a(&["--resume", "x", "--model", "fable", "--effort=high"]),
                &env,
                &l
            ),
            ("fable".into(), "max".into())
        );
        assert_eq!(
            configured(&a(&["--effort", "high"]), &none, &l).1,
            "high",
            "a flag over the files"
        );
        // An empty value is no value.
        let blank = |k: &str| (k == "ANTHROPIC_MODEL").then(|| " ".to_string());
        assert_eq!(configured(&[], &blank, &[]).0, "default");
    }

    #[test]
    fn a_default_model_is_saved_where_claude_code_saves_it_keeping_the_rest() {
        let d = scratch("default-model");
        let f = d.join("cfg").join("settings.json");
        let read = |p: &Path| json::parse(&std::fs::read_to_string(p).unwrap()).unwrap();
        // No file, nor its directory, yet.
        save_default_model(&f, "opus").unwrap();
        assert_eq!(read(&f), json::parse(r#"{"model":"opus"}"#).unwrap());
        std::fs::write(
            &f,
            r#"{"env":{"A":"1"},"model":"haiku","effortLevel":"high"}"#,
        )
        .unwrap();
        save_default_model(&f, "claude-sonnet-5-5[1m]").unwrap();
        assert_eq!(
            read(&f),
            json::parse(
                r#"{"effortLevel":"high","env":{"A":"1"},"model":"claude-sonnet-5-5[1m]"}"#
            )
            .unwrap()
        );
        // The same model again leaves the file as it is, byte for byte.
        std::fs::write(&f, r#"{"model":"opus", "z":1}"#).unwrap();
        save_default_model(&f, "opus").unwrap();
        assert_eq!(
            std::fs::read_to_string(&f).unwrap(),
            r#"{"model":"opus", "z":1}"#
        );
        // Default takes the key out, as Claude Code's picker does.
        save_default_model(&f, "default").unwrap();
        assert_eq!(read(&f), json::parse(r#"{"z":1}"#).unwrap());
        // A file it cannot read is an error, and is left alone.
        std::fs::write(&f, "{not json").unwrap();
        assert!(save_default_model(&f, "opus").is_err());
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "{not json");
        // A linked file is written through, and stays linked.
        let real = d.join("real.json");
        std::fs::write(&real, "{}").unwrap();
        let link = d.join("link.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        save_default_model(&link, "fable").unwrap();
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(read(&real), json::parse(r#"{"model":"fable"}"#).unwrap());
        let left: Vec<_> = std::fs::read_dir(&d).unwrap().collect();
        assert_eq!(left.len(), 3, "no temporary file left behind: {left:?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_amber_mark_goes_when_the_last_open_prompt_is_answered_or_withdrawn() {
        let mut log = Log {
            file: File::create(scratch("open").join("l")).unwrap(),
            offsets: Vec::new(),
            end: 0,
            closed: false,
            answered: HashSet::new(),
            open: HashSet::new(),
            models: HashMap::new(),
        };
        let msg = |l: &str| sdk::classify(&json::parse(l).unwrap());
        let ask = |id: &str, tool: &str| {
            msg(&format!(
                r#"{{"type":"control_request","request_id":"{id}","request":{{"subtype":"can_use_tool","tool_name":"{tool}","input":{{"questions":[]}}}}}}"#
            ))
        };
        // Calibration: a request nobody asked for settles nothing.
        assert!(!log.settled("r0"));
        assert!(!log.asked(&ask("q1", "AskUserQuestion")));
        assert!(!log.asked(&ask("p1", "Bash")));
        assert!(!log.settled("q1"), "another is still open");
        assert!(!log.settled("q1"), "answered twice counts once");
        assert!(log.settled("p1"), "the last one answered");
        // Withdrawn by claude is settled as well.
        log.asked(&ask("p2", "Bash"));
        assert!(log.asked(&msg(
            r#"{"type":"control_cancel_request","request_id":"p2"}"#
        )));
        // The turn's end leaves the clearing to Stop, and nothing open behind it.
        log.asked(&ask("p3", "Bash"));
        assert!(!log.asked(&msg(
            r#"{"type":"result","subtype":"success","is_error":false,"session_id":"s"}"#
        )));
        assert!(!log.settled("p3"));
    }

    #[test]
    fn claude_is_run_as_one_driven_over_ssh_so_it_has_the_artifact_tools() {
        let dir = scratch("entry");
        let program = stand_in(&dir);
        let paths = Paths {
            sock: dir.join("s.sock"),
            log: dir.join("s.log"),
        };
        let sock = paths.sock.clone();
        let holder = std::thread::spawn(move || {
            serve(&program, &[], &paths, None, "claude-9", &mut |_: &str| {}).unwrap()
        });
        let (mut a, mut a_out) = connect(&sock, 0, "");
        line(&mut a);
        line(&mut a); // the holder's `_cfg` line
        a_out.write_all(b"{\"entrypoint\":1}\n").unwrap();
        line(&mut a);
        assert_eq!(line(&mut a), "3\t{\"ep\":\"ssh-remote\"}");
        a_out.write_all(b"{\"quit\":1}\n").unwrap();
        holder.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn history_is_read_backwards_a_whole_turn_at_a_time() {
        let dir = scratch("older");
        let path = dir.join("t.jsonl");
        let prompt =
            |t: &str| format!(r#"{{"type":"user","message":{{"role":"user","content":"{t}"}}}}"#);
        let call = |id: &str| {
            format!(
                r#"{{"type":"assistant","message":{{"id":"m{id}","content":[{{"type":"tool_use","id":"{id}","name":"Bash","input":{{"command":"ls"}}}}]}}}}"#
            )
        };
        let result = |id: &str| {
            format!(
                r#"{{"type":"user","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"{id}","content":"ok"}}]}}}}"#
            )
        };
        let reply = |t: &str| {
            format!(
                r#"{{"type":"assistant","message":{{"id":"r{t}","content":[{{"type":"text","text":"{t}"}}]}}}}"#
            )
        };
        // Three turns, with lines that are not prompts or replies between, and a long one to
        // cross a block boundary.
        let long = "x".repeat(100_000);
        let lines = [
            prompt("one"),
            reply("first"),
            r#"{"type":"summary","summary":"s"}"#.to_string(),
            prompt("two"),
            call("t1"),
            result("t1"),
            reply(&long),
            prompt("three"),
            call("t2"),
            result("t2"),
            reply("third"),
        ];
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        let len = std::fs::metadata(&path).unwrap().len();
        let texts = |v: &[Value]| -> Vec<String> {
            v.iter()
                .map(|l| json::to_line(l).chars().take(40).collect())
                .collect()
        };
        // One line wanted: the last turn whole, from its prompt.
        let (got, at, more) = older(&path, len, 1).unwrap();
        assert_eq!(got.len(), 4, "{:?}", texts(&got));
        assert!(sdk::starts_turn(&got[0]));
        assert!(more);
        // Then the one before it, across the long line.
        let (got, at2, more) = older(&path, at, 1).unwrap();
        assert_eq!(got.len(), 4, "{:?}", texts(&got));
        assert!(json::to_line(&got[3]).contains(&long));
        assert!(more);
        // And the first, which is all there is.
        let (got, at3, more) = older(&path, at2, 1).unwrap();
        assert_eq!(
            got.len(),
            2,
            "the summary line is left out: {:?}",
            texts(&got)
        );
        assert_eq!((at3, more), (0, false));
        // Asked for more than a turn: turns until there are that many.
        let (got, _, _) = older(&path, len, 5).unwrap();
        assert_eq!(got.len(), 8);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_resumed_claude_reads_idle_once_it_writes_anything() {
        let read = |l: &str| sdk::classify(&json::parse(l).unwrap());
        let answer = read(r#"{"type":"control_response","response":{"subtype":"success"}}"#);
        assert_eq!(next_said("starting", Some(&answer)), Some("idle"));
        assert_eq!(
            next_said("starting", None),
            Some("idle"),
            "even a line not JSON"
        );
        assert_eq!(
            next_said("working", Some(&answer)),
            None,
            "only the first line ends starting"
        );
        let prompt =
            read(r#"{"type":"user","message":{"role":"user","content":"hi"},"isReplay":true}"#);
        assert_eq!(
            next_said("starting", Some(&prompt)),
            doing(&prompt),
            "calibration"
        );
        assert_eq!(doing(&prompt), Some("working"));
    }

    #[test]
    fn the_headless_flags_are_the_measured_ones() {
        // Each was required on 2.1.311: without the permission tool, prompts are denied and
        // AskUserQuestion is missing; without replay, a second view never sees a prompt.
        let joined = HEADLESS.join(" ");
        for need in [
            "-p",
            "--input-format stream-json",
            "--output-format stream-json",
            "--verbose",
            "--include-partial-messages",
            "--permission-prompt-tool stdio",
            "--replay-user-messages",
        ] {
            assert!(joined.contains(need), "{need}");
        }
    }
}
