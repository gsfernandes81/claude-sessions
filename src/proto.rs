//! What the hub and a container's agent say to each other: one JSON object a line, over the
//! stdin and stdout of `ssh <container> claude-sessions agent` (`agent.rs`, `hub.rs`).
//!
//! **The agent to the hub:**
//!
//! - `{"snapshot":{"header":…,"workspace":…,"rows":[…]}}` — everything one container's
//!   screen shows. Sent when the agent starts, whenever a row changes, when the minute turns
//!   (the ages), and before every `done`, so the hub has the rows an action left behind by the
//!   time it hears the action is over.
//! - `{"id":N,"run":{…}}` — the action `N` needs a child run at the owner's terminal: the
//!   hub hands its terminal over and runs it back in this container with `ssh -t … agent
//!   exec`, then answers `ran`.
//! - `{"id":N,"done":{…}}` — what came of action `N` (`ui::Outcome`).
//!
//! **The hub to the agent:**
//!
//! - `{"id":N,"op":"open","key":…}` and its siblings `new`, `shell`, `offload`, `close`,
//!   `archive`, `unarchive` and `offload_then_open` (`victim`, and `then` when there is one),
//!   each with `env`: the scrollback variables the owner's login brought
//!   (`launch::SCROLLBACK`). An agent too old to know `offload` answers that it does not, and
//!   does nothing.
//! - `{"id":N,"ran":{"code":C}}`, or `{"id":N,"ran":{"error":"…"}}` when the child could not
//!   be run at all.
//!
//! **Tolerant both ways**, like the hook's payloads: a field this version does not know is
//! ignored, and a message it cannot read is dropped and logged, never a reason to stop. The
//! hub and every container run the same binary — the hub installs it into the volume every
//! container mounts (`install`) — so a mismatch is a container not yet restarted onto it,
//! and a menu that shows less beats one that refuses.

use crate::json::Value;
use crate::ui::{Dialog, Header, Outcome, Row, RowKey};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::process::Command;

