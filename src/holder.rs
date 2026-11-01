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
//! **What the zmx session shows**: one plain line at start and at each change of what claude
//! is doing, never on a timer, so attaching from the old menu explains the slot rather than
//! showing a blank screen. Whatever is typed into the session is read and dropped: the mail
//! nudge types into idle detached sessions, and a remote slot always reads as detached.
//!
//! The holder takes no registry lock: the tap's writes to the slot's record do, with a timeout.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::FileExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::json::{self, Value};
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

/// The log as the holder keeps it: where each line starts, and whether claude has gone.
struct Log {
    /// `offsets[i]` is where line `i + 1` starts; `end` is where the next one will.
    offsets: Vec<u64>,
    end: u64,
    closed: bool,
}

struct Shared {
    log: Mutex<Log>,
    grew: Condvar,
    log_path: PathBuf,
    run: String,
    claude: Mutex<ChildStdin>,
}

/// Run claude headless under this holder until it exits, serving taps meanwhile. Returns
/// claude's exit status. `say` is given each line for the session's screen.
pub fn serve(
    program: &Path,
    args: &[String],
    paths: &Paths,
    slot: &str,
    say: &mut dyn FnMut(&str),
) -> io::Result<i32> {
    if let Some(dir) = paths.log.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut log_file = File::create(&paths.log)?;
    // A socket left by a holder that died is in the way; one that is live cannot be, since a
    // slot's name is free when it starts.
    let _ = std::fs::remove_file(&paths.sock);
    let listener = UnixListener::bind(&paths.sock)?;

    let mut child: Child = Command::new(program)
        .args(HEADLESS)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    let stdout = child.stdout.take().expect("piped");
    let stdin = child.stdin.take().expect("piped");
    let run = format!("{}-{}", std::process::id(), crate::clock::now());
    let shared = Arc::new(Shared {
        log: Mutex::new(Log {
            offsets: Vec::new(),
            end: 0,
            closed: false,
        }),
        grew: Condvar::new(),
        log_path: paths.log.clone(),
        run,
        claude: Mutex::new(stdin),
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
        if line.last() != Some(&b'\n') {
            line.push(b'\n');
        }
        log_file.write_all(&line)?;
        {
            let mut log = shared.log.lock().unwrap_or_else(|e| e.into_inner());
            let start = log.end;
            log.offsets.push(start);
            log.end += line.len() as u64;
        }
        shared.grew.notify_all();
        if let Some(doing) = doing(&line) {
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
fn doing(line: &[u8]) -> Option<&'static str> {
    let v = json::parse(std::str::from_utf8(line).ok()?.trim()).ok()?;
    match sdk::classify(&v) {
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
    let mut out = conn.try_clone()?;
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
        out.write_all(format!("{}\n", json::to_line(&header)).as_bytes())?;
    }

    {
        let shared = Arc::clone(shared);
        std::thread::spawn(move || {
            let mut line = String::new();
            loop {
                line.clear();
                match input.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let text = line.trim();
                // Only a whole JSON object reaches claude: anything else would end its input.
                if !matches!(json::parse(text), Ok(Value::Obj(_))) {
                    continue;
                }
                let mut claude = shared.claude.lock().unwrap_or_else(|e| e.into_inner());
                if claude.write_all(format!("{text}\n").as_bytes()).is_err() {
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
            let mut buf = vec![0u8; (end - start) as usize];
            log_file.read_exact_at(&mut buf, start)?;
            sent += 1;
            out.write_all(format!("{sent}\t").as_bytes())?;
            out.write_all(&buf)?;
        }
        if closed {
            // Both ways: the thread reading this tap's lines holds the socket too, and the tap
            // must see the end now, not when it next writes.
            let _ = out.shutdown(std::net::Shutdown::Both);
            return Ok(());
        }
    }
}

/// `claude-sessions tap <slot> [from] [run]`: what the hub runs over `ssh -T` to reach a
/// remote slot. Its stdin goes to the holder and the holder's lines come out on stdout, so ssh
/// carries them; it ends when either side does.
pub fn tap(slot: &str, from: u64, run: &str) -> io::Result<()> {
    let p = paths(slot);
    let conn = UnixStream::connect(&p.sock)?;
    let mut hello = Value::obj();
    hello.set("from", Value::num(from as f64));
    hello.set("run", Value::string(run));
    (&conn).write_all(format!("{}\n", json::to_line(&hello)).as_bytes())?;
    let mut to_holder = conn.try_clone()?;
    std::thread::spawn(move || {
        let _ = io::copy(&mut io::stdin().lock(), &mut to_holder);
        let _ = to_holder.shutdown(std::net::Shutdown::Write);
    });
    let mut from_holder = conn;
    let mut buf = [0u8; 16 * 1024];
    let mut stdout = io::stdout().lock();
    loop {
        let n = from_holder.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        stdout.write_all(&buf[..n])?;
        stdout.flush()?;
    }
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
               case \"$l\" in *quit*) exit 3;; esac\n\
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
            serve(&program, &[], &paths, "claude-9", &mut say).unwrap()
        });

        let (mut a, mut a_out) = connect(&sock, 0, "");
        let header = json::parse(&line(&mut a)).unwrap();
        assert_eq!(header.get("next").and_then(Value::as_u64), Some(1));
        let run = header
            .get("run")
            .and_then(Value::as_str)
            .unwrap()
            .to_string();

        let prompt = r#"{"type":"user","message":{"role":"user","content":"hi"},"isReplay":true}"#;
        a_out.write_all(format!("{prompt}\n").as_bytes()).unwrap();
        a_out
            .write_all(b"not json, never reaches claude\n")
            .unwrap();
        let done = r#"{"type":"result","subtype":"success","session_id":"s"}"#;
        a_out.write_all(format!("{done}\n").as_bytes()).unwrap();
        assert_eq!(line(&mut a), format!("1\t{prompt}"));
        assert_eq!(
            line(&mut a),
            format!("2\t{done}"),
            "the line that was not JSON was dropped"
        );

        // A second tap of the same run, from line 2, gets 2 and then what follows.
        let (mut b, _b_out) = connect(&sock, 2, &run);
        assert_eq!(
            json::parse(&line(&mut b))
                .unwrap()
                .get("next")
                .and_then(Value::as_u64),
            Some(3)
        );
        assert_eq!(line(&mut b), format!("2\t{done}"));
        // A tap naming another run is sent everything from the start.
        let (mut c, _c_out) = connect(&sock, 2, "an-older-run");
        line(&mut c);
        assert_eq!(line(&mut c), format!("1\t{prompt}"));

        a_out.write_all(b"{\"quit\":1}\n").unwrap();
        assert_eq!(holder.join().unwrap(), 3, "claude's own exit status");
        assert!(!sock.exists(), "the socket goes with the holder");
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            format!("{prompt}\n{done}\n"),
            "the log is claude's lines, raw"
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
        // The tap still connected ends with the holder.
        let mut rest = String::new();
        assert_eq!(b.read_line(&mut rest).unwrap(), 0);
        let _ = std::fs::remove_dir_all(&dir);
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
