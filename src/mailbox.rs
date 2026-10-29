//! The mail server's store and its routing: who a send reaches, what each recipient's copy
//! says, and the state each one is in. `docs/design.md` § *Agent mail* is the specification.
//!
//! **Pure where it decides.** Routing reads a [`Directory`] — the rows the containers' agents
//! last sent — and the clock it is given, so every outcome is tested without a socket; the
//! server (`mail.rs`) only feeds it. **Durable before it answers**: a message is on disk, file
//! and directory fsynced, before the sender hears it was sent.
//!
//! **Layout**, under `HUB_MAIL`: `v2/msg/<id>.eml`, the message as a reader sees it, written
//! once; `v2/state/<id>.json`, the message's fields but its body and each recipient's state,
//! rewritten through a rename whenever one changes. The store is read whole at start.

use crate::fmt::{stamp, utc};
use crate::json::{self, Value};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Names no container may have: `hub` is the server's own, `owner` is kept for the owner's
/// mail when it comes (deferred, 2026-10-29), so that its `From` is one only the hub writes.
pub const RESERVED: [&str; 2] = ["hub", "owner"];

/// How long the rate limit looks back.
pub const RATE_WINDOW_MS: u64 = 30 * 60 * 1000;

/// A repo's name or a slot's: what a directory entry and a header can both carry safely.
pub fn is_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && !s.starts_with('.')
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
}

/// What an address names.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Target {
    /// `repo/slot`.
    One(String, String),
    /// `repo/*`, or bare `repo`.
    Repo(String),
    /// `*`.
    All,
}

impl Target {
    pub fn parse(s: &str) -> Result<Target, String> {
        let s = s.trim();
        if s == "*" {
            return Ok(Target::All);
        }
        let (repo, slot) = s.split_once('/').unwrap_or((s, "*"));
        if !is_name(repo) {
            return Err(format!(
                "{s:?} is not an address: repo/slot, repo/*, repo or *"
            ));
        }
        if slot == "*" {
            return Ok(Target::Repo(repo.to_string()));
        }
        if !is_name(slot) {
            return Err(format!(
                "{s:?} is not an address: repo/slot, repo/*, repo or *"
            ));
        }
        Ok(Target::One(repo.to_string(), slot.to_string()))
    }

    /// As a header writes it.
    pub fn show(&self) -> String {
        match self {
            Target::One(r, s) => format!("{r}/{s}"),
            Target::Repo(r) => format!("{r}/*"),
            Target::All => "*".to_string(),
        }
    }
}

/// One slot, as its container's agent last told it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SlotView {
    pub slot: String,
    /// A claude is running in it.
    pub live: bool,
    pub offloaded: bool,
    pub busy: bool,
    /// A dialog is waiting on an answer: a permission prompt, an elicitation.
    pub needs_you: bool,
    pub attached: bool,
    pub title: String,
    /// Last active, in milliseconds.
    pub at: u64,
}

/// One container, as the server last heard from it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RepoView {
    /// Its agent is answering. A container that is not has no rows worth trusting.
    pub answering: bool,
    pub slots: Vec<SlotView>,
}

impl RepoView {
    pub fn live(&self) -> impl Iterator<Item = &SlotView> {
        self.slots.iter().filter(|s| s.live)
    }

    fn slot(&self, name: &str) -> Option<&SlotView> {
        self.slots.iter().find(|s| s.slot == name)
    }
}

/// Every container the server knows, by name.
pub type Directory = BTreeMap<String, RepoView>;

/// Whether `repo/slot` is a session that may speak for itself: in a container that answers,
/// and live there.
pub fn is_live(dir: &Directory, repo: &str, slot: &str) -> bool {
    dir.get(repo)
        .is_some_and(|r| r.answering && r.slot(slot).is_some_and(|s| s.live))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum State {
    Queued,
    Delivered,
    /// The hook has named it.
    Announced,
    /// `mail read` has printed it.
    Read,
}

impl State {
    pub fn name(self) -> &'static str {
        match self {
            State::Queued => "queued",
            State::Delivered => "delivered",
            State::Announced => "announced",
            State::Read => "read",
        }
    }

    pub fn from_name(s: &str) -> Option<State> {
        Some(match s {
            "queued" => State::Queued,
            "delivered" => State::Delivered,
            "announced" => State::Announced,
            "read" => State::Read,
            _ => return None,
        })
    }
}

/// One recipient's copy.
#[derive(Debug, Clone, PartialEq)]
pub struct Rcpt {
    /// `repo/slot` once delivered; the bare `repo` while queued for it.
    pub addr: String,
    pub state: State,
    /// Its place in the order mail reaches sessions, for `since`: 0 while queued.
    pub seq: u64,
    /// When it was queued, if it was.
    pub queued_since: Option<u64>,
    /// When it last changed state.
    pub at: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Msg {
    pub id: String,
    pub from: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub subject: String,
    pub date: u64,
    pub in_reply_to: Option<String>,
    pub reply_wanted: bool,
    pub body: String,
    pub rcpts: Vec<Rcpt>,
}

impl Msg {
    /// The text a reader is given: the hub's headers, a blank line, the body. `for_addr`'s
    /// copy says when it was queued, if it was.
    pub fn text(&self, for_addr: Option<&str>) -> String {
        let mut t = format!("From: {}\nTo: {}\n", self.from, self.to.join(", "));
        if !self.cc.is_empty() {
            t.push_str(&format!("Cc: {}\n", self.cc.join(", ")));
        }
        t.push_str(&format!(
            "Subject: {}\nDate: {}\nMessage-Id: <{}@hub>\n",
            self.subject,
            utc(self.date),
            self.id
        ));
        if let Some(r) = &self.in_reply_to {
            t.push_str(&format!("In-Reply-To: <{r}@hub>\n"));
        }
        t.push_str(&format!(
            "Reply-Wanted: {}\n",
            if self.reply_wanted { "yes" } else { "no" }
        ));
        let queued = for_addr
            .and_then(|a| self.rcpts.iter().find(|r| r.addr == a))
            .and_then(|r| r.queued_since);
        if let Some(q) = queued {
            t.push_str(&format!("Queued-Since: {}\n", utc(q)));
        }
        t.push('\n');
        t.push_str(&self.body);
        t
    }

