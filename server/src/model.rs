//! In-memory relay state, rebuilt by replaying the event logs.
//!
//! `apply` is deterministic. The relay that writes an event makes every decision (whether a request
//! is void, which recipients accepted a file, what gets recycled) and records it in the event, so
//! replaying the same logs yields the same state on every relay. `apply` never panics on bad input:
//! it ignores what it cannot use and defers events whose dependencies have not arrived yet (events
//! from different relays can arrive in any order).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::crypto::{b64url_decode, unhex};
use crate::ids::{Aid, parse_id64};
use crate::json::{Js, Json};

pub type Hash = [u8; 32];

/// Event kinds (the `kind` field of a log record).
pub mod kind {
    pub const RELAY: u16 = 1;
    pub const SETTING: u16 = 2;
    pub const ACCOUNT: u16 = 10;
    pub const ACCOUNT_SET: u16 = 11;
    pub const SESSION: u16 = 12;
    pub const SESSION_END: u16 = 13;
    pub const INVITE: u16 = 14;
    pub const INVITE_DEL: u16 = 16;
    pub const REQUEST: u16 = 20;
    pub const REQUEST_ANS: u16 = 21;
    pub const BLOCK: u16 = 22;
    pub const NICK: u16 = 23;
    pub const CONV: u16 = 30;
    pub const MEMBER: u16 = 31;
    pub const CONV_SET: u16 = 32;
    pub const MSG: u16 = 40;
    pub const FILE: u16 = 50;
    pub const BLOB: u16 = 51;
    pub const READ: u16 = 60;
    pub const RECOVERY: u16 = 70;
    pub const RECOVERY_USE: u16 = 71;
}

const GIB: u64 = 1 << 30;
const DAY_MS: u64 = 86_400_000;

// ------------------------------------------------------------------------------------------------
// Settings (admin), last-writer-wins per key
// ------------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Signup {
    Invite,
    Open,
    Closed,
}

impl Signup {
    pub fn as_str(self) -> &'static str {
        match self {
            Signup::Invite => "invite",
            Signup::Open => "open",
            Signup::Closed => "closed",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Smtp {
    pub host: String,
    pub port: u64,
    pub user: String,
    pub pass: String,
    pub from: String,
    /// "ssl" (implicit TLS, usually 465) or "starttls" (usually 587).
    pub tls: String,
}

#[derive(Clone, Debug)]
pub struct Settings {
    pub default_quota: u64,
    pub flex_pct: u64,
    pub cleanup_h: u64,
    pub reserve_pct: u64,
    pub reserve_min: u64,
    pub signup: Signup,
    /// Invites each non-admin member may create (0 = only admins invite).
    pub member_invites: u64,
    pub req_pending_max: u64,
    pub req_daily_max: u64,
    pub smtp: Smtp,
    stamps: HashMap<String, (u64, u64)>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            default_quota: GIB,
            flex_pct: 10,
            cleanup_h: 24,
            reserve_pct: 5,
            reserve_min: 2 * GIB,
            signup: Signup::Invite,
            member_invites: 0,
            req_pending_max: 20,
            req_daily_max: 50,
            smtp: Smtp::default(),
            stamps: HashMap::new(),
        }
    }
}

pub const SETTING_KEYS: &[&str] = &[
    "default_quota",
    "flex_pct",
    "cleanup_h",
    "reserve_pct",
    "reserve_min",
    "signup",
    "member_invites",
    "req_pending_max",
    "req_daily_max",
    "smtp.host",
    "smtp.port",
    "smtp.user",
    "smtp.pass",
    "smtp.from",
    "smtp.tls",
];

impl Settings {
    /// Apply one key. Older stamps lose (last writer wins by (time, relay)). Returns false when the
    /// key or value is not acceptable.
    pub fn set(&mut self, k: &str, v: &Json, stamp: (u64, u64)) -> bool {
        if self.stamps.get(k).is_some_and(|&old| old > stamp) {
            return false;
        }
        let u = v.as_u64();
        let s = v.as_str().map(str::to_string);
        let ok = match (k, u, s) {
            ("default_quota", Some(x), _) => {
                self.default_quota = x;
                true
            }
            ("flex_pct", Some(x), _) if x <= 100 => {
                self.flex_pct = x;
                true
            }
            ("cleanup_h", Some(x), _) if x <= 24 * 365 => {
                self.cleanup_h = x;
                true
            }
            ("reserve_pct", Some(x), _) if x <= 90 => {
                self.reserve_pct = x;
                true
            }
            ("reserve_min", Some(x), _) => {
                self.reserve_min = x;
                true
            }
            ("member_invites", Some(x), _) => {
                self.member_invites = x;
                true
            }
            ("req_pending_max", Some(x), _) if x >= 1 => {
                self.req_pending_max = x;
                true
            }
            ("req_daily_max", Some(x), _) if x >= 1 => {
                self.req_daily_max = x;
                true
            }
            ("signup", _, Some(s)) => match s.as_str() {
                "invite" | "open" | "closed" => {
                    self.signup = match s.as_str() {
                        "open" => Signup::Open,
                        "closed" => Signup::Closed,
                        _ => Signup::Invite,
                    };
                    true
                }
                _ => false,
            },
            ("smtp.port", Some(x), _) if x <= 65535 => {
                self.smtp.port = x;
                true
            }
            ("smtp.host", _, Some(s)) => {
                self.smtp.host = s;
                true
            }
            ("smtp.user", _, Some(s)) => {
                self.smtp.user = s;
                true
            }
            ("smtp.pass", _, Some(s)) => {
                self.smtp.pass = s;
                true
            }
            ("smtp.from", _, Some(s)) => {
                self.smtp.from = s;
                true
            }
            ("smtp.tls", _, Some(s)) if s == "ssl" || s == "starttls" => {
                self.smtp.tls = s;
                true
            }
            _ => false,
        };
        if ok {
            self.stamps.insert(k.to_string(), stamp);
        }
        ok
    }

    pub fn quota(&self, a: &Account) -> u64 {
        a.quota.unwrap_or(self.default_quota)
    }

    /// quota × (1 + flex%): the most an auto-recycling account may hold when receiving.
    pub fn ceiling(&self, a: &Account) -> u64 {
        let q = self.quota(a) as u128;
        (q + q * self.flex_pct as u128 / 100).min(u64::MAX as u128) as u64
    }
}

