//! The owner's `!` commands in the remote view, run as Claude Code 2.1.311 runs its own
//! (`docs/remote-view.md`): in the user's bash, or zsh, from the directory the last one ended
//! in, so a `cd` carries to the next, and `cd -` goes back; under the Bash tool's timeout;
//! stopped by Esc. Output goes to files, as the Bash tool's does, so a job left running with
//! `&` holds nothing open and is not killed when the command ends.
//!
//! The holder runs them (`holder::bang`) rather than claude's own `bash_command`, which runs
//! each in a fresh `/bin/sh` in the session's directory and says nothing of where it ended.

use std::fs::File;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The Bash tool's own default, which Claude Code's `!` runs under: two minutes, or
/// `BASH_DEFAULT_TIMEOUT_MS`.
const TIMEOUT_MS: u64 = 120_000;

/// The most of each stream kept, as the Bash tool's `BASH_MAX_OUTPUT_LENGTH` default.
pub const MAX_CHARS: usize = 30_000;

/// Where the owner's shell is, where it was before (`cd -`), and whether Esc asked the command
/// running to stop.
#[derive(Debug, Default)]
pub struct Shell {
    pub cwd: Option<PathBuf>,
    pub old: Option<PathBuf>,
    stop: bool,
}

/// What a command came to.
#[derive(Debug, Clone, PartialEq)]
pub struct Ran {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
    /// Stopped by Esc: Claude Code adds it without asking Claude to answer.
    pub interrupted: bool,
}

/// The timeout, as the Bash tool reads it.
pub fn timeout(env: &dyn Fn(&str) -> Option<String>) -> Duration {
    let ms = env("BASH_DEFAULT_TIMEOUT_MS")
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&ms| ms > 0)
        .unwrap_or(TIMEOUT_MS);
    Duration::from_millis(ms)
}

/// The shell Claude Code picks: `CLAUDE_CODE_SHELL` or `SHELL` when it is bash or zsh, else
/// bash or zsh where they are usually installed; `/bin/sh` when there is neither.
pub fn program(env: &dyn Fn(&str) -> Option<String>, exists: &dyn Fn(&Path) -> bool) -> PathBuf {
    let named = ["CLAUDE_CODE_SHELL", "SHELL"]
        .into_iter()
        .filter_map(env)
        .filter(|s| s.contains("bash") || s.contains("zsh"))
        .map(PathBuf::from);
    let usual = ["bash", "zsh"].into_iter().flat_map(|name| {
        ["/bin", "/usr/bin", "/usr/local/bin", "/opt/homebrew/bin"]
            .into_iter()
            .map(move |d| Path::new(d).join(name))
    });
    named
        .chain(usual)
        .find(|p| exists(p))
        .unwrap_or_else(|| PathBuf::from("/bin/sh"))
}

/// Ask the command running to stop; it is killed, with its group, within a moment.
pub fn stop(shell: &Mutex<Shell>) {
    shell.lock().unwrap_or_else(|e| e.into_inner()).stop = true;
}