    fn rcpt(&self, addr: &str) -> Option<&Rcpt> {
        self.rcpts.iter().find(|r| r.addr == addr)
    }

    /// What `who` (a session, `repo/slot`) may see of this message, as JSON: a recipient its
    /// own copy, the sender every recipient's state too. `None` for anybody else.
    pub fn view(&self, who: &str, with_body: bool) -> Option<Value> {
        let mine = self.rcpt(who);
        let sent = self.from == who;
        if mine.is_none() && !sent {
            return None;
        }
        let mut v = Value::obj();
        v.set("id", Value::string(&self.id));
        v.set("from", Value::string(&self.from));
        v.set("to", strs(&self.to));
        v.set("cc", strs(&self.cc));
        v.set("subject", Value::string(&self.subject));
        v.set("date", Value::string(utc(self.date)));
        if let Some(r) = &self.in_reply_to {
            v.set("in_reply_to", Value::string(r));
        }
        v.set("reply_wanted", Value::Bool(self.reply_wanted));
        if let Some(r) = mine {
            v.set("seq", Value::num(r.seq as f64));
            v.set("state", Value::string(r.state.name()));
            if let Some(q) = r.queued_since {
                v.set("queued_since", Value::string(utc(q)));
            }
        }
        if sent {
            v.set(
                "recipients",
                Value::Arr(
                    self.rcpts
                        .iter()
                        .map(|r| {
                            let mut o = Value::obj();
                            o.set("addr", Value::string(&r.addr));
                            o.set("state", Value::string(r.state.name()));
                            o.set("at", Value::string(utc(r.at)));
                            o
                        })
                        .collect(),
                ),
            );
        }
        if with_body {
            v.set("body", Value::string(&self.body));
            v.set(
                "text",
                Value::string(self.text(mine.map(|r| r.addr.as_str()))),
            );
        }
        Some(v)
    }

    fn meta(&self) -> Value {
        let mut v = Value::obj();
        v.set("id", Value::string(&self.id));
        v.set("from", Value::string(&self.from));
        v.set("to", strs(&self.to));
        v.set("cc", strs(&self.cc));
        v.set("subject", Value::string(&self.subject));
        v.set("date", Value::num(self.date as f64));
        if let Some(r) = &self.in_reply_to {
            v.set("in_reply_to", Value::string(r));
        }
        v.set("reply_wanted", Value::Bool(self.reply_wanted));
        v.set(
            "rcpts",
            Value::Arr(
                self.rcpts
                    .iter()
                    .map(|r| {
                        let mut o = Value::obj();
                        o.set("addr", Value::string(&r.addr));
                        o.set("state", Value::string(r.state.name()));
                        o.set("seq", Value::num(r.seq as f64));
                        if let Some(q) = r.queued_since {
                            o.set("queued_since", Value::num(q as f64));
                        }
                        o.set("at", Value::num(r.at as f64));
                        o
                    })
                    .collect(),
            ),
        );
        v
    }

    fn from_meta(v: &Value, body: String) -> Option<Msg> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        let list = |k: &str| -> Vec<String> {
            v.get(k)
                .and_then(Value::as_arr)
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        };
        let rcpts = v
            .get("rcpts")?
            .as_arr()?
            .iter()
            .filter_map(|r| {
                Some(Rcpt {
                    addr: r.get("addr")?.as_str()?.to_string(),
                    state: State::from_name(r.get("state")?.as_str()?)?,
                    seq: r.get("seq").and_then(Value::as_u64).unwrap_or(0),
                    queued_since: r.get("queued_since").and_then(Value::as_u64),
                    at: r.get("at").and_then(Value::as_u64).unwrap_or(0),
                })
            })
            .collect();
        Some(Msg {
            id: s("id")?,
            from: s("from")?,
            to: list("to"),
            cc: list("cc"),
            subject: s("subject").unwrap_or_default(),
            date: v.get("date").and_then(Value::as_u64)?,
            in_reply_to: s("in_reply_to"),
            reply_wanted: v
                .get("reply_wanted")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            body,
            rcpts,
        })
    }
}

fn strs(items: &[String]) -> Value {
    Value::Arr(items.iter().map(Value::string).collect())
}

/// What a send asks for, as the client sent it. `from` is never part of it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Send {
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub subject: String,
    pub body: String,
    pub in_reply_to: Option<String>,
    pub reply_wanted: bool,
}

/// What became of one recipient.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Delivered(String),
    Queued(String, String),
    Refused(String, String),
}

impl Outcome {
    pub fn line(&self) -> String {
        match self {
            Outcome::Delivered(a) => format!("delivered {a}"),
            Outcome::Queued(a, why) => format!("queued {a} ({why})"),
            Outcome::Refused(a, why) => format!("refused {a} ({why})"),
        }
    }