// ------------------------------------------------------------------------------------------------
// Entities
// ------------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub enum Recovery {
    None,
    Email(String),
    Code(Hash),
}

#[derive(Clone, Debug, PartialEq)]
pub struct FileMeta {
    pub b: Hash,
    pub name: String,
    pub mime: String,
    pub z: u64,
    pub t: Option<(Hash, u64)>,
    pub p: Option<(Hash, u64)>,
    pub w: u32,
    pub h: u32,
}

fn hash_of(s: Option<&str>) -> Option<Hash> {
    unhex(s?)?.try_into().ok()
}

impl FileMeta {
    /// Original + thumbnail + preview: what counts against quota.
    pub fn charged(&self) -> u64 {
        self.z + self.t.map_or(0, |x| x.1) + self.p.map_or(0, |x| x.1)
    }

    pub fn blobs(&self) -> impl Iterator<Item = Hash> + '_ {
        [Some(self.b), self.t.map(|x| x.0), self.p.map(|x| x.0)].into_iter().flatten()
    }

    /// `{"b":hex,"n":name,"m":mime,"z":n,"t":hex,"tz":n,"p":hex,"pz":n,"w":n,"h":n}`
    pub fn from_json(j: &Json) -> Option<FileMeta> {
        let b = hash_of(j.s("b"))?;
        let name: String = j.s("n").unwrap_or("file").chars().filter(|c| !c.is_control()).take(200).collect();
        let mime: String = j.s("m").unwrap_or("application/octet-stream").chars().take(100).collect();
        let t = hash_of(j.s("t")).map(|h| (h, j.u("tz").unwrap_or(0)));
        let p = hash_of(j.s("p")).map(|h| (h, j.u("pz").unwrap_or(0)));
        Some(FileMeta {
            b,
            name: if name.is_empty() { "file".into() } else { name },
            mime,
            z: j.u("z")?,
            t,
            p,
            w: j.u("w").unwrap_or(0).min(1 << 20) as u32,
            h: j.u("h").unwrap_or(0).min(1 << 20) as u32,
        })
    }

    pub fn write(&self, js: &mut Js) {
        js.obj().ks("b", &crate::crypto::hex(&self.b)).ks("n", &self.name).ks("m", &self.mime).ku("z", self.z);
        if let Some((h, z)) = self.t {
            js.ks("t", &crate::crypto::hex(&h)).ku("tz", z);
        }
        if let Some((h, z)) = self.p {
            js.ks("p", &crate::crypto::hex(&h)).ku("pz", z);
        }
        if self.w > 0 {
            js.ku("w", self.w as u64).ku("h", self.h as u64);
        }
        js.end_obj();
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FState {
    Active,
    Refused,
    Recycled,
    Deleted,
}

impl FState {
    pub fn code(self) -> &'static str {
        match self {
            FState::Active => "ok",
            FState::Refused => "ref",
            FState::Recycled => "rec",
            FState::Deleted => "del",
        }
    }
}

/// One account's relationship to one file (blob).
#[derive(Clone, Debug)]
pub struct FileRef {
    pub meta: Arc<FileMeta>,
    pub state: FState,
    pub pinned: bool,
    /// When it last became active (recycling goes oldest first).
    pub added: u64,
    pub changed: u64,
    /// b'd' drive upload, b's' sent, b'r' received.
    pub dir: u8,
    pub convs: Vec<u64>,
}

#[derive(Clone, Debug)]
pub struct Account {
    pub id: Aid,
    pub created: u64,
    pub origin: u64,
    pub salt: [u8; 16],
    pub ver: Hash,
    pub name: String,
    pub admin: bool,
    pub disabled: bool,
    pub accept: bool,
    pub auto_recycle: bool,
    pub quota: Option<u64>,
    pub recovery: Recovery,
    pub cleanup_at: Option<u64>,
    pub cleanup_by: u64,
    pub blocked: HashSet<Aid>,
    pub nicks: HashMap<Aid, String>,
    pub files: HashMap<Hash, FileRef>,
    pub usage: u64,
    pub pinned: u64,
    /// Blobs this account uploaded and has not attached yet (upload time).
    pub uploads: HashMap<Hash, u64>,
    /// Conversations joined or (non-void) invited to.
    pub convs: Vec<u64>,
    /// Pending visible incoming requests / pending outgoing requests.
    pub req_in: Vec<u64>,
    pub req_out: Vec<u64>,
    /// Send times of recent requests (rate limit window).
    pub req_times: Vec<u64>,
    pub reads: HashMap<u64, u64>,
    pub invites_made: u64,
}