fn s(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

fn b(v: &Value, key: &str) -> bool {
    v.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn obj(pairs: Vec<(&str, Value)>) -> Value {
    let mut o = Value::obj();
    for (k, v) in pairs {
        o.set(k, v);
    }
    o
}

fn arr(items: impl IntoIterator<Item = Value>) -> Value {
    Value::Arr(items.into_iter().collect())
}

fn strs(items: &[String]) -> Value {
    arr(items.iter().map(Value::string))
}

fn str_list(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_arr)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

// ── rows ────────────────────────────────────────────────────────────────────

pub fn key_to(k: &RowKey) -> Value {
    match k {
        RowKey::Slot(slot) => obj(vec![("slot", Value::string(slot))]),
        RowKey::Socket(name) => obj(vec![("socket", Value::string(name))]),
        RowKey::Conversation { id, cwd } => obj(vec![
            ("conversation", Value::string(id)),
            ("cwd", Value::string(cwd)),
        ]),
        RowKey::ArchiveFold => obj(vec![("fold", Value::Bool(true))]),
        RowKey::Container => obj(vec![("container", Value::Bool(true))]),
    }
}

pub fn key_from(v: &Value) -> Option<RowKey> {
    if let Some(slot) = s(v, "slot") {
        return Some(RowKey::Slot(slot));
    }
    if let Some(name) = s(v, "socket") {
        return Some(RowKey::Socket(name));
    }
    if let Some(id) = s(v, "conversation") {
        return Some(RowKey::Conversation {
            id,
            cwd: s(v, "cwd")?,
        });
    }
    if b(v, "fold") {
        return Some(RowKey::ArchiveFold);
    }
    if b(v, "container") {
        return Some(RowKey::Container);
    }
    None
}

/// A row as the agent sends it. Its `host` is not sent: the hub knows which container it is
/// talking to, and a container cannot name itself into another's rows.
pub fn row_to(r: &Row) -> Value {
    obj(vec![
        ("key", key_to(&r.key)),
        ("wants_you", Value::Bool(r.wants_you)),
        ("busy", Value::Bool(r.busy)),
        ("unread", Value::Bool(r.unread)),
        ("attached", Value::Bool(r.attached)),
        ("offloaded", Value::Bool(r.offloaded)),
        ("closed", Value::Bool(r.closed)),
        ("archived", Value::Bool(r.archived)),
        ("title", Value::string(&r.title)),
        ("age", Value::string(&r.age)),
        ("at", Value::num(r.at as f64)),
    ])
}

pub fn row_from(v: &Value, host: &str) -> Option<Row> {
    Some(Row {
        host: host.to_string(),
        key: key_from(v.get("key")?)?,
        wants_you: b(v, "wants_you"),
        busy: b(v, "busy"),
        unread: b(v, "unread"),
        attached: b(v, "attached"),
        offloaded: b(v, "offloaded"),
        closed: b(v, "closed"),
        archived: b(v, "archived"),
        title: s(v, "title").unwrap_or_default(),
        age: s(v, "age").unwrap_or_default(),
        at: v.get("at").and_then(Value::as_u64).unwrap_or(0),
        nested: false,
    })
}

pub fn header_to(h: &Header) -> Value {
    let mut o = obj(vec![("host", Value::string(&h.host))]);
    if let Some((used, limit)) = h.memory {
        o.set(
            "memory",
            arr([Value::num(used as f64), Value::num(limit as f64)]),
        );
    }
    o
}

pub fn header_from(v: &Value) -> Header {
    let memory = v
        .get("memory")
        .and_then(Value::as_arr)
        .and_then(|m| match m {
            [used, limit] => Some((used.as_u64()?, limit.as_u64()?)),
            _ => None,
        });
    Header {
        host: s(v, "host").unwrap_or_default(),
        memory,
    }
}

/// One container's screen, as the agent sends it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub header: Header,
    pub workspace: String,
    pub rows: Vec<Row>,
}

pub fn snapshot_to(header: &Header, workspace: &str, rows: &[Row]) -> Value {
    obj(vec![(
        "snapshot",
        obj(vec![
            ("header", header_to(header)),
            ("workspace", Value::string(workspace)),
            ("rows", arr(rows.iter().map(row_to))),
        ]),
    )])
}

pub fn snapshot_from(v: &Value, host: &str) -> Option<Snapshot> {
    let snap = v.get("snapshot")?;
    Some(Snapshot {
        header: header_from(snap.get("header")?),
        workspace: s(snap, "workspace").unwrap_or_else(|| "/workspace".into()),
        rows: snap
            .get("rows")
            .and_then(Value::as_arr)
            .map(|rows| rows.iter().filter_map(|r| row_from(r, host)).collect())
            .unwrap_or_default(),
    })
}

// ── outcomes ────────────────────────────────────────────────────────────────

pub fn outcome_to(o: &Outcome) -> Value {
    match o {
        Outcome::Back(status) => {
            let mut v = obj(vec![("back", Value::Bool(true))]);
            if let Some(st) = status {
                v.set("status", Value::string(st));
            }
            v
        }
        Outcome::Refused(why) => obj(vec![("refused", Value::string(why))]),
        Outcome::NoRoom(d) => obj(vec![("no_room", dialog_to(d))]),
        Outcome::ResumeFailed(d) => obj(vec![("resume_failed", dialog_to(d))]),
    }
}

pub fn outcome_from(v: &Value) -> Option<Outcome> {
    if let Some(why) = s(v, "refused") {
        return Some(Outcome::Refused(why));
    }
    if b(v, "back") {
        return Some(Outcome::Back(s(v, "status")));
    }
    if let Some(d) = v.get("no_room") {
        return Some(Outcome::NoRoom(dialog_from(d)?));
    }
    if let Some(d) = v.get("resume_failed") {
        return Some(Outcome::ResumeFailed(dialog_from(d)?));
    }
    None
}

fn dialog_to(d: &Dialog) -> Value {
    match d {
        Dialog::NoRoom {
            used,
            limit,
            want,
            offer,
        } => {
            let mut v = obj(vec![
                ("used", Value::num(*used as f64)),
                ("limit", Value::num(*limit as f64)),
                ("want", Value::num(*want as f64)),
            ]);
            if let Some((key, age, title)) = offer {
                v.set(
                    "offer",
                    obj(vec![
                        ("key", key_to(key)),
                        ("age", Value::string(age)),
                        ("title", Value::string(title)),
                    ]),
                );
            }
            v
        }
        Dialog::ResumeFailed {
            title,
            session,
            killed_for_memory,
            output,
            closed,
        } => obj(vec![
            ("title", Value::string(title)),
            ("session", Value::string(session)),
            ("killed_for_memory", Value::Bool(*killed_for_memory)),
            ("output", strs(output)),
            ("closed", Value::Bool(*closed)),
        ]),
    }
}

fn dialog_from(v: &Value) -> Option<Dialog> {
    if let Some(used) = v.get("used").and_then(Value::as_u64) {
        let offer = v.get("offer").and_then(|o| {
            Some((
                key_from(o.get("key")?)?,
                s(o, "age").unwrap_or_default(),
                s(o, "title").unwrap_or_default(),
            ))
        });
        return Some(Dialog::NoRoom {
            used,
            limit: v.get("limit").and_then(Value::as_u64).unwrap_or(0),
            want: v.get("want").and_then(Value::as_u64).unwrap_or(0),
            offer,
        });
    }
    Some(Dialog::ResumeFailed {
        title: s(v, "title")?,
        session: s(v, "session").unwrap_or_default(),
        killed_for_memory: b(v, "killed_for_memory"),
        output: str_list(v.get("output")),
        closed: b(v, "closed"),
    })
}

// ── a child to run at the hub's terminal ────────────────────────────────────

/// A command the agent wants run at the owner's terminal, in its own container: the program,
/// its arguments, where it starts, and its environment changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Child {
    pub argv: Vec<String>,
    pub cwd: Option<String>,
    /// Set these.
    pub env: BTreeMap<String, String>,
    /// And remove these (`zmx::command` strips `ZMX_SESSION`, so a slot started from inside
    /// one is not nested in it).
    pub unset: Vec<String>,
}