    pub fn to_json(&self) -> Value {
        let (outcome, addr, why) = match self {
            Outcome::Delivered(a) => ("delivered", a, None),
            Outcome::Queued(a, w) => ("queued", a, Some(w)),
            Outcome::Refused(a, w) => ("refused", a, Some(w)),
        };
        let mut v = Value::obj();
        v.set("addr", Value::string(addr));
        v.set("outcome", Value::string(outcome));
        if let Some(w) = why {
            v.set("why", Value::string(w));
        }
        v
    }
}

/// A send refused as a whole: the status to answer with, and why.
#[derive(Debug, Clone, PartialEq)]
pub struct Refusal {
    pub status: u16,
    pub why: String,
    /// Each recipient's outcome, when it was the recipients that left nothing to send to.
    pub outcomes: Vec<Outcome>,
}

fn refuse(status: u16, why: impl Into<String>) -> Refusal {
    Refusal {
        status,
        why: why.into(),
        outcomes: Vec::new(),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Sent {
    pub id: String,
    pub outcomes: Vec<Outcome>,
    /// The sessions that now have it, delivered: those the server may nudge.
    pub delivered: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limits {
    pub max_bytes: usize,
    pub max_queue: usize,
    pub retain_ms: u64,
    pub rate: usize,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            max_bytes: 16_384,
            max_queue: 200,
            retain_ms: 90 * 86_400_000,
            rate: 60,
        }
    }
}

/// Which of a session's mail a fetch wants.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Fetch {
    pub since: u64,
    pub unannounced: bool,
    pub unread: bool,
}

pub struct Store {
    /// `None` keeps it in memory only: the tests of routing.
    dir: Option<PathBuf>,
    msgs: BTreeMap<String, Msg>,
    next_seq: u64,
    sent_at: HashMap<String, VecDeque<u64>>,
    pub limits: Limits,
}

impl Store {
    pub fn memory(limits: Limits) -> Store {
        Store {
            dir: None,
            msgs: BTreeMap::new(),
            next_seq: 1,
            sent_at: HashMap::new(),
            limits,
        }
    }

