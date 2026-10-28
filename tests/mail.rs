//! Agent mail, end to end over a stand-in ssh: three containers, one of them down, and the
//! real binary at both ends — `mail-loop --once` in the hub's place, `mail-agent` in each
//! container's, `mail-hook` as Claude Code runs it.
//!
//! The known-good case and the known-bad ones side by side, as infra asked: a message that
//! should move AND ones that should bounce — an unknown receiver, one over the size cap, one
//! that is not text — because a check never seen to fail proves nothing. A fourth sender's
//! file is for a receiver that is down: its queue waits in the volume, and the message lands
//! on the tick after the receiver is back.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, SystemTime};

const BIN: &str = env!("CARGO_BIN_EXE_claude-sessions");

struct Root(PathBuf);

impl Drop for Root {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

impl Root {
    fn new(tag: &str) -> Root {
        let root = std::env::temp_dir().join(format!("cs-mail-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("bin")).unwrap();
        // ssh, as the hub runs it: `-T -o BatchMode=yes -- <host> <command…>`. A host with a
        // `down` file is refused the way ssh refuses; any other runs the command with the
        // host's mail directory.
        let ssh = root.join("bin/ssh");
        fs::write(
            &ssh,
            format!(
                r#"#!/bin/sh
while [ $# -gt 0 ]; do
  case $1 in -t|-T) shift ;; -o) shift 2 ;; --) shift; break ;; *) break ;; esac
done
host=$1; shift
d="{root}/$host"
if [ -f "$d/down" ] || [ ! -d "$d" ]; then
  echo "ssh: connect to host $host port 2222: Connection refused" >&2
  exit 255
fi
CLAUDE_SESSIONS_MAIL="$d/.mail" exec "$@""#,
                root = root.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
        for host in ["infra", "or3", "dossier"] {
            fs::create_dir_all(root.join(host).join(".mail")).unwrap();
        }
        Root(root)
    }

    fn mail(&self, host: &str) -> PathBuf {
        self.0.join(host).join(".mail")
    }

    fn volume(&self) -> PathBuf {
        self.0.join("volume")
    }

    /// A file in `host`'s outbox for `to`, old enough to be taken.
    fn send(&self, host: &str, to: &str, name: &str, body: &[u8]) -> PathBuf {
        let dir = self.mail(host).join("out").join(to);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        fs::write(&path, body).unwrap();
        let old = SystemTime::now() - Duration::from_secs(10);
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(old)
            .unwrap();
        path
    }

    fn down(&self, host: &str, is: bool) {
        let flag = self.0.join(host).join("down");
        if is {
            fs::write(flag, "").unwrap();
        } else {
            let _ = fs::remove_file(flag);
        }
    }

    /// One pass of the loop, as the hub runs it; what it logged.
    fn tick(&self) -> String {
        let out = Command::new(BIN)
            .args(["mail-loop", "--once"])
            .env("CLAUDE_SESSIONS_HOSTS", "infra or3 dossier hub")
            .env("CLAUDE_SESSIONS_SSH", self.0.join("bin/ssh"))
            .env("CLAUDE_SESSIONS_REMOTE", BIN)
            .env("HUB_MAIL", self.volume())
            .env("HUB_MAIL_MAX_BYTES", "64")
            .env("HUB_MAIL_MAX_QUEUE", "3")
            .env("HUB_MAIL_TICK", "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "mail-loop failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    /// The loop as the hub runs it, a one-second tick: `half` long, then `then()`, then as
    /// long again; what it logged.
    fn run_for(&self, half: Duration, then: impl FnOnce()) -> String {
        let mut child = Command::new(BIN)
            .arg("mail-loop")
            .env("CLAUDE_SESSIONS_HOSTS", "infra or3 dossier")
            .env("CLAUDE_SESSIONS_SSH", self.0.join("bin/ssh"))
            .env("CLAUDE_SESSIONS_REMOTE", BIN)
            .env("HUB_MAIL", self.volume())
            .env("HUB_MAIL_TICK", "1")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        std::thread::sleep(half);
        then();
        std::thread::sleep(half);
        child.kill().unwrap();
        let out = child.wait_with_output().unwrap();
        String::from_utf8(out.stdout).unwrap()
    }

    /// `mail-hook` in `host`, as Claude Code runs it, with a payload on stdin.
    fn hook(&self, host: &str) -> Output {
        let mut child = Command::new(BIN)
            .arg("mail-hook")
            .env("CLAUDE_SESSIONS_MAIL", self.mail(host))
            .env("CLAUDE_SESSIONS_DIR", self.0.join("registry"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(br#"{"hook_event_name":"UserPromptSubmit","prompt":"hi"}"#)
            .unwrap();
        child.wait_with_output().unwrap()
    }
}

fn inbox(root: &Root, host: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Ok(froms) = fs::read_dir(root.mail(host).join("in")) else {
        return out;
    };
    for from in froms.filter_map(Result::ok) {
        for f in fs::read_dir(from.path()).unwrap().filter_map(Result::ok) {
            out.push((
                from.file_name().to_string_lossy().into_owned(),
                fs::read_to_string(f.path()).unwrap(),
            ));
        }
    }
    out.sort();
    out
}

fn outbox_files(root: &Root, host: &str) -> usize {
    let Ok(tos) = fs::read_dir(root.mail(host).join("out")) else {
        return 0;
    };
    tos.filter_map(Result::ok)
        .map(|to| fs::read_dir(to.path()).map(|d| d.count()).unwrap_or(0))
        .sum()
}

#[test]
fn a_message_moves_the_bad_ones_bounce_and_a_down_receivers_queue_waits() {
    let root = Root::new("loop");
    root.send(
        "infra",
        "or3",
        "ask.md",
        b"From: the owner\n\nPlease review PR 12.\n",
    );
    root.send("infra", "nowhere", "lost.md", b"hello?\n");
    root.send("infra", "or3", "big.md", &[b'x'; 100]);
    root.send("infra", "or3", "bin.md", &[0xff, 0xfe, 0x00]);
    root.send("or3", "dossier", "later.md", b"for when you are back\n");
    root.send("or3", "hub", "hey.md", b"hub, do something\n");
    root.down("dossier", true);

    let log = root.tick();
    // The known-good case moved, and only the sender's own copy went. (The other message in
    // or3's inbox is the hub's answer to its own file for `hub`, below.)
    let or3 = inbox(&root, "or3");
    assert_eq!(or3.len(), 2, "or3's inbox: {or3:?}\nlog:\n{log}");
    let (from, text) = &or3[1];
    assert_eq!(from, "infra");
    let mut lines = text.lines();
    assert_eq!(lines.next(), Some("From: infra"), "{text}");
    assert_eq!(lines.next(), Some("To: or3"));
    assert!(lines.next().unwrap().starts_with("Date: 20"));
    assert_eq!(lines.next(), Some(""));
    assert_eq!(
        lines.next(),
        Some("From: the owner"),
        "the sender's own header line is body text"
    );
    assert_eq!(lines.next(), Some(""));
    assert_eq!(lines.next(), Some("Please review PR 12."));
    assert_eq!(
        outbox_files(&root, "infra"),
        0,
        "every file was taken, kept or bounced"
    );
    // The volume is the record: the moved message is marked delivered, the bounces are
    // delivered too (infra was up), and the waiting one is not.
    let delivered = fs::read_dir(root.volume().join("or3/infra/delivered"))
        .unwrap()
        .count();
    assert_eq!(delivered, 1);
    assert!(
        fs::read_dir(root.volume().join("dossier/or3"))
            .unwrap()
            .filter_map(Result::ok)
            .any(|e| e.file_name().to_string_lossy().ends_with(".md")),
        "dossier's queue waits in the volume"
    );
    // The known-bad cases: three bounces from `hub` into infra's inbox, each saying why.
    let infra = inbox(&root, "infra");
    assert_eq!(infra.len(), 3, "infra's inbox: {infra:?}\nlog:\n{log}");
    for (from, text) in &infra {
        assert_eq!(from, "hub");
        assert!(text.starts_with("From: hub\nTo: infra\nDate: "), "{text}");
        assert!(
            text.contains("The containers that take mail are: infra, or3, dossier."),
            "{text}"
        );
    }
    let all: String = infra.iter().map(|(_, t)| t.as_str()).collect();
    for why in [
        "unknown receiver nowhere",
        "over the size limit of 64 bytes",
        "not UTF-8 text",
    ] {
        assert!(all.contains(why), "{why:?} not in {all}");
    }
    // or3's own bounce, for writing to the hub, landed in the same pass: or3 is up.
    assert_eq!(or3[0].0, "hub");
    assert!(
        or3[0]
            .1
            .contains("was not delivered: the hub takes no mail"),
        "{}",
        or3[0].1
    );
    assert_eq!(outbox_files(&root, "or3"), 0);
    let or3_bounce = fs::read_dir(root.volume().join("or3/hub/delivered"))
        .unwrap()
        .count();
    assert_eq!(or3_bounce, 1, "or3 was told the hub takes no mail");
    // The log: one line per event, never the body, and the down container said once.
    for want in [
        "infra -> or3: ",
        "infra -> nowhere: refused, unknown receiver nowhere; bounced",
        "infra -> or3: refused, over the size limit of 64 bytes; bounced",
        "infra -> or3: refused, not UTF-8 text; bounced",
        "or3 -> hub: refused, the hub takes no mail; bounced",
        "delivered to or3: infra/",
        "delivered to infra: hub/",
        "dossier is not answering: ssh: connect to host dossier port 2222: Connection refused; its mail waits",
        "a container called \"hub\" is skipped",
    ] {
        assert!(log.contains(want), "{want:?} not in log:\n{log}");
    }
    assert!(!log.contains("Please review"), "a body in the log:\n{log}");
    assert!(root.volume().join(".beat").exists());

    // The loop as the hub runs it, a one-second tick, with dossier down for the first ticks
    // and back for the last: a container down is said once, not every tick; back, it is
    // said once more, and the waiting message lands.
    let log2 = root.run_for(Duration::from_millis(1500), || root.down("dossier", false));
    assert_eq!(
        log2.matches("dossier is not answering").count(),
        1,
        "{log2}"
    );
    assert_eq!(
        log2.matches("dossier is answering again").count(),
        1,
        "{log2}"
    );
    assert_eq!(
        log2.matches("delivered to dossier: or3/").count(),
        1,
        "{log2}"
    );
    assert!(
        !log2.contains("delivered to or3"),
        "nothing else moved:\n{log2}"
    );
    let dossier = inbox(&root, "dossier");
    assert_eq!(dossier.len(), 1);
    assert!(dossier[0].1.ends_with("\n\nfor when you are back\n"));
    assert!(
        !root
            .volume()
            .join("dossier/or3")
            .join("delivered")
            .read_dir()
            .unwrap()
            .next()
            .is_none()
    );
}

#[test]
fn a_full_queue_bounces_and_empties_when_the_receiver_returns() {
    let root = Root::new("queue");
    root.down("dossier", true);
    for i in 0..5 {
        root.send("infra", "dossier", &format!("{i}.md"), b"one more\n");
    }
    let log = root.tick();
    let pending = fs::read_dir(root.volume().join("dossier/infra"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().ends_with(".md"))
        .count();
    assert_eq!(pending, 3, "the queue holds the limit:\n{log}");
    let bounces = inbox(&root, "infra");
    assert_eq!(bounces.len(), 2, "{bounces:?}");
    assert!(
        bounces[0]
            .1
            .contains("dossier's queue is full (3 undelivered)")
    );
    root.down("dossier", false);
    root.tick();
    assert_eq!(inbox(&root, "dossier").len(), 3);
    root.send("infra", "dossier", "6.md", b"room again\n");
    root.tick();
    assert_eq!(inbox(&root, "dossier").len(), 4);
}

#[test]
fn the_hook_says_one_line_per_new_message_once_and_nothing_otherwise() {
    let root = Root::new("hook");
    // Nothing: no directory at all, then an empty inbox. Silent, exit 0, both.
    let out = root.hook("or3");
    assert!(out.status.success());
    assert!(
        out.stdout.is_empty(),
        "{:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(out.stderr.is_empty());
    root.send("infra", "or3", "a.md", b"first\n");
    root.tick();
    std::thread::sleep(Duration::from_millis(20));
    root.send("infra", "or3", "b.md", b"second\n");
    root.send("dossier", "or3", "c.md", b"third\n");
    let log = root.tick();
    assert!(
        log.contains("delivered to or3: dossier/"),
        "a message from a container later in the list lands in the same pass:\n{log}"
    );
    let out = root.hook("or3");
    assert!(out.status.success());
    let said = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = said.lines().collect();
    assert_eq!(lines.len(), 3, "{said}");
    let inbox_dir = root.mail("or3").join("in");
    assert!(
        lines[0].starts_with("new message from infra, 20")
            && lines[0].contains("Z, at ")
            && lines[0].ends_with(".md"),
        "{said}"
    );
    assert!(lines[0].contains(&format!(", at {}/infra/", inbox_dir.display())));
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("new message from dossier, "))
    );
    for l in &lines {
        let path = l.rsplit(", at ").next().unwrap();
        assert!(Path::new(path).is_file(), "{l}");
    }
    let stamps: Vec<&str> = lines
        .iter()
        .map(|l| l.rsplit('/').next().unwrap())
        .collect();
    let mut sorted = stamps.clone();
    sorted.sort();
    assert_eq!(stamps, sorted, "oldest first: {said}");
    // Announced once: the cursor moved, and a second look says nothing.
    let cursor = fs::read_to_string(root.mail("or3").join(".cursor")).unwrap();
    assert!(
        cursor.lines().next().unwrap().parse::<u128>().is_ok(),
        "{cursor:?}"
    );
    let out = root.hook("or3");
    assert!(out.status.success());
    assert!(
        out.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    // The hook never speaks on stderr or fails, even with a directory it cannot read.
    let unreadable = root.mail("or3").join("in/infra");
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).unwrap();
    let out = root.hook("or3");
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(out.status.success());
    assert!(out.stderr.is_empty());
}