impl Account {
    pub fn recyclable(&self) -> u64 {
        self.usage.saturating_sub(self.pinned)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MState {
    Invited,
    Joined,
    Left,
    Declined,
    Removed,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    Owner,
    Admin,
    Member,
}

impl Role {
    pub fn code(self) -> &'static str {
        match self {
            Role::Owner => "o",
            Role::Admin => "a",
            Role::Member => "m",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Member {
    pub a: Aid,
    pub st: MState,
    pub role: Role,
    /// Joined at (sees messages from here on), or invited at.
    pub since: u64,
    pub by: Aid,
    /// Invite that the invitee will never see (no oracle for the inviter).
    pub void: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MsgRef {
    pub ts: u64,
    pub origin: u16,
    pub off: u64,
    pub from: Aid,
}

#[derive(Clone, Debug)]
pub struct Conv {
    pub id: u64,
    pub group: bool,
    pub name: String,
    pub created: u64,
    pub by: Aid,
    pub members: Vec<Member>,
    pub msgs: Vec<MsgRef>,
    /// Last message: (ref, text snippet, attachment count).
    pub last: Option<(MsgRef, String, u32)>,
}

impl Conv {
    pub fn member(&self, a: Aid) -> Option<&Member> {
        self.members.iter().find(|m| m.a == a)
    }
    fn member_mut(&mut self, a: Aid) -> Option<&mut Member> {
        self.members.iter_mut().find(|m| m.a == a)
    }
    pub fn joined(&self, a: Aid) -> bool {
        self.member(a).is_some_and(|m| m.st == MState::Joined)
    }
    /// Joined + invited: what counts against the 128-account limit.
    pub fn seats(&self) -> usize {
        self.members.iter().filter(|m| matches!(m.st, MState::Joined | MState::Invited)).count()
    }
    pub fn joined_ids(&self) -> impl Iterator<Item = Aid> + '_ {
        self.members.iter().filter(|m| m.st == MState::Joined).map(|m| m.a)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RState {
    Pending,
    Accepted,
    Declined,
    Canceled,
}

#[derive(Clone, Debug)]
pub struct Req {
    pub id: u64,
    pub from: Aid,
    pub to: Aid,
    pub note: String,
    pub ts: u64,
    pub st: RState,
    pub void: bool,
    pub conv: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct Session {
    pub a: Aid,
    pub created: u64,
    pub label: String,
}

#[derive(Clone, Debug)]
pub struct Invite {
    pub code: String,
    pub by: Option<Aid>,
    pub uses: u64,
    pub exp: u64,
    pub admin: bool,
    pub created: u64,
    pub used_by: Vec<Aid>,
}

#[derive(Clone, Debug)]
pub struct Relay {
    pub id: u64,
    pub name: String,
    pub url: String,
    pub ts: u64,
}

#[derive(Clone, Debug)]
pub struct Blob {
    pub z: u64,
    /// Active file refs holding this blob (as original, thumbnail or preview).
    pub refs: u32,
    /// Accounts holding it as an unattached upload.
    pub pending: u32,
    pub ts: u64,
}

/// One event to apply.
#[derive(Clone, Debug)]
pub struct Ev {
    pub origin: u16,
    pub off: u64,
    pub ts: u64,
    pub kind: u16,
    pub j: Json,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Applied,
    Deferred,
    Ignored(&'static str),
}

#[derive(Default)]
pub struct State {
    pub settings: Settings,
    pub relays: HashMap<u64, Relay>,
    /// origin index -> relay id (filled by the core as it opens logs).
    pub origins: Vec<u64>,
    pub accounts: HashMap<Aid, Account>,
    pub sessions: HashMap<Hash, Session>,
    pub invites: HashMap<String, Invite>,
    pub reqs: HashMap<u64, Req>,
    pub convs: HashMap<u64, Conv>,
    pub direct: HashMap<(Aid, Aid), u64>,
    pub blobs: HashMap<Hash, Blob>,
    pub recov: HashMap<Hash, (Aid, u64)>,
    pub short_ids: usize,
    /// Blobs whose last reference went away while applying; the core deletes their files.
    pub dead_blobs: Vec<Hash>,
    pub deferred: Vec<Ev>,
}

fn pair(a: Aid, b: Aid) -> (Aid, Aid) {
    if a <= b { (a, b) } else { (b, a) }
}

fn aid(j: &Json, k: &str) -> Option<Aid> {
    Aid::parse(j.s(k)?)
}

fn id64(j: &Json, k: &str) -> Option<u64> {
    parse_id64(j.s(k)?)
}

fn snippet(text: &str) -> String {
    text.chars().take(120).collect::<String>().replace(['\n', '\r'], " ")
}

fn remove_id(v: &mut Vec<u64>, x: u64) {
    v.retain(|&y| y != x);
}

fn push_unique(v: &mut Vec<u64>, x: u64) {
    if !v.contains(&x) {
        v.push(x);
    }
}

impl State {
    /// Apply one event; on success also retries events that were waiting for it.
    pub fn apply(&mut self, ev: Ev) -> Outcome {
        match self.apply_one(&ev) {
            Outcome::Deferred => {
                if self.deferred.len() < 100_000 {
                    self.deferred.push(ev);
                }
                Outcome::Deferred
            }
            Outcome::Applied => {
                if !self.deferred.is_empty() {
                    self.retry_deferred();
                }
                Outcome::Applied
            }
            other => other,
        }
    }

    fn retry_deferred(&mut self) {
        loop {
            let pending = std::mem::take(&mut self.deferred);
            let mut progressed = false;
            for ev in pending {
                match self.apply_one(&ev) {
                    Outcome::Deferred => self.deferred.push(ev),
                    Outcome::Applied => progressed = true,
                    Outcome::Ignored(_) => {}
                }
            }
            if !progressed || self.deferred.is_empty() {
                break;
            }
        }
    }

    fn origin_id(&self, origin: u16) -> u64 {
        self.origins.get(origin as usize).copied().unwrap_or(origin as u64)
    }

    /// Total order of messages: (time, relay id, offset).
    pub fn msg_key(&self, m: &MsgRef) -> (u64, u64, u64) {
        (m.ts, self.origin_id(m.origin), m.off)
    }

    fn apply_one(&mut self, ev: &Ev) -> Outcome {
        let j = &ev.j;
        let ts = ev.ts;
        use kind::*;
        match ev.kind {
            RELAY => {
                let Some(id) = id64(j, "r") else { return Outcome::Ignored("relay id") };
                self.relays.insert(
                    id,
                    Relay {
                        id,
                        name: j.s("n").unwrap_or("").to_string(),
                        url: j.s("u").unwrap_or("").to_string(),
                        ts,
                    },
                );
                Outcome::Applied
            }
            SETTING => {
                let (Some(k), Some(v)) = (j.s("k"), j.get("v")) else { return Outcome::Ignored("setting") };
                let stamp = (ts, self.origin_id(ev.origin));
                self.settings.set(k, v, stamp);
                Outcome::Applied
            }
            ACCOUNT => self.ev_account(ev),
            ACCOUNT_SET => self.ev_account_set(ev),
            SESSION => {
                let (Some(h), Some(a)) = (hash_b64(j.s("h")), aid(j, "a")) else {
                    return Outcome::Ignored("session");
                };
                if !self.accounts.contains_key(&a) {
                    return Outcome::Deferred;
                }
                self.sessions.insert(h, Session { a, created: ts, label: j.s("l").unwrap_or("").into() });
                Outcome::Applied
            }
            SESSION_END => {
                if let Some(h) = hash_b64(j.s("h")) {
                    self.sessions.remove(&h);
                } else if let Some(a) = aid(j, "a") {
                    let keep = hash_b64(j.s("keep"));
                    self.sessions.retain(|h, s| s.a != a || Some(*h) == keep);
                }
                Outcome::Applied
            }
            INVITE => {
                let Some(code) = j.s("c") else { return Outcome::Ignored("invite") };
                let by = aid(j, "by");
                if let Some(b) = by {
                    match self.accounts.get_mut(&b) {
                        Some(acc) => acc.invites_made += 1,
                        None => return Outcome::Deferred,
                    }
                }
                self.invites.insert(
                    code.to_string(),
                    Invite {
                        code: code.to_string(),
                        by,
                        uses: j.u("u").unwrap_or(1),
                        exp: j.u("x").unwrap_or(u64::MAX),
                        admin: j.b("adm").unwrap_or(false),
                        created: ts,
                        used_by: Vec::new(),
                    },
                );
                Outcome::Applied
            }
            INVITE_DEL => {
                if let Some(c) = j.s("c") {
                    self.invites.remove(c);
                }
                Outcome::Applied
            }
            REQUEST => self.ev_request(ev),
            REQUEST_ANS => self.ev_request_ans(ev),
            BLOCK => {
                let (Some(a), Some(t)) = (aid(j, "a"), aid(j, "t")) else { return Outcome::Ignored("block") };
                let Some(acc) = self.accounts.get_mut(&a) else { return Outcome::Deferred };
                if j.b("on").unwrap_or(true) {
                    acc.blocked.insert(t);
                } else {
                    acc.blocked.remove(&t);
                }
                Outcome::Applied
            }
            NICK => {
                let (Some(a), Some(t)) = (aid(j, "a"), aid(j, "t")) else { return Outcome::Ignored("nick") };
                let Some(acc) = self.accounts.get_mut(&a) else { return Outcome::Deferred };
                let n: String = j.s("n").unwrap_or("").trim().chars().take(40).collect();
                if n.is_empty() {
                    acc.nicks.remove(&t);
                } else {
                    acc.nicks.insert(t, n);
                }
                Outcome::Applied
            }
            CONV => self.ev_conv(ev),
            MEMBER => self.ev_member(ev),
            CONV_SET => {
                let Some(c) = id64(j, "c") else { return Outcome::Ignored("conv id") };
                let Some(conv) = self.convs.get_mut(&c) else { return Outcome::Deferred };
                if let Some(n) = j.s("n") {
                    conv.name = n.chars().take(80).collect();
                }
                Outcome::Applied
            }
            MSG => self.ev_msg(ev),
            FILE => self.ev_file(ev),
            BLOB => {
                let Some(b) = hash_of(j.s("b")) else { return Outcome::Ignored("blob") };
                let z = j.u("z").unwrap_or(0);
                let by = aid(j, "by");
                let blob = self.blobs.entry(b).or_insert(Blob { z, refs: 0, pending: 0, ts });
                if let Some(by) = by
                    && let Some(acc) = self.accounts.get_mut(&by)
                    && acc.uploads.insert(b, ts).is_none()
                {
                    blob.pending += 1;
                }
                Outcome::Applied
            }
            READ => {
                let (Some(a), Some(c), Some(t)) = (aid(j, "a"), id64(j, "c"), j.u("t")) else {
                    return Outcome::Ignored("read");
                };
                let Some(acc) = self.accounts.get_mut(&a) else { return Outcome::Deferred };
                let e = acc.reads.entry(c).or_insert(0);
                *e = (*e).max(t);
                Outcome::Applied
            }
            RECOVERY => {
                let (Some(a), Some(h), Some(x)) = (aid(j, "a"), hash_b64(j.s("h")), j.u("x")) else {
                    return Outcome::Ignored("recovery");
                };
                self.recov.insert(h, (a, x));
                Outcome::Applied
            }
            RECOVERY_USE => {
                if let Some(h) = hash_b64(j.s("h")) {
                    self.recov.remove(&h);
                }
                Outcome::Applied
            }
            _ => Outcome::Ignored("unknown kind"),
        }
    }

    fn ev_account(&mut self, ev: &Ev) -> Outcome {
        let j = &ev.j;
        let Some(id) = aid(j, "a") else { return Outcome::Ignored("account id") };
        let salt: Option<[u8; 16]> = j.s("s").and_then(b64url_decode).and_then(|v| v.try_into().ok());
        let (Some(salt), Some(ver)) = (salt, hash_b64(j.s("v"))) else {
            return Outcome::Ignored("account secrets");
        };
        if self.accounts.contains_key(&id) {
            // Two relays handed out the same ID inside one replication window. Resolved in the
            // ring milestone (M5); a single relay never does this.
            return Outcome::Ignored("account id collision");
        }
        if let Some(code) = j.s("inv")
            && let Some(inv) = self.invites.get_mut(code)
        {
            inv.uses = inv.uses.saturating_sub(1);
            inv.used_by.push(id);
        }
        if id.is_short() {
            self.short_ids += 1;
        }
        self.accounts.insert(
            id,
            Account {
                id,
                created: ev.ts,
                origin: self.origin_id(ev.origin),
                salt,
                ver,
                name: j.s("n").unwrap_or("").chars().take(40).collect(),
                admin: j.b("adm").unwrap_or(false),
                disabled: false,
                accept: true,
                auto_recycle: true,
                quota: None,
                recovery: Recovery::None,
                cleanup_at: None,
                cleanup_by: 0,
                blocked: HashSet::new(),
                nicks: HashMap::new(),
                files: HashMap::new(),
                usage: 0,
                pinned: 0,
                uploads: HashMap::new(),
                convs: Vec::new(),
                req_in: Vec::new(),
                req_out: Vec::new(),
                req_times: Vec::new(),
                reads: HashMap::new(),
                invites_made: 0,
            },
        );
        Outcome::Applied
    }

    fn ev_account_set(&mut self, ev: &Ev) -> Outcome {
        let j = &ev.j;
        let Some(id) = aid(j, "a") else { return Outcome::Ignored("account id") };
        let Some(acc) = self.accounts.get_mut(&id) else { return Outcome::Deferred };
        if let Some(n) = j.s("n") {
            acc.name = n.trim().chars().take(40).collect();
        }
        if let Some(v) = j.b("acc") {
            acc.accept = v;
        }
        if let Some(v) = j.b("ar") {
            acc.auto_recycle = v;
        }
        if let Some(q) = j.get("q") {
            acc.quota = q.as_u64();
        }
        if let Some(v) = j.b("adm") {
            acc.admin = v;
        }
        if let Some(v) = j.b("dis") {
            acc.disabled = v;
        }
        if let Some(r) = j.get("rec") {
            acc.recovery = match r.s("k") {
                Some("email") => Recovery::Email(r.s("e").unwrap_or("").to_string()),
                Some("code") => match hash_b64(r.s("v")) {
                    Some(h) => Recovery::Code(h),
                    None => Recovery::None,
                },
                _ => Recovery::None,
            };
        }
        if let Some(pw) = j.get("pw") {
            let salt: Option<[u8; 16]> = pw.s("s").and_then(b64url_decode).and_then(|v| v.try_into().ok());
            if let (Some(s), Some(v)) = (salt, hash_b64(pw.s("v"))) {
                acc.salt = s;
                acc.ver = v;
            }
        }
        if let Some(cl) = j.get("cl") {
            acc.cleanup_at = cl.as_u64();
            acc.cleanup_by = id64(j, "clb").unwrap_or(0);
        }
        Outcome::Applied
    }

    fn ev_request(&mut self, ev: &Ev) -> Outcome {
        let j = &ev.j;
        let (Some(r), Some(from), Some(to)) = (id64(j, "r"), aid(j, "f"), aid(j, "t")) else {
            return Outcome::Ignored("request");
        };
        let void = j.b("v").unwrap_or(true);
        let Some(acc) = self.accounts.get_mut(&from) else { return Outcome::Deferred };
        push_unique(&mut acc.req_out, r);
        acc.req_times.retain(|&t| t + DAY_MS > ev.ts);
        acc.req_times.push(ev.ts);
        if !void && let Some(t) = self.accounts.get_mut(&to) {
            push_unique(&mut t.req_in, r);
        }
        self.reqs.insert(
            r,
            Req {
                id: r,
                from,
                to,
                note: j.s("n").unwrap_or("").chars().take(280).collect(),
                ts: ev.ts,
                st: RState::Pending,
                void,
                conv: None,
            },
        );
        Outcome::Applied
    }

    fn ev_request_ans(&mut self, ev: &Ev) -> Outcome {
        let j = &ev.j;
        let Some(r) = id64(j, "r") else { return Outcome::Ignored("request id") };
        let Some(req) = self.reqs.get_mut(&r) else { return Outcome::Deferred };
        if req.st != RState::Pending {
            return Outcome::Ignored("request already answered");
        }
        let (from, to) = (req.from, req.to);
        match j.s("a") {
            Some("acc") => {
                let Some(c) = id64(j, "c") else { return Outcome::Ignored("conv id") };
                req.st = RState::Accepted;
                req.conv = Some(c);
                for a in [from, to] {
                    if let Some(acc) = self.accounts.get_mut(&a) {
                        remove_id(&mut acc.req_in, r);
                        remove_id(&mut acc.req_out, r);
                    }
                }
                self.open_direct(c, from, to, ev.ts);
            }
            Some("dec") | Some("blk") => {
                req.st = RState::Declined;
                // Silent: the sender keeps seeing "sent" (P3). Only the recipient's inbox changes.
                if let Some(acc) = self.accounts.get_mut(&to) {
                    remove_id(&mut acc.req_in, r);
                    if j.s("a") == Some("blk") {
                        acc.blocked.insert(from);
                    }
                }
            }
            Some("can") => {
                req.st = RState::Canceled;
                for a in [from, to] {
                    if let Some(acc) = self.accounts.get_mut(&a) {
                        remove_id(&mut acc.req_in, r);
                        remove_id(&mut acc.req_out, r);
                    }
                }
            }
            _ => return Outcome::Ignored("request answer"),
        }
        Outcome::Applied
    }

    /// Create (or reuse) the direct conversation between two accounts.
    fn open_direct(&mut self, c: u64, a: Aid, b: Aid, ts: u64) {
        if self.direct.contains_key(&pair(a, b)) {
            return;
        }
        let m = |x: Aid| Member { a: x, st: MState::Joined, role: Role::Member, since: ts, by: a, void: false };
        self.convs.insert(
            c,
            Conv {
                id: c,
                group: false,
                name: String::new(),
                created: ts,
                by: a,
                members: vec![m(a), m(b)],
                msgs: Vec::new(),
                last: None,
            },
        );
        self.direct.insert(pair(a, b), c);
        for x in [a, b] {
            if let Some(acc) = self.accounts.get_mut(&x) {
                push_unique(&mut acc.convs, c);
            }
        }
    }

    fn ev_conv(&mut self, ev: &Ev) -> Outcome {
        let j = &ev.j;
        let (Some(c), Some(by)) = (id64(j, "c"), aid(j, "by")) else { return Outcome::Ignored("conv") };
        if self.convs.contains_key(&c) {
            return Outcome::Ignored("conv exists");
        }
        let Some(acc) = self.accounts.get_mut(&by) else { return Outcome::Deferred };
        push_unique(&mut acc.convs, c);
        self.convs.insert(
            c,
            Conv {
                id: c,
                group: true,
                name: j.s("n").unwrap_or("").chars().take(80).collect(),
                created: ev.ts,
                by,
                members: vec![Member { a: by, st: MState::Joined, role: Role::Owner, since: ev.ts, by, void: false }],
                msgs: Vec::new(),
                last: None,
            },
        );
        Outcome::Applied
    }

    fn ev_member(&mut self, ev: &Ev) -> Outcome {
        let j = &ev.j;
        let (Some(c), Some(a)) = (id64(j, "c"), aid(j, "a")) else { return Outcome::Ignored("member") };
        let by = aid(j, "by").unwrap_or(a);
        let Some(conv) = self.convs.get_mut(&c) else { return Outcome::Deferred };
        let state = j.s("s").unwrap_or("");
        let visible = match state {
            "inv" => {
                let void = j.b("v").unwrap_or(false);
                match conv.member_mut(a) {
                    Some(m) if m.st == MState::Joined => return Outcome::Ignored("already joined"),
                    Some(m) => {
                        m.st = MState::Invited;
                        m.since = ev.ts;
                        m.by = by;
                        m.void = void;
                        m.role = Role::Member;
                    }
                    None => conv.members.push(Member {
                        a,
                        st: MState::Invited,
                        role: Role::Member,
                        since: ev.ts,
                        by,
                        void,
                    }),
                }
                Some(!void)
            }
            "join" => {
                let Some(m) = conv.member_mut(a) else { return Outcome::Ignored("join without invite") };
                if m.st != MState::Invited {
                    return Outcome::Ignored("join without invite");
                }
                m.st = MState::Joined;
                m.since = ev.ts;
                Some(true)
            }
            "dec" | "left" | "rem" => {
                let Some(m) = conv.member_mut(a) else { return Outcome::Ignored("not a member") };
                m.st = match state {
                    "dec" => MState::Declined,
                    "left" => MState::Left,
                    _ => MState::Removed,
                };
                // Ownership passes on so a group never ends up without an owner.
                if m.role == Role::Owner {
                    m.role = Role::Member;
                    if let Some(next) = conv
                        .members
                        .iter_mut()
                        .filter(|x| x.st == MState::Joined)
                        .min_by_key(|x| (x.role != Role::Admin, x.since))
                    {
                        next.role = Role::Owner;
                    }
                }
                Some(false)
            }
            "role" => {
                let Some(m) = conv.member_mut(a) else { return Outcome::Ignored("not a member") };
                if m.role != Role::Owner {
                    m.role = if j.s("r") == Some("a") { Role::Admin } else { Role::Member };
                }
                None
            }
            _ => return Outcome::Ignored("member state"),
        };
        if let Some(vis) = visible
            && let Some(acc) = self.accounts.get_mut(&a)
        {
            if vis {
                push_unique(&mut acc.convs, c);
            } else {
                remove_id(&mut acc.convs, c);
            }
        }
        Outcome::Applied
    }

    fn ev_msg(&mut self, ev: &Ev) -> Outcome {
        let j = &ev.j;
        let (Some(c), Some(from)) = (id64(j, "c"), aid(j, "f")) else { return Outcome::Ignored("msg") };
        if !self.accounts.contains_key(&from) {
            return Outcome::Deferred;
        }
        let r = MsgRef { ts: ev.ts, origin: ev.origin, off: ev.off, from };
        let key = self.msg_key(&r);
        let origins = self.origins.clone();
        let okey = |m: &MsgRef| (m.ts, origins.get(m.origin as usize).copied().unwrap_or(m.origin as u64), m.off);
        let atts: Vec<Arc<FileMeta>> = j.a("at").iter().filter_map(FileMeta::from_json).map(Arc::new).collect();
        let Some(conv) = self.convs.get_mut(&c) else { return Outcome::Deferred };
        // Insert in (time, relay, offset) order; nearly always an append.
        if conv.msgs.last().is_none_or(|l| okey(l) < key) {
            conv.msgs.push(r);
        } else {
            let pos = conv.msgs.partition_point(|m| okey(m) < key);
            if conv.msgs.get(pos) == Some(&r) {
                return Outcome::Ignored("duplicate message");
            }
            conv.msgs.insert(pos, r);
        }
        if conv.last.as_ref().is_none_or(|(l, _, _)| okey(l) < key) {
            conv.last = Some((r, snippet(j.s("x").unwrap_or("")), atts.len() as u32));
        }
        if atts.is_empty() {
            return Outcome::Applied;
        }
        // File refs: the sender keeps a copy; recipients per the recorded decisions.
        if let Some(acc) = self.accounts.get_mut(&from) {
            for m in &atts {
                if acc.uploads.remove(&m.b).is_some() {
                    unpend(&mut self.blobs, &m.b, &mut self.dead_blobs);
                }
            }
        }
        for m in &atts {
            set_file(&mut self.accounts, &mut self.blobs, &mut self.dead_blobs, from, m, FState::Active, b's', Some(c), ev.ts);
        }
        if let Some(Json::Obj(decisions)) = j.get("d") {
            for (who, bits) in decisions {
                let Some(a) = Aid::parse(who) else { continue };
                let bits = bits.as_str().unwrap_or("").as_bytes();
                for (i, m) in atts.iter().enumerate() {
                    let st = if bits.get(i) == Some(&b'1') { FState::Active } else { FState::Refused };
                    set_file(&mut self.accounts, &mut self.blobs, &mut self.dead_blobs, a, m, st, b'r', Some(c), ev.ts);
                }
            }
        }
        Outcome::Applied
    }

    fn ev_file(&mut self, ev: &Ev) -> Outcome {
        let j = &ev.j;
        let Some(a) = aid(j, "a") else { return Outcome::Ignored("file account") };
        if !self.accounts.contains_key(&a) {
            return Outcome::Deferred;
        }
        let op = j.s("o").unwrap_or("");
        if op == "add" {
            let Some(meta) = j.get("f").and_then(FileMeta::from_json).map(Arc::new) else {
                return Outcome::Ignored("file meta");
            };
            if let Some(acc) = self.accounts.get_mut(&a)
                && acc.uploads.remove(&meta.b).is_some()
            {
                unpend(&mut self.blobs, &meta.b, &mut self.dead_blobs);
            }
            set_file(&mut self.accounts, &mut self.blobs, &mut self.dead_blobs, a, &meta, FState::Active, b'd', None, ev.ts);
            return Outcome::Applied;
        }
        for h in j.a("b").iter().filter_map(|x| hash_of(x.as_str())) {
            let Some(meta) = self.accounts.get(&a).and_then(|acc| acc.files.get(&h)).map(|f| f.meta.clone()) else {
                continue;
            };
            match op {
                "rec" | "del" | "get" => {
                    let target = match op {
                        "rec" => FState::Recycled,
                        "del" => FState::Deleted,
                        _ => FState::Active,
                    };
                    let cur = self.accounts[&a].files[&h].state;
                    if op == "rec" && cur != FState::Active {
                        continue;
                    }
                    if op == "get" && !meta.blobs().all(|b| self.blobs.contains_key(&b)) {
                        continue; // nobody holds it any more
                    }
                    let dir = self.accounts[&a].files[&h].dir;
                    set_file(&mut self.accounts, &mut self.blobs, &mut self.dead_blobs, a, &meta, target, dir, None, ev.ts);
                }
                "pin" | "unpin" => {
                    let Some(acc) = self.accounts.get_mut(&a) else { continue };
                    let Some(f) = acc.files.get_mut(&h) else { continue };
                    let pin = op == "pin";
                    if f.pinned != pin {
                        f.pinned = pin;
                        if f.state == FState::Active {
                            let z = f.meta.charged();
                            if pin {
                                acc.pinned += z;
                            } else {
                                acc.pinned = acc.pinned.saturating_sub(z);
                            }
                        }
                    }
                }
                _ => return Outcome::Ignored("file op"),
            }
        }
        Outcome::Applied
    }
}

/// Drop one pending-upload hold on a blob.
fn unpend(blobs: &mut HashMap<Hash, Blob>, h: &Hash, dead: &mut Vec<Hash>) {
    if let Some(b) = blobs.get_mut(h) {
        b.pending = b.pending.saturating_sub(1);
        if b.refs == 0 && b.pending == 0 {
            blobs.remove(h);
            dead.push(*h);
        }
    }
}

fn blob_inc(blobs: &mut HashMap<Hash, Blob>, h: &Hash, z: u64, ts: u64) {
    blobs.entry(*h).or_insert(Blob { z, refs: 0, pending: 0, ts }).refs += 1;
}

fn blob_dec(blobs: &mut HashMap<Hash, Blob>, h: &Hash, dead: &mut Vec<Hash>) {
    if let Some(b) = blobs.get_mut(h) {
        b.refs = b.refs.saturating_sub(1);
        if b.refs == 0 && b.pending == 0 {
            blobs.remove(h);
            dead.push(*h);
        }
    }
}

/// Move one account's ref to `meta` into `state`, keeping usage, pinned bytes and blob refcounts
/// consistent. The single place where file accounting changes.
#[allow(clippy::too_many_arguments)]
fn set_file(
    accounts: &mut HashMap<Aid, Account>,
    blobs: &mut HashMap<Hash, Blob>,
    dead: &mut Vec<Hash>,
    a: Aid,
    meta: &Arc<FileMeta>,
    state: FState,
    dir: u8,
    conv: Option<u64>,
    ts: u64,
) {
    let Some(acc) = accounts.get_mut(&a) else { return };
    let z = meta.charged();
    let f = acc.files.entry(meta.b).or_insert_with(|| FileRef {
        meta: meta.clone(),
        state: FState::Deleted,
        pinned: false,
        added: ts,
        changed: ts,
        dir,
        convs: Vec::new(),
    });
    if let Some(c) = conv {
        push_unique(&mut f.convs, c);
    }
    let was = f.state;
    if was == state {
        return;
    }
    if was == FState::Active {
        acc.usage = acc.usage.saturating_sub(z);
        if f.pinned {
            acc.pinned = acc.pinned.saturating_sub(z);
        }
        for b in f.meta.blobs() {
            blob_dec(blobs, &b, dead);
        }
    }
    if state == FState::Active {
        acc.usage += z;
        if f.pinned {
            acc.pinned += z;
        }
        f.added = ts;
        let sizes = [meta.z, meta.t.map_or(0, |x| x.1), meta.p.map_or(0, |x| x.1)];
        for (b, sz) in meta.blobs().zip(sizes) {
            blob_inc(blobs, &b, sz, ts);
        }
        f.meta = meta.clone();
    }
    f.state = state;
    f.changed = ts;
}

fn hash_b64(s: Option<&str>) -> Option<Hash> {
    b64url_decode(s?)?.try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{b64url_encode, hex};
    use crate::ids::id64 as id_str;
    use crate::json::parse;

    struct T {
        s: State,
        off: u64,
    }

    impl T {
        fn new() -> T {
            T { s: State { origins: vec![7], ..Default::default() }, off: 0 }
        }
        fn ev(&mut self, ts: u64, kind: u16, json: &str) -> Outcome {
            self.off += 100;
            self.s.apply(Ev { origin: 0, off: self.off, ts, kind, j: parse(json).unwrap() })
        }
        fn account(&mut self, id: &str) {
            let salt = b64url_encode(&[1u8; 16]);
            let ver = b64url_encode(&[2u8; 32]);
            let r = self.ev(1, kind::ACCOUNT, &format!(r#"{{"a":"{id}","s":"{salt}","v":"{ver}"}}"#));
            assert_eq!(r, Outcome::Applied);
        }
        fn acc(&self, id: &str) -> &Account {
            &self.s.accounts[&Aid::parse(id).unwrap()]
        }
    }

    fn h(n: u8) -> String {
        hex(&[n; 32])
    }

    #[test]
    fn requests_are_silent_until_accepted() {
        let mut t = T::new();
        t.account("BBBB");
        t.account("CCCC");
        let r = id_str(1);
        t.ev(10, kind::REQUEST, &format!(r#"{{"r":"{r}","f":"BBBB","t":"CCCC","n":"hi","v":false}}"#));
        assert_eq!(t.acc("CCCC").req_in.len(), 1);
        assert_eq!(t.acc("BBBB").req_out.len(), 1);
        // Decline: the recipient's inbox empties, the sender still sees it pending.
        t.ev(11, kind::REQUEST_ANS, &format!(r#"{{"r":"{r}","a":"dec"}}"#));
        assert!(t.acc("CCCC").req_in.is_empty());
        assert_eq!(t.acc("BBBB").req_out.len(), 1);
        // A void request (unknown / requests off) never reaches anyone.
        let r2 = id_str(2);
        t.ev(12, kind::REQUEST, &format!(r#"{{"r":"{r2}","f":"BBBB","t":"DDDD","v":true}}"#));
        assert_eq!(t.acc("BBBB").req_out.len(), 2);
        // Accept creates the direct conversation for both.
        let r3 = id_str(3);
        let c = id_str(99);
        t.ev(13, kind::REQUEST, &format!(r#"{{"r":"{r3}","f":"CCCC","t":"BBBB","v":false}}"#));
        t.ev(14, kind::REQUEST_ANS, &format!(r#"{{"r":"{r3}","a":"acc","c":"{c}"}}"#));
        assert_eq!(t.acc("BBBB").convs, vec![99]);
        assert_eq!(t.acc("CCCC").convs, vec![99]);
        assert!(t.s.convs[&99].joined(Aid::parse("BBBB").unwrap()));
        assert!(t.acc("BBBB").req_in.is_empty());
    }

    #[test]
    fn group_membership_and_owner_handover() {
        let mut t = T::new();
        for id in ["BBBB", "CCCC", "DDDD"] {
            t.account(id);
        }
        let c = id_str(5);
        t.ev(20, kind::CONV, &format!(r#"{{"c":"{c}","n":"crew","by":"BBBB"}}"#));
        t.ev(21, kind::MEMBER, &format!(r#"{{"c":"{c}","a":"CCCC","s":"inv","by":"BBBB"}}"#));
        t.ev(21, kind::MEMBER, &format!(r#"{{"c":"{c}","a":"DDDD","s":"inv","by":"BBBB","v":true}}"#));
        assert_eq!(t.acc("CCCC").convs, vec![5]);
        assert!(t.acc("DDDD").convs.is_empty(), "void invites stay invisible");
        assert_eq!(t.s.convs[&5].seats(), 3);
        t.ev(22, kind::MEMBER, &format!(r#"{{"c":"{c}","a":"CCCC","s":"join"}}"#));
        assert!(t.s.convs[&5].joined(Aid::parse("CCCC").unwrap()));
        t.ev(23, kind::MEMBER, &format!(r#"{{"c":"{c}","a":"BBBB","s":"left"}}"#));
        let conv = &t.s.convs[&5];
        assert_eq!(conv.member(Aid::parse("CCCC").unwrap()).unwrap().role, Role::Owner);
        assert!(t.acc("BBBB").convs.is_empty());
    }

    #[test]
    fn deferred_events_wait_for_dependencies() {
        let mut t = T::new();
        let c = id_str(8);
        assert_eq!(t.ev(5, kind::MSG, &format!(r#"{{"c":"{c}","f":"BBBB","x":"early"}}"#)), Outcome::Deferred);
        t.account("BBBB");
        t.account("CCCC");
        assert_eq!(t.s.deferred.len(), 1);
        let r = id_str(1);
        t.ev(3, kind::REQUEST, &format!(r#"{{"r":"{r}","f":"BBBB","t":"CCCC","v":false}}"#));
        t.ev(4, kind::REQUEST_ANS, &format!(r#"{{"r":"{r}","a":"acc","c":"{c}"}}"#));
        assert!(t.s.deferred.is_empty());
        assert_eq!(t.s.convs[&8].msgs.len(), 1);
        assert_eq!(t.s.convs[&8].last.as_ref().unwrap().1, "early");
    }

    #[test]
    fn file_accounting_usage_pins_refcounts() {
        let mut t = T::new();
        t.account("BBBB");
        t.account("CCCC");
        let (r, c) = (id_str(1), id_str(9));
        t.ev(1, kind::REQUEST, &format!(r#"{{"r":"{r}","f":"BBBB","t":"CCCC","v":false}}"#));
        t.ev(2, kind::REQUEST_ANS, &format!(r#"{{"r":"{r}","a":"acc","c":"{c}"}}"#));
        let att = format!(r#"[{{"b":"{}","n":"a.jpg","m":"image/jpeg","z":1000,"t":"{}","tz":50}},{{"b":"{}","n":"b.bin","z":500}}]"#, h(1), h(2), h(3));
        t.ev(3, kind::MSG, &format!(r#"{{"c":"{c}","f":"BBBB","x":"files","at":{att},"d":{{"CCCC":"10"}}}}"#));
        assert_eq!(t.acc("BBBB").usage, 1550);
        assert_eq!(t.acc("CCCC").usage, 1050);
        let cf = &t.acc("CCCC").files;
        assert_eq!(cf[&[3u8; 32]].state, FState::Refused);
        assert_eq!(t.s.blobs[&[1u8; 32]].refs, 2);
        assert_eq!(t.s.blobs[&[3u8; 32]].refs, 1);
        // Pin, recycle (pinned is still recycled when asked explicitly), receive later.
        t.ev(4, kind::FILE, &format!(r#"{{"a":"CCCC","o":"pin","b":["{}"]}}"#, h(1)));
        assert_eq!(t.acc("CCCC").pinned, 1050);
        t.ev(5, kind::FILE, &format!(r#"{{"a":"CCCC","o":"get","b":["{}"]}}"#, h(3)));
        assert_eq!(t.acc("CCCC").usage, 1550);
        t.ev(6, kind::FILE, &format!(r#"{{"a":"BBBB","o":"del","b":["{}","{}"]}}"#, h(1), h(3)));
        assert_eq!(t.acc("BBBB").usage, 0);
        assert_eq!(t.s.blobs[&[1u8; 32]].refs, 1);
        assert!(t.s.dead_blobs.is_empty());
        t.ev(7, kind::FILE, &format!(r#"{{"a":"CCCC","o":"rec","b":["{}","{}"]}}"#, h(1), h(3)));
        assert_eq!(t.acc("CCCC").usage, 0);
        assert_eq!(t.acc("CCCC").pinned, 0);
        let mut dead = t.s.dead_blobs.clone();
        dead.sort();
        assert_eq!(dead, vec![[1u8; 32], [2u8; 32], [3u8; 32]]);
        // Gone for everyone: "get again" is impossible now.
        t.ev(8, kind::FILE, &format!(r#"{{"a":"CCCC","o":"get","b":["{}"]}}"#, h(3)));
        assert_eq!(t.acc("CCCC").files[&[3u8; 32]].state, FState::Recycled);
    }

    #[test]
    fn settings_last_writer_wins() {
        let mut t = T::new();
        t.ev(10, kind::SETTING, r#"{"k":"default_quota","v":5}"#);
        t.ev(5, kind::SETTING, r#"{"k":"default_quota","v":7}"#);
        assert_eq!(t.s.settings.default_quota, 5);
        t.ev(11, kind::SETTING, r#"{"k":"signup","v":"open"}"#);
        assert_eq!(t.s.settings.signup, Signup::Open);
        t.ev(12, kind::SETTING, r#"{"k":"flex_pct","v":101}"#);
        assert_eq!(t.s.settings.flex_pct, 10);
    }

    #[test]
    fn messages_stay_ordered_across_relays() {
        let mut t = T::new();
        t.s.origins = vec![7, 3];
        t.account("BBBB");
        t.account("CCCC");
        let (r, c) = (id_str(1), id_str(4));
        t.ev(1, kind::REQUEST, &format!(r#"{{"r":"{r}","f":"BBBB","t":"CCCC","v":false}}"#));
        t.ev(2, kind::REQUEST_ANS, &format!(r#"{{"r":"{r}","a":"acc","c":"{c}"}}"#));
        t.ev(50, kind::MSG, &format!(r#"{{"c":"{c}","f":"BBBB","x":"late"}}"#));
        // An earlier message replicated from relay 3 arrives afterwards.
        t.off += 1;
        t.s.apply(Ev { origin: 1, off: 9, ts: 40, kind: kind::MSG, j: parse(&format!(r#"{{"c":"{c}","f":"CCCC","x":"early"}}"#)).unwrap() });
        let msgs = &t.s.convs[&4].msgs;
        assert_eq!(msgs.iter().map(|m| m.ts).collect::<Vec<_>>(), vec![40, 50]);
        assert_eq!(t.s.convs[&4].last.as_ref().unwrap().1, "late");
    }
}