    /// The store under `dir`, read whole. A message whose files cannot be read is skipped
    /// and named in the second half, for the log.
    pub fn open(dir: &Path, limits: Limits) -> io::Result<(Store, Vec<String>)> {
        let mut store = Store::memory(limits);
        store.dir = Some(dir.to_path_buf());
        let state = dir.join("v2/state");
        fs::create_dir_all(&state)?;
        fs::create_dir_all(dir.join("v2/msg"))?;
        let mut skipped = Vec::new();
        for entry in fs::read_dir(&state)? {
            let path = entry?.path();
            let Some(id) = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(".json"))
                .filter(|n| !n.starts_with('.'))
                .map(str::to_string)
            else {
                continue;
            };
            let loaded = (|| {
                let meta = json::parse(&fs::read_to_string(&path).ok()?).ok()?;
                let text = fs::read_to_string(dir.join(format!("v2/msg/{id}.eml"))).ok()?;
                let body = text.split_once("\n\n").map(|(_, b)| b)?.to_string();
                Msg::from_meta(&meta, body).filter(|m| m.id == id)
            })();
            match loaded {
                Some(m) => {
                    let top = m.rcpts.iter().map(|r| r.seq).max().unwrap_or(0);
                    store.next_seq = store.next_seq.max(top + 1);
                    store.msgs.insert(id, m);
                }
                None => skipped.push(id),
            }
        }
        Ok((store, skipped))
    }

    pub fn get(&self, id: &str) -> Option<&Msg> {
        self.msgs.get(id)
    }

    fn fresh_id(&self, now: u64) -> String {
        let base = stamp(now);
        let mut id = base.clone();
        let mut n = 1;
        while self.msgs.contains_key(&id) {
            n += 1;
            id = format!("{base}-{n}");
        }
        id
    }

    fn unread_for(&self, addr: &str) -> usize {
        self.msgs
            .values()
            .filter(|m| {
                m.rcpt(addr)
                    .is_some_and(|r| matches!(r.state, State::Delivered | State::Announced))
            })
            .count()
    }

    fn queued_for(&self, repo: &str) -> usize {
        self.msgs
            .values()
            .flat_map(|m| m.rcpts.iter())
            .filter(|r| r.state == State::Queued && r.addr == repo)
            .count()
    }

    /// Who one address reaches now, before the caps: `(addr, Some(why))` for a refusal,
    /// `(repo, None)` with `queued` for a queue, `(repo/slot, None)` for a delivery.
    fn expand(dir: &Directory, from: &str, t: &Target) -> Vec<Outcome> {
        let offline = |r: &RepoView| !r.answering || r.live().next().is_none();
        let why_offline = |r: &RepoView| {
            if r.answering {
                "no live session".to_string()
            } else {
                "its container is not answering".to_string()
            }
        };
        match t {
            Target::All => dir
                .iter()
                .filter(|(_, r)| r.answering)
                .flat_map(|(name, r)| r.live().map(move |s| format!("{name}/{}", s.slot)))
                .filter(|a| a != from)
                .map(Outcome::Delivered)
                .collect(),
            Target::Repo(repo) => match dir.get(repo) {
                None => vec![Outcome::Refused(t.show(), "no such repo".into())],
                Some(r) if offline(r) => vec![Outcome::Queued(repo.clone(), why_offline(r))],
                Some(r) => {
                    let out: Vec<Outcome> = r
                        .live()
                        .map(|s| format!("{repo}/{}", s.slot))
                        .filter(|a| a != from)
                        .map(Outcome::Delivered)
                        .collect();
                    if out.is_empty() {
                        vec![Outcome::Refused(
                            t.show(),
                            "no live session but yours".into(),
                        )]
                    } else {
                        out
                    }
                }
            },
            Target::One(repo, slot) => {
                let addr = t.show();
                match dir.get(repo) {
                    None => vec![Outcome::Refused(addr, "no such repo".into())],
                    Some(r) if !r.answering => vec![Outcome::Queued(repo.clone(), why_offline(r))],
                    Some(r) => match r.slot(slot) {
                        Some(s) if s.live => vec![Outcome::Delivered(addr)],
                        Some(s) if s.offloaded => {
                            vec![Outcome::Refused(addr, "offloaded".into())]
                        }
                        _ => vec![Outcome::Refused(addr, "no such session".into())],
                    },
                }
            }
        }
    }

    /// A send from `from` (a session, already checked live), routed over `dir` at `now`.
    pub fn send(
        &mut self,
        from: &str,
        req: Send,
        dir: &Directory,
        now: u64,
    ) -> Result<Sent, Refusal> {
        if req.subject.trim().is_empty() {
            return Err(refuse(400, "a message needs a subject"));
        }
        if req.subject.chars().any(char::is_control) || req.subject.len() > 200 {
            return Err(refuse(400, "a subject is one line of at most 200 bytes"));
        }
        if req.body.len() > self.limits.max_bytes {
            return Err(refuse(
                413,
                format!("a body is at most {} bytes", self.limits.max_bytes),
            ));
        }
        if req.to.is_empty() {
            return Err(refuse(400, "a message needs a To"));
        }
        let mut targets: Vec<(Target, bool)> = Vec::new();
        for (list, is_to) in [(&req.to, true), (&req.cc, false)] {
            for a in list {
                let t = Target::parse(a).map_err(|e| refuse(400, e))?;
                if let Target::One(r, _) | Target::Repo(r) = &t {
                    if RESERVED.contains(&r.as_str()) {
                        return Err(refuse(400, format!("{r:?} takes no mail")));
                    }
                }
                if !targets.iter().any(|(x, _)| *x == t) {
                    targets.push((t, is_to));
                }
            }
        }
        if let Some(r) = &req.in_reply_to {
            if !self
                .msgs
                .get(r)
                .is_some_and(|m| m.view(from, false).is_some())
            {
                return Err(refuse(
                    400,
                    format!("{r} is not a message you can reply to"),
                ));
            }
        }
        let recent = self.sent_at.entry(from.to_string()).or_default();
        while recent.front().is_some_and(|&t| t + RATE_WINDOW_MS <= now) {
            recent.pop_front();
        }
        if recent.len() >= self.limits.rate {
            return Err(refuse(
                429,
                format!(
                    "{} sends in half an hour is the limit for one session; wait",
                    self.limits.rate
                ),
            ));
        }
        // Route, then the caps, each recipient once.
        let mut outcomes: Vec<Outcome> = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for (t, _) in &targets {
            for o in Self::expand(dir, from, t) {
                let key = match &o {
                    Outcome::Delivered(a) | Outcome::Refused(a, _) => a.clone(),
                    Outcome::Queued(r, _) => format!("{r} (queue)"),
                };
                if !seen.insert(key) {
                    continue;
                }
                let o = match o {
                    Outcome::Delivered(a) if self.unread_for(&a) >= self.limits.max_queue => {
                        Outcome::Refused(a, "its mailbox is full".into())
                    }
                    Outcome::Queued(r, _) if self.queued_for(&r) >= self.limits.max_queue => {
                        Outcome::Refused(r, "its queue is full".into())
                    }
                    o => o,
                };
                outcomes.push(o);
            }
        }
        if !outcomes.iter().any(|o| !matches!(o, Outcome::Refused(..))) {
            return Err(Refusal {
                status: 409,
                why: if outcomes.is_empty() {
                    "nobody is live to receive it".into()
                } else {
                    "no recipient could take it".into()
                },
                outcomes,
            });
        }
        let id = self.fresh_id(now);
        let mut rcpts = Vec::new();
        let mut delivered = Vec::new();
        for o in &outcomes {
            match o {
                Outcome::Delivered(a) => {
                    rcpts.push(Rcpt {
                        addr: a.clone(),
                        state: State::Delivered,
                        seq: self.next_seq,
                        queued_since: None,
                        at: now,
                    });
                    self.next_seq += 1;
                    delivered.push(a.clone());
                }
                Outcome::Queued(r, _) => rcpts.push(Rcpt {
                    addr: r.clone(),
                    state: State::Queued,
                    seq: 0,
                    queued_since: Some(now),
                    at: now,
                }),
                Outcome::Refused(..) => {}
            }
        }
        let single = targets.len() == 1 && matches!(targets[0].0, Target::One(..));
        let msg = Msg {
            id: id.clone(),
            from: from.to_string(),
            to: targets
                .iter()
                .filter(|(_, to)| *to)
                .map(|(t, _)| t.show())
                .collect(),
            cc: targets
                .iter()
                .filter(|(_, to)| !*to)
                .map(|(t, _)| t.show())
                .collect(),
            subject: req.subject.trim().to_string(),
            date: now,
            in_reply_to: req.in_reply_to,
            reply_wanted: req.reply_wanted || single,
            body: req.body,
            rcpts,
        };
        self.persist(&msg, true)
            .map_err(|e| refuse(503, format!("the store could not keep it: {e}")))?;
        self.msgs.insert(id.clone(), msg);
        self.sent_at
            .entry(from.to_string())
            .or_default()
            .push_back(now);
        Ok(Sent {
            id,
            outcomes,
            delivered,
        })
    }

    /// Queued mail handed to the first live session of its repo, now that `dir` shows one:
    /// each `(id, repo/slot)` given, for the log and the nudge.
    pub fn claim(&mut self, dir: &Directory, now: u64) -> io::Result<Vec<(String, String)>> {
        let mut given = Vec::new();
        let ids: Vec<String> = self.msgs.keys().cloned().collect();
        for id in ids {
            let mut changed = false;
            let Some(m) = self.msgs.get_mut(&id) else {
                continue;
            };
            for r in m.rcpts.iter_mut().filter(|r| r.state == State::Queued) {
                let Some(repo) = dir.get(&r.addr).filter(|v| v.answering) else {
                    continue;
                };
                let Some(first) = repo.live().next() else {
                    continue;
                };
                r.addr = format!("{}/{}", r.addr, first.slot);
                r.state = State::Delivered;
                r.seq = self.next_seq;
                r.at = now;
                self.next_seq += 1;
                given.push((id.clone(), r.addr.clone()));
                changed = true;
            }
            if changed {
                let m = m.clone();
                self.persist(&m, false)?;
            }
        }
        Ok(given)
    }

    /// `who`'s mail, oldest first.
    pub fn fetch(&self, who: &str, f: Fetch) -> Vec<&Msg> {
        let mut out: Vec<(&Msg, u64)> = self
            .msgs
            .values()
            .filter_map(|m| {
                let r = m.rcpt(who)?;
                let keep = r.state != State::Queued
                    && r.seq > f.since
                    && (!f.unannounced || r.state == State::Delivered)
                    && (!f.unread || r.state != State::Read);
                keep.then_some((m, r.seq))
            })
            .collect();
        out.sort_by_key(|(_, seq)| *seq);
        out.into_iter().map(|(m, _)| m).collect()
    }

    /// The newest `seq` `who` has been given: where a `wait` starts from.
    pub fn top_seq(&self, who: &str) -> u64 {
        self.msgs
            .values()
            .filter_map(|m| m.rcpt(who).map(|r| r.seq))
            .max()
            .unwrap_or(0)
    }

    /// `ids` of `who`'s copies moved on to `to`, never back: the ids that changed.
    pub fn ack(
        &mut self,
        who: &str,
        ids: &[String],
        to: State,
        now: u64,
    ) -> io::Result<Vec<String>> {
        let mut changed = Vec::new();
        for id in ids {
            let Some(m) = self.msgs.get_mut(id) else {
                continue;
            };
            let Some(r) = m.rcpts.iter_mut().find(|r| r.addr == who) else {
                continue;
            };
            if r.state == State::Queued || r.state >= to {
                continue;
            }
            r.state = to;
            r.at = now;
            let m = m.clone();
            self.persist(&m, false)?;
            changed.push(id.clone());
        }
        Ok(changed)
    }

    /// Messages older than the retention, gone from memory and disk: how many.
    pub fn prune(&mut self, now: u64) -> io::Result<usize> {
        let old: Vec<String> = self
            .msgs
            .values()
            .filter(|m| m.date + self.limits.retain_ms < now)
            .map(|m| m.id.clone())
            .collect();
        for id in &old {
            if let Some(dir) = &self.dir {
                for p in [
                    dir.join(format!("v2/state/{id}.json")),
                    dir.join(format!("v2/msg/{id}.eml")),
                ] {
                    match fs::remove_file(&p) {
                        Ok(()) => {}
                        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                        Err(e) => return Err(e),
                    }
                }
            }
            self.msgs.remove(id);
        }
        Ok(old.len())
    }

    fn persist(&self, m: &Msg, new: bool) -> io::Result<()> {
        let Some(dir) = &self.dir else { return Ok(()) };
        if new {
            put(
                &dir.join(format!("v2/msg/{}.eml", m.id)),
                m.text(None).as_bytes(),
            )?;
        }
        put(
            &dir.join(format!("v2/state/{}.json", m.id)),
            json::to_line(&m.meta()).as_bytes(),
        )
    }
}