/// Run `command` in `program` from where the shell is (`home` the first time, and when that
/// directory has gone), with `scratch` as the stem of the files its output and ending
/// directory pass through. Where it ended is where the next starts.
pub fn run(
    command: &str,
    shell: &Mutex<Shell>,
    program: &Path,
    home: &Path,
    scratch: &Path,
    wait: Duration,
) -> Ran {
    let (cwd, old) = {
        let mut s = shell.lock().unwrap_or_else(|e| e.into_inner());
        s.stop = false;
        let cwd = s.cwd.clone().filter(|d| d.is_dir()).unwrap_or(home.into());
        (cwd, s.old.clone())
    };
    let file = |ext: &str| scratch.with_extension(ext);
    let (out_path, err_path, cwd_path) = (file("out"), file("err"), file("cwd"));
    let _ = std::fs::remove_file(&cwd_path);
    let failed = |e: std::io::Error| Ran {
        stdout: String::new(),
        stderr: format!("Not run: {e}"),
        code: 127,
        interrupted: false,
    };
    let (Ok(out), Ok(err)) = (File::create(&out_path), File::create(&err_path)) else {
        return failed(std::io::Error::other("its output has nowhere to go"));
    };
    // Where it ends, however it ends: an `exit` in the command included.
    let script = format!("trap 'pwd -P >\"$CLAUDE_SESSIONS_CWD\" 2>/dev/null' EXIT\n{command}");
    let mut cmd = Command::new(program);
    cmd.arg("-c")
        .arg(script)
        .current_dir(&cwd)
        .env("CLAUDE_SESSIONS_CWD", &cwd_path)
        .env("PWD", &cwd)
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err)
        .process_group(0);
    match &old {
        Some(o) => cmd.env("OLDPWD", o),
        None => cmd.env_remove("OLDPWD"),
    };
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return failed(e),
    };
    let started = Instant::now();
    let mut interrupted = false;
    let mut timed_out = false;
    // Killed only from here, before the child is reaped, so its group is still its own.
    let status = loop {
        if let Ok(Some(st)) = child.try_wait() {
            break Some(st);
        }
        let asked = shell.lock().unwrap_or_else(|e| e.into_inner()).stop;
        if (asked || started.elapsed() >= wait) && !(interrupted || timed_out) {
            interrupted = asked;
            timed_out = !asked;
            let _ = crate::signal::send_group(child.id(), crate::signal::SIGKILL);
        }
        std::thread::sleep(Duration::from_millis(20));
        if started.elapsed() > wait + Duration::from_secs(5) && (interrupted || timed_out) {
            break child.wait().ok();
        }
    };
    let mut stdout = read_capped(&out_path);
    let mut stderr = read_capped(&err_path);
    let _ = std::fs::remove_file(&out_path);
    let _ = std::fs::remove_file(&err_path);
    if interrupted {
        push_line(&mut stderr, "[interrupted]");
    } else if timed_out {
        push_line(
            &mut stderr,
            &format!("Command timed out after {}", said(wait)),
        );
    }
    let ended = std::fs::read_to_string(&cwd_path)
        .ok()
        .map(|s| PathBuf::from(s.trim_end_matches('\n')))
        .filter(|d| d.is_dir());
    let _ = std::fs::remove_file(&cwd_path);
    {
        let mut s = shell.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(d) = ended.filter(|d| *d != cwd) {
            s.old = Some(cwd);
            s.cwd = Some(d);
        } else if s.cwd.is_none() {
            s.cwd = Some(cwd);
        }
    }
    use std::os::unix::process::ExitStatusExt;
    let code = status.map_or(130, |st| {
        st.code().unwrap_or_else(|| 128 + st.signal().unwrap_or(9))
    });
    stdout.truncate(stdout.trim_end().len());
    stderr.truncate(stderr.trim_end().len());
    Ran {
        stdout,
        stderr,
        code,
        interrupted,
    }
}

/// `2m`, `1m 30s`, `45s`, `0.3s`.
fn said(d: Duration) -> String {
    let s = d.as_secs();
    match (s / 60, s % 60) {
        (0, 0) => format!("{:.1}s", d.as_secs_f64()),
        (0, s) => format!("{s}s"),
        (m, 0) => format!("{m}m"),
        (m, s) => format!("{m}m {s}s"),
    }
}

fn push_line(s: &mut String, line: &str) {
    s.truncate(s.trim_end().len());
    if !s.is_empty() {
        s.push('\n');
    }
    s.push_str(line);
}

/// A stream's first `MAX_CHARS` characters, and how many more there were. Read no further
/// than that: a command can print without end, and the container's RAM is the hard limit.
fn read_capped(path: &Path) -> String {
    let Ok(f) = File::open(path) else {
        return String::new();
    };
    let total = f.metadata().map_or(0, |m| m.len());
    let mut buf = Vec::new();
    let _ = f.take((MAX_CHARS * 4) as u64).read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf);
    let kept: String = text.chars().take(MAX_CHARS).collect();
    if (kept.len() as u64) < total {
        let more = total - kept.len() as u64;
        format!("{kept}\n\n... [{more} more bytes not shown]")
    } else {
        kept
    }
}