fn lossy(s: &OsStr) -> String {
    s.to_string_lossy().into_owned()
}

impl Child {
    pub fn of(cmd: &Command) -> Child {
        let mut argv = vec![lossy(cmd.get_program())];
        argv.extend(cmd.get_args().map(lossy));
        let mut env = BTreeMap::new();
        let mut unset = Vec::new();
        for (k, v) in cmd.get_envs() {
            match v {
                Some(v) => {
                    env.insert(lossy(k), lossy(v));
                }
                None => unset.push(lossy(k)),
            }
        }
        Child {
            argv,
            cwd: cmd.get_current_dir().map(|d| d.display().to_string()),
            env,
            unset,
        }
    }

    /// The command again, ready to run — or `None` with nothing to run.
    pub fn command(&self) -> Option<Command> {
        let (program, args) = self.argv.split_first()?;
        let mut cmd = Command::new(program);
        cmd.args(args);
        if let Some(cwd) = &self.cwd {
            cmd.current_dir(cwd);
        }
        for k in &self.unset {
            cmd.env_remove(k);
        }
        cmd.envs(&self.env);
        Some(cmd)
    }

    pub fn to_value(&self) -> Value {
        let mut env = Value::obj();
        for (k, v) in &self.env {
            env.set(k, Value::string(v));
        }
        let mut o = obj(vec![
            ("argv", strs(&self.argv)),
            ("env", env),
            ("unset", strs(&self.unset)),
        ]);
        if let Some(cwd) = &self.cwd {
            o.set("cwd", Value::string(cwd));
        }
        o
    }

    pub fn from_value(v: &Value) -> Option<Child> {
        let argv = str_list(v.get("argv"));
        if argv.is_empty() {
            return None;
        }
        let env = match v.get("env") {
            Some(Value::Obj(m)) => m
                .iter()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                .collect(),
            _ => BTreeMap::new(),
        };
        Some(Child {
            argv,
            cwd: s(v, "cwd"),
            env,
            unset: str_list(v.get("unset")),
        })
    }

    /// As one word of hex digits: what `ssh -t <container> claude-sessions agent exec` is
    /// given. **Hex because the container's login shell parses the command line** — fish in
    /// one container, bash in another — and a word of `[0-9a-f]` means the same in both, with
    /// nothing to quote and nothing to get wrong.
    pub fn to_hex(&self) -> String {
        let mut out = String::new();
        for byte in crate::json::to_line(&self.to_value()).bytes() {
            out.push_str(&format!("{byte:02x}"));
        }
        out
    }