/// `bytes` at `path` whole and durably: a temporary name beside it, fsynced, renamed over it,
/// and the directory fsynced.
fn put(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no directory"))?;
    fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(
        ".{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("mail")
    ));
    let mut f = fs::File::create(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    drop(f);
    fs::rename(&tmp, path)?;
    fs::File::open(parent)?.sync_all()
}

#[cfg(test)]
#[allow(clippy::cloned_ref_to_slice_refs)] // ids compared as the lists they are
mod tests {
    use super::*;

    const NOW: u64 = 1_793_157_300_123;

    fn slot(name: &str, live: bool) -> SlotView {
        SlotView {
            slot: name.into(),
            live,
            offloaded: !live,
            title: format!("{name} title"),
            ..SlotView::default()
        }
    }

    /// infra: claude-1 live, claude-2 offloaded. or3: claude-1 and claude-2 live. dossier: not
    /// answering. quiet: answering, nothing live.
    fn dir() -> Directory {
        let mut d = Directory::new();
        d.insert(
            "infra".into(),
            RepoView {
                answering: true,
                slots: vec![slot("claude-1", true), slot("claude-2", false)],
            },
        );
        d.insert(
            "or3".into(),
            RepoView {
                answering: true,
                slots: vec![slot("claude-1", true), slot("claude-2", true)],
            },
        );
        d.insert("dossier".into(), RepoView::default());
        d.insert(
            "quiet".into(),
            RepoView {
                answering: true,
                slots: vec![slot("claude-1", false)],
            },
        );
        d
    }