/// Claude Code's escaping of what goes inside its `<bash-…>` tags.
fn escape(t: &str) -> String {
    t.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The command, as Claude Code keeps it in the conversation.
pub fn input_text(command: &str) -> String {
    format!("<bash-input>{}</bash-input>", escape(command))
}

/// What it printed, as Claude Code keeps it, with the exit code `bash_command` adds.
pub fn output_text(ran: &Ran) -> String {
    format!(
        "<bash-stdout>{}</bash-stdout><bash-stderr>{}</bash-stderr><bash-exit-code>{}</bash-exit-code>",
        escape(&ran.stdout),
        escape(&ran.stderr),
        ran.code
    )
}

/// Claude Code's caveat before a `!` command it adds without asking Claude to answer.
pub const CAVEAT: &str = "<local-command-caveat>The command below was run directly in Claude Code, not sent to you as a request, and its output goes straight to the user. It's recorded here as context for later messages.</local-command-caveat>";

/// Said to Claude when the owner's shell moves: unlike Claude Code's, where `!` and the Bash
/// tool share one shell, a headless claude's Bash tool keeps its own directory.
pub fn moved_text(cwd: &Path) -> String {
    format!(
        "<system-reminder>The user's ! commands now run in {}. That is their shell alone: the working directory of your own Bash tool has not changed.</system-reminder>",
        cwd.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cs-bang-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn sh() -> PathBuf {
        program(&|k| std::env::var(k).ok(), &|p| p.is_file())
    }

    #[test]
    fn a_cd_carries_to_the_next_command_and_cd_minus_goes_back() {
        let d = scratch("cd");
        std::fs::create_dir_all(d.join("work/sub")).unwrap();
        let home = d.join("work").canonicalize().unwrap();
        let shell = Mutex::new(Shell::default());
        let run = |c: &str| {
            run(
                c,
                &shell,
                &sh(),
                &home,
                &d.join("s"),
                Duration::from_secs(10),
            )
        };
        assert_eq!(run("pwd").stdout, home.display().to_string(), "calibration");
        assert_eq!(run("cd sub").code, 0);
        assert_eq!(run("pwd").stdout, home.join("sub").display().to_string());
        // An exit still says where it ended.
        assert_eq!(run("cd .. && exit 3").code, 3);
        assert_eq!(run("pwd").stdout, home.display().to_string());
        assert_eq!(
            run("cd - >/dev/null; pwd").stdout,
            home.join("sub").display().to_string()
        );
        // A directory that has gone: back where the session started.
        std::fs::remove_dir(home.join("sub")).unwrap();
        assert_eq!(run("pwd").stdout, home.display().to_string());
    }

    #[test]
    fn output_errors_codes_and_a_job_left_running() {
        let d = scratch("out");
        let shell = Mutex::new(Shell::default());
        let t0 = Instant::now();
        let ran = run(
            "echo out; echo err >&2; sleep 30 & exit 2",
            &shell,
            &sh(),
            &d,
            &d.join("s"),
            Duration::from_secs(10),
        );
        assert_eq!(
            (ran.stdout.as_str(), ran.stderr.as_str(), ran.code),
            ("out", "err", 2)
        );
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "the job holds nothing open"
        );
        assert!(!ran.interrupted);
        assert_eq!(
            output_text(&ran),
            "<bash-stdout>out</bash-stdout><bash-stderr>err</bash-stderr><bash-exit-code>2</bash-exit-code>"
        );
        assert_eq!(
            input_text("a > b && c"),
            "<bash-input>a &gt; b &amp;&amp; c</bash-input>"
        );
    }

    #[test]
    fn esc_and_the_timeout_kill_the_command_and_its_children() {
        let d = scratch("stop");
        let shell = Mutex::new(Shell::default());
        let t0 = Instant::now();
        let ran = std::thread::scope(|s| {
            let h = s.spawn(|| {
                run(
                    "echo started; sleep 30 | cat",
                    &shell,
                    &sh(),
                    &d,
                    &d.join("s"),
                    Duration::from_secs(60),
                )
            });
            std::thread::sleep(Duration::from_millis(500));
            stop(&shell);
            h.join().unwrap()
        });
        assert!(t0.elapsed() < Duration::from_secs(5));
        assert!(ran.interrupted);
        assert_eq!(
            (ran.stdout.as_str(), ran.stderr.as_str()),
            ("started", "[interrupted]")
        );
        let ran = run(
            "sleep 30",
            &shell,
            &sh(),
            &d,
            &d.join("s"),
            Duration::from_millis(300),
        );
        assert!(!ran.interrupted);
        assert!(ran.stderr.starts_with("Command timed out after"), "{ran:?}");
    }

    #[test]
    fn a_stream_is_cut_at_the_bash_tools_length() {
        let d = scratch("cap");
        let shell = Mutex::new(Shell::default());
        let ran = run(
            "head -c 40000 /dev/zero | tr '\\0' x",
            &shell,
            &sh(),
            &d,
            &d.join("s"),
            Duration::from_secs(10),
        );
        assert!(ran.stdout.starts_with(&"x".repeat(MAX_CHARS)));
        assert!(
            ran.stdout.ends_with("... [10000 more bytes not shown]"),
            "{}",
            &ran.stdout[MAX_CHARS..]
        );
    }

    #[test]
    fn the_shell_is_claude_codes_pick_and_the_timeout_its_own() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        let all = |_: &Path| true;
        assert_eq!(
            program(&env(&[("SHELL", "/usr/bin/fish")]), &all),
            PathBuf::from("/bin/bash")
        );
        assert_eq!(
            program(&env(&[("SHELL", "/bin/zsh")]), &all),
            PathBuf::from("/bin/zsh")
        );
        assert_eq!(program(&env(&[]), &|_| false), PathBuf::from("/bin/sh"));
        assert_eq!(timeout(&env(&[])), Duration::from_secs(120));
        assert_eq!(
            timeout(&env(&[("BASH_DEFAULT_TIMEOUT_MS", "5000")])),
            Duration::from_secs(5)
        );
    }
}