    pub fn from_hex(hex: &str) -> Option<Child> {
        if hex.len() % 2 != 0 {
            return None;
        }
        let bytes: Option<Vec<u8>> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
            .collect();
        let text = String::from_utf8(bytes?).ok()?;
        Child::from_value(&crate::json::parse(&text).ok()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json;

    fn row(key: RowKey) -> Row {
        Row {
            host: "infra-dev".into(),
            key,
            wants_you: true,
            busy: false,
            unread: true,
            attached: false,
            offloaded: true,
            closed: false,
            archived: true,
            title: "retire \"the\" old\ttunnel\u{1b}[2J".into(),
            age: "14m".into(),
            at: 1_759_000_000_123,
            nested: false,
        }
    }

    /// Through text, as it goes over the wire.
    fn wire(v: &Value) -> Value {
        let line = json::to_line(v);
        assert!(!line.contains('\n'), "one message, one line: {line}");
        json::parse(&line).unwrap()
    }

    #[test]
    fn every_kind_of_row_crosses_the_wire_whole_and_takes_the_hubs_host() {
        let keys = [
            RowKey::Slot("claude-1".into()),
            RowKey::Socket("work".into()),
            RowKey::Conversation {
                id: "0f9c4a1e".into(),
                cwd: "/workspace/sub dir".into(),
            },
            RowKey::ArchiveFold,
            RowKey::Container,
        ];
        for key in keys {
            let r = row(key);
            let back = row_from(&wire(&row_to(&r)), "infra-dev").unwrap();
            assert_eq!(back, r);
            // The host is the hub's to say, whatever the agent sends.
            assert_eq!(row_from(&row_to(&r), "or3-dev").unwrap().host, "or3-dev");
        }
    }

    #[test]
    fn a_snapshot_carries_the_header_the_workspace_and_the_rows() {
        let header = Header {
            host: "infra-dev".into(),
            memory: Some((812 << 20, 1 << 30)),
        };
        let rows = vec![row(RowKey::Slot("claude-1".into()))];
        let v = wire(&snapshot_to(&header, "/workspace", &rows));
        let snap = snapshot_from(&v, "infra-dev").unwrap();
        assert_eq!(snap.header, header);
        assert_eq!(snap.workspace, "/workspace");
        assert_eq!(snap.rows, rows);
        let bare = Header {
            host: "x".into(),
            memory: None,
        };
        assert_eq!(
            snapshot_from(&wire(&snapshot_to(&bare, "/w", &[])), "x")
                .unwrap()
                .header,
            bare
        );
        assert_eq!(snapshot_from(&Value::obj(), "x"), None, "not a snapshot");
    }

    #[test]
    fn every_outcome_and_dialog_crosses_the_wire_whole() {
        let outcomes = [
            Outcome::Back(None),
            Outcome::Back(Some("detached · it is still running".into())),
            Outcome::Refused("\"a\" is busy".into()),
            Outcome::NoRoom(Dialog::NoRoom {
                used: 892 << 20,
                limit: 1 << 30,
                want: 250 << 20,
                offer: Some((RowKey::Slot("claude-2".into()), "14m".into(), "t".into())),
            }),
            Outcome::NoRoom(Dialog::NoRoom {
                used: 0,
                limit: 1,
                want: 2,
                offer: None,
            }),
            Outcome::ResumeFailed(Dialog::ResumeFailed {
                title: "mount guards on one".into(),
                session: "0f9c4a1e".into(),
                killed_for_memory: true,
                output: vec!["No conversation found".into(), "".into()],
                closed: true,
            }),
        ];
        for o in outcomes {
            assert_eq!(outcome_from(&wire(&outcome_to(&o))), Some(o));
        }
        assert_eq!(outcome_from(&Value::obj()), None, "nothing to read");
    }

    #[test]
    fn a_child_crosses_as_hex_and_comes_back_as_the_same_command() {
        let mut cmd = Command::new("zmx");
        cmd.args([
            "attach",
            "claude-1",
            "env",
            "A=1",
            "sh",
            "-c",
            r#"e=$1; exec "$@""#,
        ])
        .current_dir("/workspace")
        .env("CLAUDE_CODE_DISABLE_MOUSE", "")
        .env_remove("ZMX_SESSION");
        let child = Child::of(&cmd);
        let hex = child.to_hex();
        assert!(hex.bytes().all(|c| c.is_ascii_hexdigit()), "{hex}");
        let back = Child::from_hex(&hex).unwrap();
        assert_eq!(back, child);
        let again = Child::of(&back.command().unwrap());
        assert_eq!(again, child, "and runs as what was asked");
        assert_eq!(
            again
                .env
                .get("CLAUDE_CODE_DISABLE_MOUSE")
                .map(String::as_str),
            Some(""),
            "an empty value is a value, and it means something here"
        );
        assert_eq!(again.unset, ["ZMX_SESSION"]);
        for bad in ["", "0", "zz", "7b7d"] {
            assert_eq!(Child::from_hex(bad), None, "{bad:?}");
        }
    }
}