    fn msg(to: &[&str]) -> Send {
        Send {
            to: to.iter().map(|s| s.to_string()).collect(),
            subject: "hello".into(),
            body: "body\n".into(),
            ..Send::default()
        }
    }

    fn lines(s: &Sent) -> Vec<String> {
        s.outcomes.iter().map(Outcome::line).collect()
    }

    #[test]
    fn addresses_parse_and_bare_repo_means_every_session() {
        assert_eq!(
            Target::parse("or3/claude-1"),
            Ok(Target::One("or3".into(), "claude-1".into()))
        );
        assert_eq!(Target::parse("or3/*"), Ok(Target::Repo("or3".into())));
        assert_eq!(Target::parse("or3"), Ok(Target::Repo("or3".into())));
        assert_eq!(Target::parse("*"), Ok(Target::All));
        for bad in ["", "/x", "or3/", "a b", "or3/c/d", ".hidden", "or3/.x", "ü"] {
            assert!(Target::parse(bad).is_err(), "{bad:?} parsed");
        }
    }

    #[test]
    fn one_send_says_what_became_of_each_recipient() {
        let mut s = Store::memory(Limits::default());
        let sent = s
            .send(
                "infra/claude-1",
                msg(&[
                    "or3/claude-1",
                    "dossier",
                    "infra/claude-2",
                    "infra/claude-9",
                    "nope",
                ]),
                &dir(),
                NOW,
            )
            .unwrap();
        assert_eq!(
            lines(&sent),
            [
                "delivered or3/claude-1",
                "queued dossier (its container is not answering)",
                "refused infra/claude-2 (offloaded)",
                "refused infra/claude-9 (no such session)",
                "refused nope/* (no such repo)",
            ]
        );
        assert_eq!(sent.delivered, ["or3/claude-1"]);
        let m = s.get(&sent.id).unwrap();
        assert!(!m.reply_wanted, "five addresses do not ask for a reply");
    }

    #[test]
    fn a_fan_out_reaches_every_live_session_but_the_senders_own() {
        let mut s = Store::memory(Limits::default());
        let all = s.send("or3/claude-1", msg(&["*"]), &dir(), NOW).unwrap();
        assert_eq!(
            lines(&all),
            ["delivered infra/claude-1", "delivered or3/claude-2"]
        );
        let repo = s
            .send("or3/claude-1", msg(&["or3/*"]), &dir(), NOW)
            .unwrap();
        assert_eq!(lines(&repo), ["delivered or3/claude-2"]);
        // A repo with nothing live queues; `*` does not reach it, and does not queue.
        let quiet = s
            .send("or3/claude-1", msg(&["quiet"]), &dir(), NOW)
            .unwrap();
        assert_eq!(lines(&quiet), ["queued quiet (no live session)"]);
        // The sender alone in its repo: nobody else to take it.
        let mut d = dir();
        d.get_mut("or3").unwrap().slots.truncate(1);
        let err = s.send("or3/claude-1", msg(&["or3"]), &d, NOW).unwrap_err();
        assert_eq!(err.status, 409);
        assert_eq!(
            err.outcomes[0].line(),
            "refused or3/* (no live session but yours)"
        );
    }

    #[test]
    fn a_recipient_named_twice_gets_one_copy() {
        let mut s = Store::memory(Limits::default());
        let mut m = msg(&["or3/claude-1", "or3"]);
        m.cc = vec!["or3/claude-1".into(), "*".into()];
        let sent = s.send("infra/claude-1", m, &dir(), NOW).unwrap();
        assert_eq!(
            lines(&sent),
            ["delivered or3/claude-1", "delivered or3/claude-2"]
        );
        let stored = s.get(&sent.id).unwrap();
        assert_eq!(stored.to, ["or3/claude-1", "or3/*"]);
        assert_eq!(stored.cc, ["*"]);
    }

    #[test]
    fn a_send_nobody_can_take_stores_nothing() {
        let mut s = Store::memory(Limits::default());
        let err = s
            .send("infra/claude-1", msg(&["infra/claude-2"]), &dir(), NOW)
            .unwrap_err();
        assert_eq!(err.status, 409);
        assert_eq!(err.outcomes.len(), 1);
        assert!(s.msgs.is_empty());
    }

    #[test]
    fn what_a_send_carries_is_checked() {
        let mut s = Store::memory(Limits {
            max_bytes: 8,
            ..Limits::default()
        });
        let d = dir();
        let mut bad = msg(&["or3/claude-1"]);
        bad.subject = "two\nlines".into();
        assert_eq!(
            s.send("infra/claude-1", bad, &d, NOW).unwrap_err().status,
            400
        );
        let mut bad = msg(&["or3/claude-1"]);
        bad.subject = "  ".into();
        assert_eq!(
            s.send("infra/claude-1", bad, &d, NOW).unwrap_err().status,
            400
        );
        let mut bad = msg(&["or3/claude-1"]);
        bad.body = "123456789".into();
        assert_eq!(
            s.send("infra/claude-1", bad, &d, NOW).unwrap_err().status,
            413
        );
        assert_eq!(
            s.send("infra/claude-1", msg(&[]), &d, NOW)
                .unwrap_err()
                .status,
            400
        );
        for reserved in ["hub", "owner/claude-1"] {
            assert_eq!(
                s.send("infra/claude-1", msg(&[reserved]), &d, NOW)
                    .unwrap_err()
                    .status,
                400
            );
        }
        let mut bad = msg(&["or3/claude-1"]);
        bad.in_reply_to = Some("20990101T000000.000Z".into());
        assert_eq!(
            s.send("infra/claude-1", bad, &d, NOW).unwrap_err().status,
            400
        );
        // Calibration: the same, well formed, goes.
        let mut ok = msg(&["or3/claude-1"]);
        ok.body = "12345678".into();
        assert!(s.send("infra/claude-1", ok, &d, NOW).is_ok());
    }

    #[test]
    fn a_reply_names_a_message_its_sender_could_read() {
        let mut s = Store::memory(Limits::default());
        let d = dir();
        let first = s
            .send("infra/claude-1", msg(&["or3/claude-1"]), &d, NOW)
            .unwrap();
        let mut reply = msg(&["infra/claude-1"]);
        reply.in_reply_to = Some(first.id.clone());
        assert!(s.send("or3/claude-1", reply.clone(), &d, NOW + 1).is_ok());
        // or3/claude-2 was not sent it, so cannot reply to it.
        assert_eq!(
            s.send("or3/claude-2", reply, &d, NOW + 2)
                .unwrap_err()
                .status,
            400
        );
    }

    #[test]
    fn one_session_is_held_to_its_rate() {
        let mut s = Store::memory(Limits {
            rate: 2,
            ..Limits::default()
        });
        let d = dir();
        for i in 0..2 {
            assert!(
                s.send("infra/claude-1", msg(&["or3/claude-1"]), &d, NOW + i)
                    .is_ok()
            );
        }
        assert_eq!(
            s.send("infra/claude-1", msg(&["or3/claude-1"]), &d, NOW + 2)
                .unwrap_err()
                .status,
            429
        );
        // Another session is not; and the window passes.
        assert!(
            s.send("or3/claude-2", msg(&["or3/claude-1"]), &d, NOW + 3)
                .is_ok()
        );
        assert!(
            s.send(
                "infra/claude-1",
                msg(&["or3/claude-1"]),
                &d,
                NOW + RATE_WINDOW_MS
            )
            .is_ok()
        );
    }

    #[test]
    fn a_full_mailbox_or_queue_refuses_that_recipient_only() {
        let mut s = Store::memory(Limits {
            max_queue: 1,
            ..Limits::default()
        });
        let d = dir();
        s.send("infra/claude-1", msg(&["or3/claude-1", "dossier"]), &d, NOW)
            .unwrap();
        let sent = s
            .send(
                "infra/claude-1",
                msg(&["or3/claude-1", "dossier", "or3/claude-2"]),
                &d,
                NOW + 1,
            )
            .unwrap();
        assert_eq!(
            lines(&sent),
            [
                "refused or3/claude-1 (its mailbox is full)",
                "refused dossier (its queue is full)",
                "delivered or3/claude-2",
            ]
        );
        // Read mail no longer counts.
        let id = s.fetch("or3/claude-1", Fetch::default())[0].id.clone();
        s.ack("or3/claude-1", &[id], State::Read, NOW + 2).unwrap();
        let again = s
            .send("infra/claude-1", msg(&["or3/claude-1"]), &d, NOW + 3)
            .unwrap();
        assert_eq!(lines(&again), ["delivered or3/claude-1"]);
    }

    #[test]
    fn states_only_move_forward_and_fetch_sees_them() {
        let mut s = Store::memory(Limits::default());
        let d = dir();
        let a = s
            .send("infra/claude-1", msg(&["or3/claude-1"]), &d, NOW)
            .unwrap()
            .id;
        let b = s
            .send("infra/claude-1", msg(&["or3/claude-1"]), &d, NOW + 1)
            .unwrap()
            .id;
        let me = "or3/claude-1";
        let ids = |v: Vec<&Msg>| v.iter().map(|m| m.id.clone()).collect::<Vec<_>>();
        let un = Fetch {
            unannounced: true,
            ..Fetch::default()
        };
        assert_eq!(ids(s.fetch(me, un)), [a.clone(), b.clone()]);
        assert_eq!(
            s.ack(me, &[a.clone()], State::Announced, NOW).unwrap(),
            [a.clone()]
        );
        assert_eq!(ids(s.fetch(me, un)), [b.clone()]);
        assert_eq!(
            s.ack(me, &[a.clone()], State::Read, NOW).unwrap(),
            [a.clone()]
        );
        // Never back.
        assert!(
            s.ack(me, &[a.clone()], State::Announced, NOW)
                .unwrap()
                .is_empty()
        );
        let unread = Fetch {
            unread: true,
            ..Fetch::default()
        };
        assert_eq!(ids(s.fetch(me, unread)), [b.clone()]);
        let since = Fetch {
            since: s.get(&a).unwrap().rcpts[0].seq,
            ..Fetch::default()
        };
        assert_eq!(ids(s.fetch(me, since)), [b.clone()]);
        assert_eq!(s.top_seq(me), s.get(&b).unwrap().rcpts[0].seq);
        // Somebody else acknowledging changes nothing of mine.
        assert!(
            s.ack("or3/claude-2", &[b.clone()], State::Read, NOW)
                .unwrap()
                .is_empty()
        );
        assert_eq!(s.fetch(me, unread).len(), 1);
    }

    #[test]
    fn queued_mail_goes_to_the_first_session_seen_live() {
        let mut s = Store::memory(Limits::default());
        let mut d = dir();
        let id = s
            .send("infra/claude-1", msg(&["dossier"]), &d, NOW)
            .unwrap()
            .id;
        assert!(s.fetch("dossier/claude-1", Fetch::default()).is_empty());
        assert!(s.claim(&d, NOW + 1).unwrap().is_empty(), "still offline");
        d.insert(
            "dossier".into(),
            RepoView {
                answering: true,
                slots: vec![slot("claude-3", true), slot("claude-4", true)],
            },
        );
        assert_eq!(
            s.claim(&d, NOW + 5).unwrap(),
            [(id.clone(), "dossier/claude-3".to_string())]
        );
        assert!(s.claim(&d, NOW + 6).unwrap().is_empty(), "once");
        let got = s.fetch("dossier/claude-3", Fetch::default());
        assert_eq!(got.len(), 1);
        let text = got[0].text(Some("dossier/claude-3"));
        assert!(
            text.contains(&format!("Queued-Since: {}\n", utc(NOW))),
            "{text}"
        );
        assert!(!got[0].text(None).contains("Queued-Since"));
    }

    #[test]
    fn the_text_is_the_hubs_headers_then_the_body() {
        let mut s = Store::memory(Limits::default());
        let d = dir();
        let mut m = msg(&["or3/claude-1"]);
        m.cc = vec!["infra".into()];
        let first = s
            .send("or3/claude-2", msg(&["infra/claude-1"]), &d, NOW)
            .unwrap()
            .id;
        m.in_reply_to = Some(first.clone());
        let id = s.send("infra/claude-1", m, &d, NOW + 1000).unwrap().id;
        let text = s.get(&id).unwrap().text(None);
        assert_eq!(
            text,
            format!(
                "From: infra/claude-1\nTo: or3/claude-1\nCc: infra/*\nSubject: hello\n\
                 Date: {}\nMessage-Id: <{id}@hub>\nIn-Reply-To: <{first}@hub>\n\
                 Reply-Wanted: no\n\nbody\n",
                utc(NOW + 1000)
            )
        );
        let single = s
            .send("infra/claude-1", msg(&["or3/claude-1"]), &d, NOW + 2000)
            .unwrap();
        assert!(
            s.get(&single.id).unwrap().reply_wanted,
            "one session wants a reply"
        );
    }

    #[test]
    fn a_view_is_for_its_recipients_and_its_sender() {
        let mut s = Store::memory(Limits::default());
        let sent = s
            .send("infra/claude-1", msg(&["or3/claude-1"]), &dir(), NOW)
            .unwrap();
        let m = s.get(&sent.id).unwrap();
        let mine = m.view("or3/claude-1", true).unwrap();
        assert_eq!(mine.get("state").and_then(Value::as_str), Some("delivered"));
        assert!(mine.get("recipients").is_none());
        assert!(
            mine.get("text")
                .and_then(Value::as_str)
                .unwrap()
                .starts_with("From: ")
        );
        let theirs = m.view("infra/claude-1", false).unwrap();
        assert!(theirs.get("recipients").is_some());
        assert!(theirs.get("body").is_none());
        assert!(m.view("or3/claude-2", false).is_none());
    }

    #[test]
    fn the_store_survives_a_restart_and_prunes_by_age() {
        let root = std::env::temp_dir().join(format!("cs-mailbox-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let limits = Limits::default();
        let (mut s, skipped) = Store::open(&root, limits).unwrap();
        assert!(skipped.is_empty());
        let d = dir();
        let a = s
            .send("infra/claude-1", msg(&["or3/claude-1", "dossier"]), &d, NOW)
            .unwrap();
        s.ack("or3/claude-1", &[a.id.clone()], State::Announced, NOW)
            .unwrap();
        // A file that is not a message is skipped and named.
        fs::write(root.join("v2/state/junk.json"), "{").unwrap();
        let (mut again, skipped) = Store::open(&root, limits).unwrap();
        assert_eq!(skipped, ["junk"]);
        let m = again.get(&a.id).unwrap().clone();
        assert_eq!(&m, s.get(&a.id).unwrap());
        assert_eq!(
            fs::read_to_string(root.join(format!("v2/msg/{}.eml", a.id))).unwrap(),
            m.text(None)
        );
        // New seqs follow the old ones.
        let b = again
            .send("infra/claude-1", msg(&["or3/claude-1"]), &d, NOW + 1)
            .unwrap();
        assert!(again.get(&b.id).unwrap().rcpts[0].seq > m.rcpts[0].seq);
        assert_eq!(again.prune(NOW + limits.retain_ms).unwrap(), 0);
        assert_eq!(again.prune(NOW + limits.retain_ms + 1).unwrap(), 1);
        assert!(!root.join(format!("v2/msg/{}.eml", a.id)).exists());
        assert!(!root.join(format!("v2/state/{}.json", a.id)).exists());
        assert!(again.get(&b.id).is_some());
        let _ = fs::remove_dir_all(&root);
    }
}
