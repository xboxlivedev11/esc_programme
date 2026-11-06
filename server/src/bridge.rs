//! The relay bridge: state + logs + blob store + live clients, and every rule decision.
//!
//! A command takes the state lock, validates, decides (privacy rules P1-P7, quota rules R1-R9),
//! appends events to this relay's log and applies them, then releases the lock, waits for the log
//! to be durable (group commit) and only then pushes to clients. Nobody ever sees an event that a
//! crash could take back.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Read};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, RwLock};
use std::time::Duration;

use std::thread;

use crate::courier::{self, Mail};
use crate::hold::{Put, Store, disk_free};
use crate::crypto::{b64url_decode, b64url_encode, ct_eq, hex, hmac_sha256, hmac_sha256_parts, random_bytes, sha256, unhex};
use crate::ids::{Aid, id64, new_id64, now_ms, parse_id64};
use crate::json::{self, Js, Json};
use crate::logbook::{self, Log};
use crate::model::{
    Account, Conv, Ev, FState, FileMeta, Hash, MState, MsgRef, Outcome, Recovery, Role, SETTING_KEYS, Signup, State, kind,
};

pub const MAX_TEXT: usize = 16_000;
pub const MAX_ATTACH: usize = 32;
pub const GROUP_MAX: usize = 128;
const HOUR_MS: u64 = 3_600_000;
const DAY_MS: u64 = 86_400_000;

/// A user-facing error: HTTP-ish status + message.
#[derive(Debug, Clone)]
pub struct Fail(pub u16, pub String);
pub type R<T> = Result<T, Fail>;

fn bad(m: &str) -> Fail {
    Fail(400, m.to_string())
}
fn forbidden(m: &str) -> Fail {
    Fail(403, m.to_string())
}
fn ioerr(e: io::Error) -> Fail {
    eprintln!("esc: io error: {e}");
    Fail(500, "storage error".into())
}

/// Lock ignoring poisoning: a panic elsewhere must not take the relay down.
pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ------------------------------------------------------------------------------------------------
// The Switchboard: live clients
// ------------------------------------------------------------------------------------------------

/// One connected WebSocket. Frames go through a bounded queue to a dedicated writer thread, so a
/// slow client can never stall the relay; if its queue fills up it is dropped.
/// What the writer thread of a client sends next.
pub enum Out {
    Text(Arc<str>),
    Pong(Vec<u8>),
    Close,
}

pub struct Client {
    pub id: u64,
    pub aid: Aid,
    pub sess: Hash,
    tx: SyncSender<Out>,
    pub alive: AtomicBool,
}

impl Client {
    pub fn new(aid: Aid, sess: Hash) -> (Arc<Client>, Receiver<Out>) {
        let (tx, rx) = sync_channel(4096);
        (Arc::new(Client { id: new_id64(), aid, sess, tx, alive: AtomicBool::new(true) }), rx)
    }
    /// Queue a text frame; false when the client is gone or hopelessly behind.
    pub fn send(&self, msg: &Arc<str>) -> bool {
        if !self.alive.load(Ordering::Relaxed) {
            return false;
        }
        self.queue(Out::Text(msg.clone()))
    }
    /// Queue any frame (pongs and closes jump no queue: order is kept).
    pub fn queue(&self, o: Out) -> bool {
        match self.tx.try_send(o) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                self.alive.store(false, Ordering::Relaxed);
                false
            }
        }
    }
}

#[derive(Default)]
pub struct Switchboard {
    map: Mutex<HashMap<Aid, Vec<Arc<Client>>>>,
}

impl Switchboard {
    pub fn add(&self, c: Arc<Client>) {
        lock(&self.map).entry(c.aid).or_default().push(c);
    }
    pub fn remove(&self, c: &Client) {
        let mut m = lock(&self.map);
        if let Some(v) = m.get_mut(&c.aid) {
            v.retain(|x| x.id != c.id);
            if v.is_empty() {
                m.remove(&c.aid);
            }
        }
    }
    pub fn send(&self, to: Aid, msg: &Arc<str>) {
        let targets: Vec<Arc<Client>> = lock(&self.map).get(&to).cloned().unwrap_or_default();
        for c in targets {
            if !c.send(msg) {
                self.remove(&c);
            }
        }
    }
    /// Disconnect every client of a session (logout, password change, disabled account).
    pub fn kick(&self, f: impl Fn(&Client) -> bool) {
        let all: Vec<Arc<Client>> = lock(&self.map).values().flatten().filter(|c| f(c)).cloned().collect();
        for c in all {
            c.alive.store(false, Ordering::Relaxed);
            let _ = c.tx.try_send(Out::Close);
            self.remove(&c);
        }
    }
    pub fn online(&self) -> usize {
        lock(&self.map).values().map(Vec::len).sum()
    }
}

// ------------------------------------------------------------------------------------------------
// Bridge
// ------------------------------------------------------------------------------------------------

pub struct Bridge {
    pub data: PathBuf,
    pub me: u64,
    pub name: String,
    pub listen: String,
    pub url: Mutex<String>,
    pepper: Hash,
    pub ring_key: Hash,
    pub st: Mutex<State>,
    pub logs: RwLock<Vec<Arc<Log>>>,
    pub store: Store,
    pub board: Switchboard,
    clock: Mutex<u64>,
    limits: Mutex<HashMap<(u8, String), (f64, u64)>>,
    nonces: Mutex<HashMap<(Aid, u64), (String, u64)>>,
    /// Bumped whenever any log grows; the Ring's pull streams wait on it.
    grew: (Mutex<u64>, Condvar),
    /// One-time Sparks (relay join tokens) -> expiry. In memory: a restart just means a new Spark.
    sparks: Mutex<HashMap<String, u64>>,
    /// URLs to pull from before this Star knows the Ring (written by `esc join`).
    pub seeds: Vec<String>,
}

/// Names for Stars (relays) in the Ring, in the order new Stars get them.
pub const STARS: &[&str] = &[
    "Polaris", "Vega", "Sirius", "Altair", "Deneb", "Rigel", "Capella", "Arcturus", "Antares", "Aldebaran", "Spica",
    "Procyon", "Canopus", "Achernar", "Fomalhaut", "Regulus", "Bellatrix", "Castor", "Pollux", "Mira", "Mizar", "Alnilam",
    "Shaula", "Hadar", "Acrux", "Mimosa", "Alhena", "Elnath", "Menkar", "Nunki",
];

/// A transaction: state lock + events written + pushes queued until durable.
pub struct Tx<'a> {
    bridge: &'a Bridge,
    pub st: MutexGuard<'a, State>,
    end: u64,
    out: Vec<(Aid, Arc<str>)>,
    kicked: Vec<Hash>,
}

impl Tx<'_> {
    /// Append an event to this relay's log and apply it.
    pub fn emit(&mut self, kind: u16, js: Js) -> R<(u64, u64)> {
        let text = js.finish();
        let j = json::parse(&text).map_err(|e| Fail(500, e))?;
        let ts = self.bridge.tick();
        let rec = logbook::encode(ts, kind, text.as_bytes());
        let mine = self.bridge.logs.read().unwrap_or_else(|e| e.into_inner())[0].clone();
        let off = mine.append(&rec).map_err(ioerr)?;
        self.end = off + rec.len() as u64;
        let out = self.st.apply(Ev { origin: 0, off, ts, kind, j });
        if out != Outcome::Applied {
            eprintln!("esc: local event kind {kind} not applied: {out:?}");
        }
        Ok((off, ts))
    }
    pub fn push(&mut self, to: Aid, msg: &str) {
        self.out.push((to, Arc::from(msg)));
    }
    pub fn push_many(&mut self, to: impl IntoIterator<Item = Aid>, msg: &str) {
        let m: Arc<str> = Arc::from(msg);
        for a in to {
            self.out.push((a, m.clone()));
        }
    }
    pub fn kick_session(&mut self, h: Hash) {
        self.kicked.push(h);
    }
    /// Release the lock, make the log durable, then deliver pushes and delete dead blobs.
    pub fn commit(mut self) -> R<()> {
        let dead = std::mem::take(&mut self.st.dead_blobs);
        let Tx { bridge, st, end, out, kicked } = self;
        drop(st);
        if end > 0 {
            let mine = bridge.logs.read().unwrap_or_else(|e| e.into_inner())[0].clone();
            mine.sync(end).map_err(ioerr)?;
        }
        for (to, msg) in out {
            bridge.board.send(to, &msg);
        }
        for h in kicked {
            bridge.board.kick(|c| c.sess == h);
        }
        for h in dead {
            let _ = bridge.store.remove(&h);
        }
        if end > 0 {
            bridge.notify_grew();
        }
        Ok(())
    }
}

fn read_kv(p: &Path) -> io::Result<HashMap<String, String>> {
    let text = fs::read_to_string(p)?;
    Ok(text
        .lines()
        .filter_map(|l| {
            let l = l.trim();
            if l.starts_with('#') {
                return None;
            }
            let (k, v) = l.split_once('=')?;
            Some((k.trim().to_string(), v.trim().to_string()))
        })
        .collect())
}

fn hash32(s: &str) -> Option<Hash> {
    unhex(s)?.try_into().ok()
}

pub struct Opened {
    pub bridge: Arc<Bridge>,
    /// One-time admin sign-up code, when no admin account exists yet.
    pub admin_invite: Option<String>,
    pub replayed: usize,
}

impl Bridge {
    /// Open the data folder, creating a new relay (genesis) if it is empty.
    pub fn open(data: &Path, listen_override: Option<&str>) -> io::Result<Opened> {
        fs::create_dir_all(data.join("log"))?;
        let ini = data.join("relay.ini");
        let secret_path = data.join("secret");
        let genesis = !ini.exists();
        if genesis {
            let mut p = [0u8; 32];
            let mut r = [0u8; 32];
            random_bytes(&mut p);
            random_bytes(&mut r);
            fs::write(&secret_path, format!("pepper = {}\nring = {}\n", hex(&p), hex(&r)))?;
            let id = new_id64();
            fs::write(
                &ini,
                format!(
                    "# ESC relay (a Star of the Ring)\nid = {}\nname = Polaris\nlisten = {}\nurl =\n",
                    id64(id),
                    listen_override.unwrap_or("127.0.0.1:7878")
                ),
            )?;
        }
        let kv = read_kv(&ini)?;
        let sec = read_kv(&secret_path)?;
        let bad = |m: &str| io::Error::new(io::ErrorKind::InvalidData, m.to_string());
        let me = kv.get("id").and_then(|s| parse_id64(s)).ok_or_else(|| bad("relay.ini: bad id"))?;
        let pepper = sec.get("pepper").and_then(|s| hash32(s)).ok_or_else(|| bad("secret: bad pepper"))?;
        let ring_key = sec.get("ring").and_then(|s| hash32(s)).ok_or_else(|| bad("secret: bad ring"))?;
        let listen = listen_override.map(str::to_string).or_else(|| kv.get("listen").cloned()).unwrap_or("127.0.0.1:7878".into());

        // Logs: mine first (origin 0), then every other origin found on disk.
        let mut names: Vec<(u64, PathBuf)> = Vec::new();
        for e in fs::read_dir(data.join("log"))? {
            let p = e?.path();
            if p.extension().is_some_and(|x| x == "log")
                && let Some(id) = p.file_stem().and_then(|s| s.to_str()).and_then(parse_id64)
                && id != me
            {
                names.push((id, p));
            }
        }
        names.sort();
        names.insert(0, (me, data.join("log").join(format!("{}.log", id64(me)))));

        let mut st = State { origins: names.iter().map(|(id, _)| *id).collect(), ..Default::default() };
        let mut logs = Vec::new();
        let mut replayed = 0usize;
        let mut last_ts = 0u64;
        for (idx, (_, path)) in names.iter().enumerate() {
            let (lg, dropped) = Log::open(path, |off, ts, kind, json| {
                replayed += 1;
                if idx == 0 {
                    last_ts = last_ts.max(ts);
                }
                match json::parse_bytes(json) {
                    Ok(j) => {
                        st.apply(Ev { origin: idx as u16, off, ts, kind, j });
                    }
                    Err(e) => eprintln!("esc: bad record at {}:{off}: {e}", path.display()),
                }
            })?;
            if dropped > 0 {
                eprintln!("esc: {}: dropped {dropped} bytes of torn tail", path.display());
            }
            logs.push(Arc::new(lg));
        }
        st.dead_blobs.clear();

        let store = Store::open(data)?;
        let bridge = Arc::new(Bridge {
            data: data.to_path_buf(),
            me,
            name: kv.get("name").cloned().unwrap_or("relay".into()),
            listen,
            url: Mutex::new(kv.get("url").cloned().unwrap_or_default()),
            pepper,
            ring_key,
            st: Mutex::new(st),
            logs: RwLock::new(logs),
            store,
            board: Switchboard::default(),
            clock: Mutex::new(last_ts),
            limits: Mutex::new(HashMap::new()),
            nonces: Mutex::new(HashMap::new()),
            grew: (Mutex::new(0), Condvar::new()),
            sparks: Mutex::new(HashMap::new()),
            seeds: kv
                .get("seeds")
                .map(|s| s.split(',').map(|x| x.trim().trim_end_matches('/').to_string()).filter(|x| !x.is_empty()).collect())
                .unwrap_or_default(),
        });

        if genesis {
            let mut tx = bridge.begin();
            let mut j = Js::new();
            j.obj().ks("r", &id64(me)).ks("n", &bridge.name).ks("u", "").end_obj();
            tx.emit(kind::RELAY, j).map_err(|e| bad(&e.1))?;
            tx.commit().map_err(|e| bad(&e.1))?;
        }
        let admin_invite = bridge.ensure_admin_invite().map_err(|e| bad(&e.1))?;
        Ok(Opened { bridge, admin_invite, replayed })
    }

    pub fn begin(&self) -> Tx<'_> {
        Tx { bridge: self, st: lock(&self.st), end: 0, out: Vec::new(), kicked: Vec::new() }
    }

    /// Strictly increasing milliseconds for this relay's events.
    fn tick(&self) -> u64 {
        let mut c = lock(&self.clock);
        let t = now_ms().max(*c + 1);
        *c = t;
        t
    }

    /// While no admin exists, keep exactly one unexpired admin invite and return its code.
    fn ensure_admin_invite(&self) -> R<Option<String>> {
        let mut tx = self.begin();
        if tx.st.accounts.values().any(|a| a.admin) {
            return Ok(None);
        }
        let now = now_ms();
        if let Some(inv) = tx.st.invites.values().find(|i| i.admin && i.uses > 0 && i.exp > now) {
            return Ok(Some(inv.code.clone()));
        }
        let code = new_code(10);
        let mut j = Js::new();
        j.obj().ks("c", &code).ku("u", 1).kb("adm", true).end_obj();
        tx.emit(kind::INVITE, j)?;
        tx.commit()?;
        Ok(Some(code))
    }

    // --------------------------------------------------------------------------------------------
    // The Bouncer: rate limiting (in memory, per relay)
    // --------------------------------------------------------------------------------------------

    /// Token bucket: `per_min` refill, `burst` capacity. False = over the limit.
    pub fn bouncer(&self, bucket: u8, key: &str, per_min: f64, burst: f64) -> bool {
        let now = now_ms();
        let mut m = lock(&self.limits);
        let e = m.entry((bucket, key.to_string())).or_insert((burst, now));
        let refill = (now.saturating_sub(e.1)) as f64 / 60_000.0 * per_min;
        e.0 = (e.0 + refill).min(burst);
        e.1 = now;
        if e.0 >= 1.0 {
            e.0 -= 1.0;
            true
        } else {
            false
        }
    }

    // --------------------------------------------------------------------------------------------
    // Accounts & sessions
    // --------------------------------------------------------------------------------------------

    fn verifier(&self, key: &[u8]) -> Hash {
        hmac_sha256(&self.pepper, key)
    }

    /// The salt for an ID. Unknown IDs get a stable fake salt, so this never reveals existence.
    pub fn pre(&self, id: &str) -> String {
        let st = lock(&self.st);
        if let Some(acc) = Aid::parse(id).and_then(|a| st.accounts.get(&a)) {
            return b64url_encode(&acc.salt);
        }
        let norm = Aid::parse(id).map(|a| a.to_string()).unwrap_or_else(|| id.to_uppercase());
        let fake = hmac_sha256_parts(&self.pepper, &[b"fake-salt:", norm.as_bytes()]);
        b64url_encode(&fake[..16])
    }

    fn new_session(&self, tx: &mut Tx, a: Aid, label: &str) -> R<String> {
        let mut t = [0u8; 32];
        random_bytes(&mut t);
        let mut j = Js::new();
        j.obj().ks("h", &b64url_encode(&sha256(&t))).ks("a", &a.to_string()).ks("l", label).end_obj();
        tx.emit(kind::SESSION, j)?;
        Ok(b64url_encode(&t))
    }

    pub fn signup(&self, inv: &str, salt: &[u8], key: &[u8], name: &str, label: &str) -> R<(Aid, String)> {
        let salt: [u8; 16] = salt.try_into().map_err(|_| bad("bad salt"))?;
        if key.len() != 32 {
            return Err(bad("bad key"));
        }
        let name: String = name.trim().chars().take(40).collect();
        let mut tx = self.begin();
        let now = now_ms();
        let invite = tx.st.invites.get(inv.trim()).filter(|i| i.uses > 0 && i.exp > now).cloned();
        let admin = invite.as_ref().is_some_and(|i| i.admin);
        match tx.st.settings.signup {
            Signup::Open => {}
            Signup::Invite if invite.is_some() => {}
            Signup::Closed if admin => {}
            Signup::Invite => return Err(forbidden("That invite code is not valid.")),
            Signup::Closed => return Err(forbidden("Sign-up is closed.")),
        }
        let id = loop {
            let id = Aid::random(tx.st.short_ids);
            if !tx.st.accounts.contains_key(&id) {
                break id;
            }
        };
        let mut j = Js::new();
        j.obj()
            .ks("a", &id.to_string())
            .ks("s", &b64url_encode(&salt))
            .ks("v", &b64url_encode(&self.verifier(key)))
            .kb("adm", admin)
            .ks("n", &name);
        if let Some(i) = &invite {
            j.ks("inv", &i.code);
        }
        j.end_obj();
        tx.emit(kind::ACCOUNT, j)?;
        let token = self.new_session(&mut tx, id, label)?;
        tx.commit()?;
        Ok((id, token))
    }

    pub fn login(&self, id: &str, key: &[u8], label: &str, ip: IpAddr) -> R<(Aid, String)> {
        if !self.bouncer(1, &ip.to_string(), 10.0, 20.0) {
            return Err(Fail(429, "Too many attempts. Wait a minute.".into()));
        }
        let wrong = || Fail(401, "Wrong ID or password.".into());
        let Some(a) = Aid::parse(id) else { return Err(wrong()) };
        if !self.bouncer(2, &a.to_string(), 5.0, 10.0) {
            return Err(Fail(429, "Too many attempts for this ID. Wait a few minutes.".into()));
        }
        let v = self.verifier(key);
        let mut tx = self.begin();
        let Some(acc) = tx.st.accounts.get(&a) else { return Err(wrong()) };
        if !ct_eq(&v, &acc.ver) {
            return Err(wrong());
        }
        if acc.disabled {
            return Err(forbidden("This account is disabled."));
        }
        let token = self.new_session(&mut tx, a, label)?;
        tx.commit()?;
        Ok((a, token))
    }

    /// Session token (from the cookie) -> (account, session hash).
    pub fn auth(&self, token: &str) -> Option<(Aid, Hash)> {
        let t = b64url_decode(token)?;
        if t.len() != 32 {
            return None;
        }
        let h = sha256(&t);
        let st = lock(&self.st);
        let s = st.sessions.get(&h)?;
        let acc = st.accounts.get(&s.a)?;
        (!acc.disabled).then_some((s.a, h))
    }

    pub fn logout(&self, h: &Hash) -> R<()> {
        let mut tx = self.begin();
        let mut j = Js::new();
        j.obj().ks("h", &b64url_encode(h)).end_obj();
        tx.emit(kind::SESSION_END, j)?;
        tx.kick_session(*h);
        tx.commit()
    }

    // --------------------------------------------------------------------------------------------
    // Views (JSON for the web app)
    // --------------------------------------------------------------------------------------------

    fn me_json(st: &State, a: &Account, js: &mut Js) {
        js.obj()
            .ks("id", &a.id.to_string())
            .ks("n", &a.name)
            .kb("adm", a.admin)
            .kb("acc", a.accept)
            .kb("ar", a.auto_recycle)
            .ku("q", st.settings.quota(a))
            .ku("ceil", st.settings.ceiling(a))
            .ku("u", a.usage)
            .ku("pin", a.pinned)
            .ku("fx", st.settings.flex_pct);
        js.key("cl");
        match a.cleanup_at {
            Some(t) => js.uint(t),
            None => js.null(),
        };
        js.ks(
            "rec",
            match a.recovery {
                Recovery::None => "none",
                Recovery::Email(_) => "email",
                Recovery::Code(_) => "code",
            },
        );
        if let Recovery::Email(e) = &a.recovery {
            js.ks("em", &mask_email(e));
        }
        js.key("blk").arr();
        for b in &a.blocked {
            js.str(&b.to_string());
        }
        js.end_arr().key("nick").obj();
        for (k, v) in &a.nicks {
            js.ks(&k.to_string(), v);
        }
        js.end_obj().end_obj();
    }

    fn conv_json(st: &State, c: &Conv, viewer: Aid, js: &mut Js) {
        let me = c.member(viewer);
        let joined = me.is_some_and(|m| m.st == MState::Joined);
        js.obj().ks("c", &id64(c.id)).kb("g", c.group).ks("n", &c.name);
        if !joined {
            // Invited: only what the inviter chose to show (P5).
            let by = me.map(|m| m.by).unwrap_or(c.by);
            js.ks("me", "i").ks("by", &by.to_string());
            js.ks("byn", st.accounts.get(&by).map_or("", |a| a.name.as_str()));
            js.ku("cnt", c.joined_ids().count() as u64).end_obj();
            return;
        }
        js.ks("me", "j").key("m").arr();
        for m in &c.members {
            let shown = match m.st {
                MState::Joined => "j",
                MState::Invited => "i",
                MState::Declined => "i", // decline is silent to others
                MState::Left | MState::Removed => continue,
            };
            js.obj().ks("a", &m.a.to_string()).ks("s", shown).ks("r", m.role.code());
            if m.st == MState::Joined
                && let Some(acc) = st.accounts.get(&m.a)
            {
                js.ks("n", &acc.name);
            }
            js.end_obj();
        }
        js.end_arr();
        let since = me.map_or(0, |m| m.since);
        if let Some((r, text, n)) = &c.last
            && r.ts >= since
        {
            js.key("l").obj().ku("t", r.ts).ks("f", &r.from.to_string()).ks("x", text).ku("k", *n as u64).end_obj();
        }
        let read = st.accounts.get(&viewer).and_then(|a| a.reads.get(&c.id)).copied().unwrap_or(0).max(since);
        let start = c.msgs.partition_point(|m| m.ts <= read);
        let unread = c.msgs[start..].iter().filter(|m| m.from != viewer).count();
        js.ku("u", unread as u64).ku("rd", read).ku("t", c.last.as_ref().map_or(c.created, |l| l.0.ts)).end_obj();
    }

    fn req_in_json(st: &State, r: &crate::model::Req, js: &mut Js) {
        js.obj().ks("r", &id64(r.id)).ks("f", &r.from.to_string()).ks("x", &r.note).ku("t", r.ts);
        js.ks("n", st.accounts.get(&r.from).map_or("", |a| a.name.as_str())).end_obj();
    }

    fn req_out_json(r: &crate::model::Req, js: &mut Js) {
        js.obj().ks("r", &id64(r.id)).ks("to", &r.to.to_string()).ks("x", &r.note).ku("t", r.ts).end_obj();
    }

    /// One message as `viewer` sees it (attachment states are per viewer).
    fn msg_json(&self, st: &State, m: &MsgRef, conv: u64, viewer: Aid, js: &mut Js) -> R<()> {
        let lg = self.logs.read().unwrap_or_else(|e| e.into_inner()).get(m.origin as usize).cloned();
        let Some(lg) = lg else { return Err(Fail(500, "missing log".into())) };
        let (_, _, raw) = lg.read(m.off).map_err(ioerr)?;
        let j = json::parse_bytes(&raw).map_err(|e| Fail(500, e))?;
        let origin = st.origins.get(m.origin as usize).copied().unwrap_or(0);
        js.obj()
            .ks("id", &format!("{}-{}", id64(origin), m.off))
            .ks("c", &id64(conv))
            .ks("f", &m.from.to_string())
            .ku("t", m.ts)
            .ks("x", j.s("x").unwrap_or(""));
        let atts = j.a("at");
        if !atts.is_empty() {
            let files = st.accounts.get(&viewer).map(|a| &a.files);
            js.key("a").arr();
            for a in atts {
                let Some(meta) = FileMeta::from_json(a) else { continue };
                let state = files.and_then(|f| f.get(&meta.b)).map_or("gone", |f| f.state.code());
                js.obj()
                    .ks("b", &hex(&meta.b))
                    .ks("n", &meta.name)
                    .ks("m", &meta.mime)
                    .ku("z", meta.z)
                    .ku("cz", meta.charged())
                    .kb("th", meta.t.is_some())
                    .kb("pv", meta.p.is_some())
                    .ks("s", state);
                if meta.w > 0 {
                    js.ku("w", meta.w as u64).ku("h", meta.h as u64);
                }
                js.end_obj();
            }
            js.end_arr();
        }
        js.end_obj();
        Ok(())
    }

    /// Everything the app needs after connecting.
    pub fn hello(&self, a: Aid) -> R<String> {
        let st = lock(&self.st);
        let acc = st.accounts.get(&a).ok_or_else(|| Fail(401, "no account".into()))?;
        let mut js = Js::with_capacity(4096);
        js.obj().key("me");
        Self::me_json(&st, acc, &mut js);
        js.key("convs").arr();
        for c in acc.convs.iter().filter_map(|c| st.convs.get(c)) {
            Self::conv_json(&st, c, a, &mut js);
        }
        js.end_arr().key("rin").arr();
        for r in acc.req_in.iter().filter_map(|r| st.reqs.get(r)) {
            Self::req_in_json(&st, r, &mut js);
        }
        js.end_arr().key("rout").arr();
        for r in acc.req_out.iter().filter_map(|r| st.reqs.get(r)) {
            Self::req_out_json(r, &mut js);
        }
        js.end_arr().key("relays").arr();
        for r in st.relays.values() {
            js.obj().ks("n", &r.name).ks("u", &r.url).end_obj();
        }
        js.end_arr();
        js.ks("signup", st.settings.signup.as_str())
            .ku("mi", st.settings.member_invites)
            .ku("now", now_ms())
            .ku("gmax", GROUP_MAX as u64)
            .ku("tmax", MAX_TEXT as u64)
            .end_obj();
        Ok(js.finish())
    }

    fn me_push(st: &State, a: Aid) -> Option<String> {
        let acc = st.accounts.get(&a)?;
        let mut js = Js::new();
        js.obj().ks("p", "me").key("me");
        Self::me_json(st, acc, &mut js);
        js.end_obj();
        Some(js.finish())
    }

    fn conv_push(st: &State, c: u64, viewer: Aid) -> Option<String> {
        let conv = st.convs.get(&c)?;
        let mut js = Js::new();
        js.obj().ks("p", "conv").key("c");
        Self::conv_json(st, conv, viewer, &mut js);
        js.end_obj();
        Some(js.finish())
    }

    /// Push the current view of a conversation to each listed account.
    fn push_conv(tx: &mut Tx, c: u64, to: &[Aid]) {
        for &a in to {
            if let Some(m) = Self::conv_push(&tx.st, c, a) {
                tx.push(a, &m);
            }
        }
    }

    fn push_me(tx: &mut Tx, a: Aid) {
        if let Some(m) = Self::me_push(&tx.st, a) {
            tx.push(a, &m);
        }
    }

    // --------------------------------------------------------------------------------------------
    // Profile
    // --------------------------------------------------------------------------------------------

    pub fn me_set(&self, a: Aid, j: &Json) -> R<String> {
        let mut tx = self.begin();
        let acc = tx.st.accounts.get(&a).cloned().ok_or_else(|| Fail(401, "no account".into()))?;
        let mut e = Js::new();
        e.obj().ks("a", &a.to_string());
        let mut any = false;
        if let Some(n) = j.s("n") {
            let n: String = n.trim().chars().take(40).collect();
            e.ks("n", &n);
            any = true;
        }
        if let Some(v) = j.b("acc") {
            e.kb("acc", v);
            any = true;
        }
        if let Some(v) = j.b("ar")
            && v != acc.auto_recycle
        {
            e.kb("ar", v);
            any = true;
            if v {
                // R5: turning auto-recycle on while over quota schedules a cleanup.
                let q = tx.st.settings.quota(&acc);
                if acc.usage > q && acc.recyclable() > 0 {
                    let at = now_ms() + tx.st.settings.cleanup_h * HOUR_MS;
                    e.ku("cl", at).ks("clb", &id64(self.me));
                }
            } else if acc.cleanup_at.is_some() {
                // R6: off cancels a scheduled cleanup.
                e.key("cl").null();
            }
        }
        e.end_obj();
        if any {
            tx.emit(kind::ACCOUNT_SET, e)?;
        }
        Self::push_me(&mut tx, a);
        if j.s("n").is_some() {
            // Everyone who shares a conversation sees the new name.
            let mut peers: Vec<Aid> = Vec::new();
            let mut convs: Vec<u64> = Vec::new();
            for c in tx.st.accounts[&a].convs.iter().filter_map(|c| tx.st.convs.get(c)) {
                if c.joined(a) {
                    convs.push(c.id);
                    peers.extend(c.joined_ids());
                }
            }
            peers.sort();
            peers.dedup();
            for p in peers {
                for &c in &convs {
                    if tx.st.convs.get(&c).is_some_and(|cv| cv.joined(p))
                        && let Some(m) = Self::conv_push(&tx.st, c, p)
                    {
                        tx.push(p, &m);
                    }
                }
            }
        }
        tx.commit()?;
        Ok("{}".into())
    }

    pub fn block(&self, a: Aid, target: &str, on: bool) -> R<String> {
        let t = Aid::parse(target).ok_or_else(|| bad("Not a valid ID."))?;
        let mut tx = self.begin();
        let mut j = Js::new();
        j.obj().ks("a", &a.to_string()).ks("t", &t.to_string()).kb("on", on).end_obj();
        tx.emit(kind::BLOCK, j)?;
        Self::push_me(&mut tx, a);
        tx.commit()?;
        Ok("{}".into())
    }

    pub fn nick(&self, a: Aid, target: &str, name: &str) -> R<String> {
        let t = Aid::parse(target).ok_or_else(|| bad("Not a valid ID."))?;
        let mut tx = self.begin();
        let mut j = Js::new();
        j.obj().ks("a", &a.to_string()).ks("t", &t.to_string()).ks("n", name).end_obj();
        tx.emit(kind::NICK, j)?;
        Self::push_me(&mut tx, a);
        tx.commit()?;
        Ok("{}".into())
    }

    /// Change password: requires the current key.
    pub fn password(&self, a: Aid, sess: Hash, old: &[u8], salt: &[u8], key: &[u8]) -> R<String> {
        if salt.len() != 16 || key.len() != 32 {
            return Err(bad("bad key"));
        }
        let mut tx = self.begin();
        let acc = tx.st.accounts.get(&a).ok_or_else(|| Fail(401, "no account".into()))?;
        if !ct_eq(&self.verifier(old), &acc.ver) {
            return Err(forbidden("The current password is wrong."));
        }
        let mut j = Js::new();
        j.obj().ks("a", &a.to_string()).key("pw").obj();
        j.ks("s", &b64url_encode(salt)).ks("v", &b64url_encode(&self.verifier(key))).end_obj().end_obj();
        tx.emit(kind::ACCOUNT_SET, j)?;
        self.end_other_sessions(&mut tx, a, Some(sess))?;
        tx.commit()?;
        Ok("{}".into())
    }

    fn end_other_sessions(&self, tx: &mut Tx, a: Aid, keep: Option<Hash>) -> R<()> {
        let others: Vec<Hash> = tx.st.sessions.iter().filter(|(h, s)| s.a == a && Some(**h) != keep).map(|(h, _)| *h).collect();
        let mut j = Js::new();
        j.obj().ks("a", &a.to_string()).kb("all", true);
        if let Some(k) = keep {
            j.ks("keep", &b64url_encode(&k));
        }
        j.end_obj();
        tx.emit(kind::SESSION_END, j)?;
        for h in others {
            tx.kick_session(h);
        }
        Ok(())
    }

    pub fn sessions(&self, a: Aid, current: Hash) -> R<String> {
        let st = lock(&self.st);
        let mut js = Js::new();
        js.obj().key("s").arr();
        for (h, s) in st.sessions.iter().filter(|(_, s)| s.a == a) {
            js.obj().ks("h", &b64url_encode(h)).ks("l", &s.label).ku("t", s.created).kb("me", *h == current).end_obj();
        }
        js.end_arr().end_obj();
        Ok(js.finish())
    }

    pub fn session_end(&self, a: Aid, h: &str) -> R<String> {
        let h: Hash = b64url_decode(h).and_then(|v| v.try_into().ok()).ok_or_else(|| bad("bad session"))?;
        let mut tx = self.begin();
        if tx.st.sessions.get(&h).is_none_or(|s| s.a != a) {
            return Err(bad("unknown session"));
        }
        let mut j = Js::new();
        j.obj().ks("h", &b64url_encode(&h)).end_obj();
        tx.emit(kind::SESSION_END, j)?;
        tx.kick_session(h);
        tx.commit()?;
        Ok("{}".into())
    }

    // --------------------------------------------------------------------------------------------
    // Requests (P2, P3)
    // --------------------------------------------------------------------------------------------

    fn shares_conversation(st: &State, a: Aid, b: Aid) -> bool {
        st.accounts.get(&a).is_some_and(|acc| acc.convs.iter().filter_map(|c| st.convs.get(c)).any(|c| c.joined(a) && c.joined(b)))
    }

    pub fn req_send(&self, me: Aid, to: &str, note: &str) -> R<String> {
        let to = Aid::parse(to).ok_or_else(|| bad("That is not a valid ID. IDs look like K7Q2."))?;
        if to == me {
            return Err(bad("That is your own ID."));
        }
        let note: String = note.trim().chars().take(280).collect();
        let mut tx = self.begin();
        if let Some(&c) = tx.st.direct.get(&if me <= to { (me, to) } else { (to, me) }) {
            return Ok(format!("{{\"c\":\"{}\"}}", id64(c)));
        }
        let acc = &tx.st.accounts[&me];
        let pending = acc.req_out.len() as u64;
        let today = acc.req_times.iter().filter(|&&t| t + DAY_MS > now_ms()).count() as u64;
        if let Some(r) = acc.req_out.iter().filter_map(|r| tx.st.reqs.get(r)).find(|r| r.to == to) {
            return Ok(format!("{{\"r\":\"{}\"}}", id64(r.id)));
        }
        if pending >= tx.st.settings.req_pending_max {
            return Err(Fail(429, "You have too many pending requests. Cancel some first.".into()));
        }
        if today >= tx.st.settings.req_daily_max {
            return Err(Fail(429, "Daily request limit reached. Try again tomorrow.".into()));
        }
        // They already asked us: both sides acted, so connect right away.
        let reverse = tx.st.accounts.get(&to).and_then(|t| {
            t.req_out.iter().filter_map(|r| tx.st.reqs.get(r)).find(|r| r.to == me && !r.void).map(|r| r.id)
        });
        if let Some(r) = reverse {
            return self.accept_request(tx, me, r);
        }
        let void = match tx.st.accounts.get(&to) {
            Some(t) => !t.accept || t.disabled || t.blocked.contains(&me),
            None => true,
        };
        let r = new_id64();
        let mut j = Js::new();
        j.obj().ks("r", &id64(r)).ks("f", &me.to_string()).ks("t", &to.to_string()).ks("n", &note).kb("v", void).end_obj();
        tx.emit(kind::REQUEST, j)?;
        let req = tx.st.reqs[&r].clone();
        let mut out = Js::new();
        out.obj().ks("p", "rout").key("r");
        Self::req_out_json(&req, &mut out);
        out.end_obj();
        tx.push(me, &out.finish());
        if !void {
            let mut inn = Js::new();
            inn.obj().ks("p", "rin").key("r");
            Self::req_in_json(&tx.st, &req, &mut inn);
            inn.end_obj();
            tx.push(to, &inn.finish());
        }
        tx.commit()?;
        Ok(format!("{{\"r\":\"{}\"}}", id64(r)))
    }

    fn accept_request(&self, mut tx: Tx, me: Aid, r: u64) -> R<String> {
        let req = tx.st.reqs.get(&r).cloned().ok_or_else(|| bad("unknown request"))?;
        let c = new_id64();
        let mut j = Js::new();
        j.obj().ks("r", &id64(r)).ks("a", "acc").ks("c", &id64(c)).end_obj();
        tx.emit(kind::REQUEST_ANS, j)?;
        let c = tx.st.reqs[&r].conv.unwrap_or(c);
        Self::push_conv(&mut tx, c, &[req.from, req.to]);
        let gone = format!("{{\"p\":\"rgone\",\"r\":\"{}\"}}", id64(r));
        tx.push_many([req.from, req.to], &gone);
        let _ = me;
        tx.commit()?;
        Ok(format!("{{\"c\":\"{}\"}}", id64(c)))
    }

    pub fn req_answer(&self, me: Aid, r: &str, answer: &str) -> R<String> {
        let r = parse_id64(r).ok_or_else(|| bad("bad request id"))?;
        let mut tx = self.begin();
        if !tx.st.accounts[&me].req_in.contains(&r) {
            return Err(bad("That request is gone."));
        }
        match answer {
            "accept" => self.accept_request(tx, me, r),
            "decline" | "block" => {
                let mut j = Js::new();
                j.obj().ks("r", &id64(r)).ks("a", if answer == "block" { "blk" } else { "dec" }).end_obj();
                tx.emit(kind::REQUEST_ANS, j)?;
                tx.push(me, &format!("{{\"p\":\"rgone\",\"r\":\"{}\"}}", id64(r)));
                if answer == "block" {
                    Self::push_me(&mut tx, me);
                }
                tx.commit()?;
                Ok("{}".into())
            }
            _ => Err(bad("answer must be accept, decline or block")),
        }
    }

    pub fn req_cancel(&self, me: Aid, r: &str) -> R<String> {
        let r = parse_id64(r).ok_or_else(|| bad("bad request id"))?;
        let mut tx = self.begin();
        if !tx.st.accounts[&me].req_out.contains(&r) {
            return Err(bad("That request is gone."));
        }
        let to = tx.st.reqs[&r].to;
        let mut j = Js::new();
        j.obj().ks("r", &id64(r)).ks("a", "can").end_obj();
        tx.emit(kind::REQUEST_ANS, j)?;
        let gone = format!("{{\"p\":\"rgone\",\"r\":\"{}\"}}", id64(r));
        tx.push_many([me, to], &gone);
        tx.commit()?;
        Ok("{}".into())
    }

    // --------------------------------------------------------------------------------------------
    // Groups (P5, max 128 accounts)
    // --------------------------------------------------------------------------------------------

    fn invite_void(st: &State, by: Aid, who: Aid) -> bool {
        match st.accounts.get(&who) {
            Some(t) => t.disabled || t.blocked.contains(&by) || (!t.accept && !Self::shares_conversation(st, by, who)),
            None => true,
        }
    }

    fn invite_many(&self, tx: &mut Tx, me: Aid, c: u64, ids: &[Json]) -> R<()> {
        let mut want: Vec<Aid> = Vec::new();
        for v in ids {
            let a = v.as_str().and_then(Aid::parse).ok_or_else(|| bad("Not a valid ID in the list."))?;
            if a != me && !want.contains(&a) {
                want.push(a);
            }
        }
        let conv = tx.st.convs.get(&c).ok_or_else(|| bad("unknown conversation"))?;
        want.retain(|a| !conv.member(*a).is_some_and(|m| matches!(m.st, MState::Joined | MState::Invited)));
        if conv.seats() + want.len() > GROUP_MAX {
            return Err(bad(&format!("A group holds at most {GROUP_MAX} accounts (invites count).")));
        }
        for a in want {
            let void = Self::invite_void(&tx.st, me, a);
            let mut j = Js::new();
            j.obj().ks("c", &id64(c)).ks("a", &a.to_string()).ks("s", "inv").ks("by", &me.to_string()).kb("v", void).end_obj();
            tx.emit(kind::MEMBER, j)?;
            if !void {
                Self::push_conv(tx, c, &[a]);
            }
        }
        Ok(())
    }

    pub fn group_new(&self, me: Aid, name: &str, ids: &[Json]) -> R<String> {
        let name: String = name.trim().chars().take(80).collect();
        if name.is_empty() {
            return Err(bad("Give the group a name."));
        }
        if ids.len() + 1 > GROUP_MAX {
            return Err(bad(&format!("A group holds at most {GROUP_MAX} accounts.")));
        }
        let mut tx = self.begin();
        let c = new_id64();
        let mut j = Js::new();
        j.obj().ks("c", &id64(c)).ks("n", &name).ks("by", &me.to_string()).end_obj();
        tx.emit(kind::CONV, j)?;
        self.invite_many(&mut tx, me, c, ids)?;
        Self::push_conv(&mut tx, c, &[me]);
        tx.commit()?;
        Ok(format!("{{\"c\":\"{}\"}}", id64(c)))
    }

    fn joined_role(st: &State, c: u64, me: Aid) -> R<Role> {
        let conv = st.convs.get(&c).ok_or_else(|| bad("unknown conversation"))?;
        match conv.member(me) {
            Some(m) if m.st == MState::Joined => Ok(m.role),
            _ => Err(forbidden("You are not in that conversation.")),
        }
    }

    fn push_members(tx: &mut Tx, c: u64) {
        let ids: Vec<Aid> = tx.st.convs.get(&c).map(|cv| cv.joined_ids().collect()).unwrap_or_default();
        Self::push_conv(tx, c, &ids);
    }

    pub fn group_invite(&self, me: Aid, c: &str, ids: &[Json]) -> R<String> {
        let c = parse_id64(c).ok_or_else(|| bad("bad conversation id"))?;
        let mut tx = self.begin();
        Self::joined_role(&tx.st, c, me)?;
        if !tx.st.convs[&c].group {
            return Err(bad("Only groups take more people."));
        }
        self.invite_many(&mut tx, me, c, ids)?;
        Self::push_members(&mut tx, c);
        tx.commit()?;
        Ok("{}".into())
    }

    pub fn group_answer(&self, me: Aid, c: &str, accept: bool) -> R<String> {
        let c = parse_id64(c).ok_or_else(|| bad("bad conversation id"))?;
        let mut tx = self.begin();
        let ok = tx.st.convs.get(&c).and_then(|cv| cv.member(me)).is_some_and(|m| m.st == MState::Invited && !m.void);
        if !ok {
            return Err(bad("That invite is gone."));
        }
        let mut j = Js::new();
        j.obj().ks("c", &id64(c)).ks("a", &me.to_string()).ks("s", if accept { "join" } else { "dec" }).end_obj();
        tx.emit(kind::MEMBER, j)?;
        if accept {
            Self::push_members(&mut tx, c);
        } else {
            tx.push(me, &format!("{{\"p\":\"cgone\",\"c\":\"{}\"}}", id64(c)));
        }
        tx.commit()?;
        Ok("{}".into())
    }

    pub fn group_leave(&self, me: Aid, c: &str) -> R<String> {
        let c = parse_id64(c).ok_or_else(|| bad("bad conversation id"))?;
        let mut tx = self.begin();
        Self::joined_role(&tx.st, c, me)?;
        if !tx.st.convs[&c].group {
            return Err(bad("You can only leave groups."));
        }
        let mut j = Js::new();
        j.obj().ks("c", &id64(c)).ks("a", &me.to_string()).ks("s", "left").end_obj();
        tx.emit(kind::MEMBER, j)?;
        tx.push(me, &format!("{{\"p\":\"cgone\",\"c\":\"{}\"}}", id64(c)));
        Self::push_members(&mut tx, c);
        tx.commit()?;
        Ok("{}".into())
    }

    pub fn group_remove(&self, me: Aid, c: &str, who: &str) -> R<String> {
        let c = parse_id64(c).ok_or_else(|| bad("bad conversation id"))?;
        let who = Aid::parse(who).ok_or_else(|| bad("Not a valid ID."))?;
        let mut tx = self.begin();
        let role = Self::joined_role(&tx.st, c, me)?;
        let target = tx.st.convs[&c].member(who).cloned().ok_or_else(|| bad("Not in this group."))?;
        let allowed = target.by == me && target.st == MState::Invited
            || role == Role::Owner && target.role != Role::Owner
            || role == Role::Admin && target.role == Role::Member;
        if !allowed || who == me {
            return Err(forbidden("You can't remove that member."));
        }
        let mut j = Js::new();
        j.obj().ks("c", &id64(c)).ks("a", &who.to_string()).ks("s", "rem").ks("by", &me.to_string()).end_obj();
        tx.emit(kind::MEMBER, j)?;
        tx.push(who, &format!("{{\"p\":\"cgone\",\"c\":\"{}\"}}", id64(c)));
        Self::push_members(&mut tx, c);
        tx.commit()?;
        Ok("{}".into())
    }

    pub fn group_set(&self, me: Aid, c: &str, j: &Json) -> R<String> {
        let c = parse_id64(c).ok_or_else(|| bad("bad conversation id"))?;
        let mut tx = self.begin();
        let role = Self::joined_role(&tx.st, c, me)?;
        if role == Role::Member {
            return Err(forbidden("Only the owner or an admin can change the group."));
        }
        if let Some(n) = j.s("n") {
            let n: String = n.trim().chars().take(80).collect();
            if n.is_empty() {
                return Err(bad("Give the group a name."));
            }
            let mut e = Js::new();
            e.obj().ks("c", &id64(c)).ks("n", &n).end_obj();
            tx.emit(kind::CONV_SET, e)?;
        }
        if let (Some(who), Some(admin)) = (j.s("a").and_then(Aid::parse), j.b("admin")) {
            if role != Role::Owner {
                return Err(forbidden("Only the owner can change admins."));
            }
            let mut e = Js::new();
            e.obj().ks("c", &id64(c)).ks("a", &who.to_string()).ks("s", "role").ks("r", if admin { "a" } else { "m" }).end_obj();
            tx.emit(kind::MEMBER, e)?;
        }
        Self::push_members(&mut tx, c);
        tx.commit()?;
        Ok("{}".into())
    }

    // --------------------------------------------------------------------------------------------
    // Messages
    // --------------------------------------------------------------------------------------------

    pub fn history(&self, me: Aid, c: &str, before: Option<u64>, n: usize) -> R<String> {
        let c = parse_id64(c).ok_or_else(|| bad("bad conversation id"))?;
        let st = lock(&self.st);
        let conv = st.convs.get(&c).ok_or_else(|| bad("unknown conversation"))?;
        let since = match conv.member(me) {
            Some(m) if m.st == MState::Joined => m.since,
            _ => return Err(forbidden("You are not in that conversation.")),
        };
        let end = before.map_or(conv.msgs.len(), |b| conv.msgs.partition_point(|m| m.ts < b));
        let first = conv.msgs.partition_point(|m| m.ts < since);
        let start = end.saturating_sub(n.clamp(1, 200)).max(first);
        let mut js = Js::with_capacity(8192);
        js.obj().key("m").arr();
        for m in &conv.msgs[start..end.max(start)] {
            self.msg_json(&st, m, c, me, &mut js)?;
        }
        js.end_arr().kb("more", start > first).end_obj();
        Ok(js.finish())
    }

    pub fn read(&self, me: Aid, c: &str, t: u64) -> R<String> {
        let c = parse_id64(c).ok_or_else(|| bad("bad conversation id"))?;
        let mut tx = self.begin();
        Self::joined_role(&tx.st, c, me)?;
        let cur = tx.st.accounts[&me].reads.get(&c).copied().unwrap_or(0);
        if t > cur {
            let mut j = Js::new();
            j.obj().ks("a", &me.to_string()).ks("c", &id64(c)).ku("t", t).end_obj();
            tx.emit(kind::READ, j)?;
            tx.push(me, &format!("{{\"p\":\"read\",\"c\":\"{}\",\"t\":{t}}}", id64(c)));
        }
        tx.commit()?;
        Ok("{}".into())
    }

    /// Files a sender may attach: fresh uploads, or anything they already hold.
    fn resolve_attachment(st: &State, me: Aid, j: &Json) -> R<FileMeta> {
        let meta = FileMeta::from_json(j).ok_or_else(|| bad("bad attachment"))?;
        let acc = &st.accounts[&me];
        let held = acc.files.get(&meta.b).filter(|f| f.state == FState::Active);
        if let Some(f) = held {
            // Re-sending something already held: reuse the stored metadata, keep the new name.
            let mut m = (*f.meta).clone();
            m.name = meta.name;
            return Ok(m);
        }
        for (h, z) in [Some((meta.b, meta.z)), meta.t, meta.p].into_iter().flatten() {
            if !acc.uploads.contains_key(&h) {
                return Err(bad("An attachment was not uploaded (or expired). Attach it again."));
            }
            if st.blobs.get(&h).is_none_or(|b| b.z != z) {
                return Err(bad("An attachment's size does not match its upload."));
            }
        }
        Ok(meta)
    }

    /// R1: may `a` add `bytes` of new files? Err explains why not.
    fn check_send_room(st: &State, a: &Account, bytes: u64) -> R<()> {
        if bytes == 0 {
            return Ok(());
        }
        let q = st.settings.quota(a);
        if a.auto_recycle {
            if a.pinned + bytes > q {
                return Err(Fail(507, "Your pinned (always saved) files leave no room for this. Unpin some in the Cleaning Room.".into()));
            }
        } else if a.usage + bytes > q {
            return Err(Fail(507, "Not enough space. Free some in the Cleaning Room, or turn on auto-recycling.".into()));
        }
        Ok(())
    }

    /// Oldest unpinned active files to recycle until usage <= quota (never `protect`).
    pub fn plan_recycle(st: &State, a: &Account, protect: &[Hash]) -> Vec<Hash> {
        let q = st.settings.quota(a);
        if a.usage <= q {
            return Vec::new();
        }
        let mut cands: Vec<(&Hash, u64, u64)> = a
            .files
            .iter()
            .filter(|(h, f)| f.state == FState::Active && !f.pinned && !protect.contains(h))
            .map(|(h, f)| (h, f.added, f.meta.charged()))
            .collect();
        cands.sort_by_key(|&(h, added, _)| (added, *h));
        let mut usage = a.usage;
        let mut out = Vec::new();
        for (h, _, z) in cands {
            if usage <= q {
                break;
            }
            usage = usage.saturating_sub(z);
            out.push(*h);
        }
        out
    }

    fn emit_recycle(tx: &mut Tx, a: Aid, hs: &[Hash]) -> R<()> {
        if hs.is_empty() {
            return Ok(());
        }
        let mut j = Js::new();
        j.obj().ks("a", &a.to_string()).ks("o", "rec").key("b").arr();
        for h in hs {
            j.str(&hex(h));
        }
        j.end_arr().end_obj();
        tx.emit(kind::FILE, j)?;
        Ok(())
    }

    /// Immediate auto-recycling after new files arrive (R1/R2), unless a cleanup is scheduled (R5).
    fn auto_recycle_now(tx: &mut Tx, a: Aid, protect: &[Hash]) -> R<bool> {
        let Some(acc) = tx.st.accounts.get(&a) else { return Ok(false) };
        if !acc.auto_recycle || acc.cleanup_at.is_some() {
            return Ok(false);
        }
        let plan = Self::plan_recycle(&tx.st, acc, protect);
        Self::emit_recycle(tx, a, &plan)?;
        Ok(!plan.is_empty())
    }

    pub fn send(&self, me: Aid, c: &str, text: &str, atts: &[Json], nonce: Option<u64>) -> R<String> {
        let c = parse_id64(c).ok_or_else(|| bad("bad conversation id"))?;
        if text.chars().count() > MAX_TEXT {
            return Err(bad(&format!("Messages hold at most {MAX_TEXT} characters.")));
        }
        if text.trim().is_empty() && atts.is_empty() {
            return Err(bad("Empty message."));
        }
        if atts.len() > MAX_ATTACH {
            return Err(bad(&format!("At most {MAX_ATTACH} files per message.")));
        }
        if let Some(n) = nonce
            && let Some((id, _)) = lock(&self.nonces).get(&(me, n))
        {
            return Ok(id.clone()); // a retried send: same answer, no duplicate
        }
        let mut tx = self.begin();
        Self::joined_role(&tx.st, c, me)?;
        let metas: Vec<FileMeta> = atts.iter().map(|j| Self::resolve_attachment(&tx.st, me, j)).collect::<R<_>>()?;
        let mut seen = Vec::new();
        for m in &metas {
            if seen.contains(&m.b) {
                return Err(bad("The same file is attached twice."));
            }
            seen.push(m.b);
        }
        // R1 for the sender.
        let acc = &tx.st.accounts[&me];
        let new_bytes: u64 =
            metas.iter().filter(|m| acc.files.get(&m.b).is_none_or(|f| f.state != FState::Active)).map(FileMeta::charged).sum();
        Self::check_send_room(&tx.st, acc, new_bytes)?;
        let recipients: Vec<Aid> = tx.st.convs[&c].joined_ids().filter(|&a| a != me).collect();
        let mut j = Js::with_capacity(256 + text.len());
        j.obj().ks("c", &id64(c)).ks("f", &me.to_string()).ks("x", text);
        if !metas.is_empty() {
            j.key("at").arr();
            for m in &metas {
                m.write(&mut j);
            }
            j.end_arr().key("d").obj();
            // R2: each recipient decides alone; the sender is never told (P6).
            for &r in &recipients {
                let Some(ra) = tx.st.accounts.get(&r) else { continue };
                let q = tx.st.settings.quota(ra);
                let ceil = tx.st.settings.ceiling(ra);
                let (mut usage, mut pinned) = (ra.usage, ra.pinned);
                let mut bits = String::with_capacity(metas.len());
                for m in &metas {
                    let have = ra.files.get(&m.b).is_some_and(|f| f.state == FState::Active);
                    let z = m.charged();
                    let ok = have
                        || if ra.auto_recycle {
                            pinned + z <= ceil
                        } else {
                            usage + z <= q
                        };
                    if ok && !have {
                        usage += z;
                        if ra.auto_recycle {
                            pinned += 0; // new files arrive unpinned
                        }
                    }
                    bits.push(if ok { '1' } else { '0' });
                }
                let _ = pinned;
                j.ks(&r.to_string(), &bits);
            }
            j.end_obj();
        }
        j.end_obj();
        let (off, ts) = tx.emit(kind::MSG, j)?;
        let protect: Vec<Hash> = metas.iter().map(|m| m.b).collect();
        let mut changed = Vec::new();
        if !metas.is_empty() {
            for a in std::iter::once(me).chain(recipients.iter().copied()) {
                Self::auto_recycle_now(&mut tx, a, &protect)?;
                changed.push(a);
            }
        }
        // Push the message to every member, each with their own attachment states.
        let mref = MsgRef { ts, origin: 0, off, from: me };
        let id = format!("{}-{}", id64(self.me), off);
        for a in std::iter::once(me).chain(recipients.iter().copied()) {
            let mut js = Js::with_capacity(512);
            js.obj().ks("p", "msg").key("m");
            self.msg_json(&tx.st, &mref, c, a, &mut js)?;
            js.end_obj();
            tx.push(a, &js.finish());
        }
        for a in changed {
            Self::push_me(&mut tx, a);
            tx.push(a, "{\"p\":\"files\"}");
        }
        tx.commit()?;
        let reply = format!("{{\"id\":\"{id}\",\"t\":{ts}}}");
        if let Some(n) = nonce {
            let mut nm = lock(&self.nonces);
            if nm.len() > 50_000 {
                let cutoff = now_ms().saturating_sub(HOUR_MS);
                nm.retain(|_, v| v.1 > cutoff);
            }
            nm.insert((me, n), (reply.clone(), now_ms()));
        }
        Ok(reply)
    }

    // --------------------------------------------------------------------------------------------
    // Files: uploads, drive, cleaning room (R1-R9)
    // --------------------------------------------------------------------------------------------

    /// Free bytes on the relay's disk after the admin reserve (R9).
    pub fn disk_room(&self) -> u64 {
        let st = lock(&self.st);
        let (free, total) = disk_free(&self.data).unwrap_or((u64::MAX, u64::MAX));
        let reserve = (total / 100 * st.settings.reserve_pct).max(st.settings.reserve_min);
        free.saturating_sub(reserve)
    }

    pub fn upload_begin(&self, me: Aid, total: u64) -> R<String> {
        if !self.bouncer(3, &me.to_string(), 600.0, 600.0) {
            return Err(Fail(429, "Too many uploads at once.".into()));
        }
        if total > self.disk_room() {
            return Err(Fail(507, "The relay is out of disk space. Contact the admin.".into()));
        }
        {
            let st = lock(&self.st);
            let acc = st.accounts.get(&me).ok_or_else(|| Fail(401, "no account".into()))?;
            Self::check_send_room(&st, acc, total)?;
        }
        let id = self.store.begin(me.0, total).map_err(ioerr)?;
        Ok(format!("{{\"u\":\"{}\"}}", id64(id)))
    }

    /// One chunk. Returns `{"o":written}` or, when complete, `{"b":hash}`.
    pub fn upload_put(&self, me: Aid, up: &str, offset: u64, len: u64, body: &mut impl Read) -> R<String> {
        let id = parse_id64(up).ok_or_else(|| bad("bad upload id"))?;
        match self.store.put(id, me.0, offset, len, body) {
            Ok(Put::More(w)) => Ok(format!("{{\"o\":{w}}}")),
            Ok(Put::Done(h)) => {
                let z = self.store.open_blob(&h).map(|(_, z)| z).map_err(ioerr)?;
                let mut tx = self.begin();
                let mut j = Js::new();
                j.obj().ks("b", &hex(&h)).ku("z", z).ks("by", &me.to_string()).end_obj();
                tx.emit(kind::BLOB, j)?;
                tx.commit()?;
                Ok(format!("{{\"b\":\"{}\",\"z\":{z}}}", hex(&h)))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Err(Fail(404, "Upload not found; start again.".into())),
            Err(e) if e.kind() == io::ErrorKind::InvalidInput => {
                let w = self.store.status(id, me.0).map(|s| s.0).unwrap_or(0);
                Err(Fail(409, format!("{{\"o\":{w}}}")))
            }
            Err(e) => Err(ioerr(e)),
        }
    }

    pub fn upload_status(&self, me: Aid, up: &str) -> R<String> {
        let id = parse_id64(up).ok_or_else(|| bad("bad upload id"))?;
        let (w, t) = self.store.status(id, me.0).ok_or_else(|| Fail(404, "Upload not found; start again.".into()))?;
        Ok(format!("{{\"o\":{w},\"z\":{t}}}"))
    }

    /// Add an uploaded file to the personal drive.
    pub fn drive_add(&self, me: Aid, f: &Json) -> R<String> {
        let mut tx = self.begin();
        let meta = Self::resolve_attachment(&tx.st, me, f)?;
        let acc = &tx.st.accounts[&me];
        if acc.files.get(&meta.b).is_some_and(|x| x.state == FState::Active) {
            return Ok("{}".into());
        }
        Self::check_send_room(&tx.st, acc, meta.charged())?;
        let mut j = Js::new();
        j.obj().ks("a", &me.to_string()).ks("o", "add").key("f");
        meta.write(&mut j);
        j.end_obj();
        tx.emit(kind::FILE, j)?;
        Self::auto_recycle_now(&mut tx, me, &[meta.b])?;
        Self::push_me(&mut tx, me);
        tx.push(me, "{\"p\":\"files\"}");
        tx.commit()?;
        Ok("{}".into())
    }

    /// Every file ref of an account (drive + conversations), for Drive and the Cleaning Room.
    pub fn files(&self, me: Aid) -> R<String> {
        let st = lock(&self.st);
        let acc = st.accounts.get(&me).ok_or_else(|| Fail(401, "no account".into()))?;
        let mut js = Js::with_capacity(acc.files.len() * 200 + 64);
        js.obj().key("f").arr();
        for (h, f) in &acc.files {
            js.obj()
                .ks("b", &hex(h))
                .ks("n", &f.meta.name)
                .ks("m", &f.meta.mime)
                .ku("z", f.meta.z)
                .ku("cz", f.meta.charged())
                .ks("s", f.state.code())
                .kb("pin", f.pinned)
                .ku("t", f.added)
                .ku("ch", f.changed)
                .ks("d", match f.dir {
                    b'd' => "drive",
                    b's' => "sent",
                    _ => "recv",
                })
                .kb("th", f.meta.t.is_some())
                .kb("pv", f.meta.p.is_some())
                .kb("live", f.meta.blobs().all(|b| st.blobs.contains_key(&b)));
            if f.meta.w > 0 {
                js.ku("w", f.meta.w as u64).ku("h", f.meta.h as u64);
            }
            js.key("c").arr();
            for c in &f.convs {
                js.str(&id64(*c));
            }
            js.end_arr().end_obj();
        }
        js.end_arr();
        // R5 preview: exactly what a scheduled cleanup would recycle.
        js.key("queue").arr();
        for h in Self::plan_recycle(&st, acc, &[]) {
            js.str(&hex(&h));
        }
        js.end_arr().end_obj();
        Ok(js.finish())
    }

    /// pin / unpin / delete / get (receive now, get again) / cleanup (run the scheduled cleanup now).
    pub fn file_op(&self, me: Aid, op: &str, list: &[Json]) -> R<String> {
        let mut tx = self.begin();
        if op == "cleanup" {
            let acc = &tx.st.accounts[&me];
            let plan = Self::plan_recycle(&tx.st, acc, &[]);
            Self::emit_recycle(&mut tx, me, &plan)?;
            let mut j = Js::new();
            j.obj().ks("a", &me.to_string()).key("cl").null().end_obj();
            tx.emit(kind::ACCOUNT_SET, j)?;
            Self::push_me(&mut tx, me);
            tx.push(me, "{\"p\":\"files\"}");
            tx.commit()?;
            return Ok(format!("{{\"n\":{}}}", plan.len()));
        }
        let hashes: Vec<Hash> = list.iter().filter_map(|x| unhex(x.as_str()?)?.try_into().ok()).collect();
        if hashes.is_empty() {
            return Ok("{}".into());
        }
        let code = match op {
            "pin" => "pin",
            "unpin" => "unpin",
            "delete" => "del",
            "get" => "get",
            _ => return Err(bad("unknown file operation")),
        };
        let acc = &tx.st.accounts[&me];
        if code == "get" {
            // R8 / "Receive now": judged like receiving (R2), and the file must still exist.
            let mut need = 0u64;
            for h in &hashes {
                let f = acc.files.get(h).ok_or_else(|| bad("unknown file"))?;
                if f.state == FState::Active {
                    continue;
                }
                if !f.meta.blobs().all(|b| tx.st.blobs.contains_key(&b)) {
                    return Err(Fail(410, "Nobody holds that file any more.".into()));
                }
                need += f.meta.charged();
            }
            let q = tx.st.settings.quota(acc);
            let room = if acc.auto_recycle {
                acc.pinned + need <= tx.st.settings.ceiling(acc)
            } else {
                acc.usage + need <= q
            };
            if !room {
                return Err(Fail(507, "Not enough space for that. Free some first.".into()));
            }
        }
        let mut j = Js::new();
        j.obj().ks("a", &me.to_string()).ks("o", code).key("b").arr();
        for h in &hashes {
            j.str(&hex(h));
        }
        j.end_arr().end_obj();
        tx.emit(kind::FILE, j)?;
        if code == "get" {
            Self::auto_recycle_now(&mut tx, me, &hashes)?;
        }
        Self::push_me(&mut tx, me);
        tx.push(me, "{\"p\":\"files\"}");
        tx.commit()?;
        Ok("{}".into())
    }

    /// Which blob to serve for `GET /f/<orig>/<o|t|p>`, if `me` holds the file.
    pub fn blob_for(&self, me: Aid, orig: &str, which: &str) -> Option<(Hash, String, String)> {
        let h: Hash = unhex(orig)?.try_into().ok()?;
        let st = lock(&self.st);
        let f = st.accounts.get(&me)?.files.get(&h)?;
        if f.state != FState::Active {
            return None;
        }
        match which {
            "o" => Some((h, f.meta.mime.clone(), f.meta.name.clone())),
            "t" => f.meta.t.map(|(b, _)| (b, "image/webp".into(), "thumb.webp".into())),
            "p" => f.meta.p.map(|(b, _)| (b, "image/webp".into(), "preview.webp".into())),
            _ => None,
        }
    }

    // --------------------------------------------------------------------------------------------
    // Dustin, the recycling robot: scheduled cleanups (R5/R7), upload and rate-limit housekeeping
    // --------------------------------------------------------------------------------------------

    pub fn dustin(&self) {
        self.store.gc_uploads(HOUR_MS);
        {
            let now = now_ms();
            lock(&self.limits).retain(|_, v| now.saturating_sub(v.1) < 10 * 60_000);
            lock(&self.nonces).retain(|_, v| now.saturating_sub(v.1) < HOUR_MS);
        }
        let now = now_ms();
        let mut tx = self.begin();
        let mut touched = Vec::new();
        let ids: Vec<Aid> = tx.st.accounts.keys().copied().collect();
        for a in ids {
            let acc = &tx.st.accounts[&a];
            let q = tx.st.settings.quota(acc);
            let mine = acc.cleanup_by == self.me || !tx.st.relays.contains_key(&acc.cleanup_by);
            match acc.cleanup_at {
                // R5/R7: over quota with something recyclable -> schedule (grace window).
                None if acc.auto_recycle && acc.usage > q && acc.recyclable() > 0 => {
                    let at = now + tx.st.settings.cleanup_h * HOUR_MS;
                    let mut j = Js::new();
                    j.obj().ks("a", &a.to_string()).ku("cl", at).ks("clb", &id64(self.me)).end_obj();
                    if tx.emit(kind::ACCOUNT_SET, j).is_ok() {
                        touched.push(a);
                    }
                }
                Some(t) if t <= now && mine => {
                    let plan = Self::plan_recycle(&tx.st, acc, &[]);
                    let _ = Self::emit_recycle(&mut tx, a, &plan);
                    let mut j = Js::new();
                    j.obj().ks("a", &a.to_string()).key("cl").null().end_obj();
                    if tx.emit(kind::ACCOUNT_SET, j).is_ok() {
                        touched.push(a);
                    }
                }
                _ => {}
            }
            // Unattached uploads expire after a day.
            let stale: Vec<Hash> = tx.st.accounts[&a].uploads.iter().filter(|(_, t)| **t + DAY_MS < now).map(|(h, _)| *h).collect();
            for h in stale {
                let st = &mut *tx.st;
                if let Some(acc) = st.accounts.get_mut(&a) {
                    acc.uploads.remove(&h);
                }
                if let Some(b) = st.blobs.get_mut(&h) {
                    b.pending = b.pending.saturating_sub(1);
                    if b.refs == 0 && b.pending == 0 {
                        st.blobs.remove(&h);
                        st.dead_blobs.push(h);
                    }
                }
            }
        }
        for a in touched {
            Self::push_me(&mut tx, a);
            tx.push(a, "{\"p\":\"files\"}");
        }
        let _ = tx.commit();
    }

    // --------------------------------------------------------------------------------------------
    // Admin (M4 fills the screen; the Bridge already enforces it)
    // --------------------------------------------------------------------------------------------

    pub fn is_admin(&self, a: Aid) -> bool {
        lock(&self.st).accounts.get(&a).is_some_and(|x| x.admin)
    }

    pub fn admin_overview(&self) -> R<String> {
        let st = lock(&self.st);
        let (free, total) = disk_free(&self.data).unwrap_or((0, 0));
        let mut js = Js::with_capacity(8192);
        js.obj().key("set").obj();
        let s = &st.settings;
        js.ku("default_quota", s.default_quota)
            .ku("flex_pct", s.flex_pct)
            .ku("cleanup_h", s.cleanup_h)
            .ku("reserve_pct", s.reserve_pct)
            .ku("reserve_min", s.reserve_min)
            .ks("signup", s.signup.as_str())
            .ku("member_invites", s.member_invites)
            .ku("req_pending_max", s.req_pending_max)
            .ku("req_daily_max", s.req_daily_max)
            .ks("smtp.host", &s.smtp.host)
            .ku("smtp.port", s.smtp.port)
            .ks("smtp.user", &s.smtp.user)
            .kb("smtp.pass_set", !s.smtp.pass.is_empty())
            .ks("smtp.from", &s.smtp.from)
            .ks("smtp.tls", &s.smtp.tls)
            .end_obj();
        js.key("disk").obj().ku("free", free).ku("total", total).ku("room", free.saturating_sub((total / 100 * s.reserve_pct).max(s.reserve_min))).end_obj();
        let committed: u64 = st.accounts.values().map(|a| s.quota(a)).sum();
        let used: u64 = st.blobs.values().map(|b| b.z).sum();
        js.ku("committed", committed).ku("stored", used).ku("online", self.board.online() as u64);
        js.key("accounts").arr();
        let mut accs: Vec<&Account> = st.accounts.values().collect();
        accs.sort_by_key(|a| a.created);
        for a in accs {
            js.obj()
                .ks("id", &a.id.to_string())
                .ks("n", &a.name)
                .ku("t", a.created)
                .ku("u", a.usage)
                .ku("pin", a.pinned)
                .ku("q", s.quota(a))
                .kb("qo", a.quota.is_some())
                .kb("ar", a.auto_recycle)
                .kb("adm", a.admin)
                .kb("dis", a.disabled)
                .ku("files", a.files.values().filter(|f| f.state == FState::Active).count() as u64)
                .ku("convs", a.convs.len() as u64)
                .end_obj();
        }
        js.end_arr().key("invites").arr();
        for i in st.invites.values().filter(|i| i.uses > 0) {
            js.obj().ks("c", &i.code).ku("u", i.uses).ku("x", i.exp).kb("adm", i.admin).ku("t", i.created);
            js.kos("by", i.by.map(|b| b.to_string()).as_deref()).end_obj();
        }
        js.end_arr().key("relays").arr();
        for r in st.relays.values() {
            js.obj().ks("id", &id64(r.id)).ks("n", &r.name).ks("u", &r.url).kb("me", r.id == self.me).end_obj();
        }
        js.end_arr().end_obj();
        Ok(js.finish())
    }

    pub fn admin_set(&self, me: Aid, j: &Json) -> R<String> {
        let Json::Obj(kv) = j.get("set").unwrap_or(&Json::Null) else { return Err(bad("expected set")) };
        let mut tx = self.begin();
        for (k, v) in kv {
            if !SETTING_KEYS.contains(&k.as_str()) {
                return Err(bad(&format!("unknown setting {k}")));
            }
            let mut probe = tx.st.settings.clone();
            if !probe.set(k, v, (u64::MAX, u64::MAX)) {
                return Err(bad(&format!("bad value for {k}")));
            }
            let mut e = Js::new();
            e.obj().ks("k", k).key("v");
            v.write(&mut e);
            e.end_obj();
            tx.emit(kind::SETTING, e)?;
        }
        let _ = me;
        let all: Vec<Aid> = tx.st.accounts.keys().copied().collect();
        for a in all {
            Self::push_me(&mut tx, a);
        }
        tx.commit()?;
        Ok("{}".into())
    }

    pub fn admin_account(&self, me: Aid, j: &Json) -> R<String> {
        let a = j.s("id").and_then(Aid::parse).ok_or_else(|| bad("bad id"))?;
        let mut tx = self.begin();
        if !tx.st.accounts.contains_key(&a) {
            return Err(bad("unknown account"));
        }
        let mut e = Js::new();
        e.obj().ks("a", &a.to_string());
        if let Some(q) = j.get("q") {
            e.key("q");
            match q.as_u64() {
                Some(v) => e.uint(v),
                None => e.null(),
            };
        }
        if let Some(v) = j.b("adm") {
            if a == me && !v {
                return Err(bad("You can't remove your own admin rights."));
            }
            e.kb("adm", v);
        }
        if let Some(v) = j.b("dis") {
            if a == me {
                return Err(bad("You can't disable yourself."));
            }
            e.kb("dis", v);
        }
        e.end_obj();
        tx.emit(kind::ACCOUNT_SET, e)?;
        if j.b("dis") == Some(true) {
            let hs: Vec<Hash> = tx.st.sessions.iter().filter(|(_, s)| s.a == a).map(|(h, _)| *h).collect();
            for h in hs {
                tx.kick_session(h);
            }
        }
        Self::push_me(&mut tx, a);
        tx.commit()?;
        Ok("{}".into())
    }

    pub fn invite_create(&self, me: Aid, uses: u64, days: u64, admin: bool) -> R<String> {
        let mut tx = self.begin();
        let acc = &tx.st.accounts[&me];
        let is_admin = acc.admin;
        if !is_admin {
            let allowed = tx.st.settings.member_invites;
            if acc.invites_made >= allowed {
                return Err(forbidden("You can't create more invites."));
            }
        }
        let code = new_code(10);
        let uses = if is_admin { uses.clamp(1, 1000) } else { 1 };
        let exp = now_ms() + days.clamp(1, 365) * DAY_MS;
        let mut j = Js::new();
        j.obj().ks("c", &code).ks("by", &me.to_string()).ku("u", uses).ku("x", exp).kb("adm", admin && is_admin).end_obj();
        tx.emit(kind::INVITE, j)?;
        tx.commit()?;
        Ok(format!("{{\"c\":\"{code}\"}}"))
    }

    pub fn invite_delete(&self, code: &str) -> R<String> {
        let mut tx = self.begin();
        let mut j = Js::new();
        j.obj().ks("c", code).end_obj();
        tx.emit(kind::INVITE_DEL, j)?;
        tx.commit()?;
        Ok("{}".into())
    }

    pub fn my_invites(&self, me: Aid) -> R<String> {
        let st = lock(&self.st);
        let mut js = Js::new();
        js.obj().key("i").arr();
        for i in st.invites.values().filter(|i| i.by == Some(me) && i.uses > 0) {
            js.obj().ks("c", &i.code).ku("u", i.uses).ku("x", i.exp).end_obj();
        }
        let left = st.settings.member_invites.saturating_sub(st.accounts[&me].invites_made);
        js.end_arr().ku("left", left).end_obj();
        Ok(js.finish())
    }
}

// ------------------------------------------------------------------------------------------------
// Lifeline (account recovery), Sparks, and the Ring host
// ------------------------------------------------------------------------------------------------

/// 4 groups of 5 from the callsign alphabet (~95 bits).
fn lifeline_code() -> String {
    let raw = new_code(20);
    raw.as_bytes().chunks(5).map(|c| String::from_utf8_lossy(c).into_owned()).collect::<Vec<_>>().join("-")
}

fn normalize_code(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_uppercase()).collect()
}

fn valid_email(e: &str) -> bool {
    let Some((u, d)) = e.split_once('@') else { return false };
    e.len() <= 254
        && !u.is_empty()
        && d.contains('.')
        && !d.starts_with('.')
        && !d.ends_with('.')
        && !e.contains('@'.to_string().repeat(2).as_str())
        && e.chars().all(|c| !c.is_whitespace() && !c.is_control() && c != '<' && c != '>' && c != '"')
        && !d.contains('@')
}

impl Bridge {
    pub fn notify_grew(&self) {
        let (m, cv) = &self.grew;
        *lock(m) += 1;
        cv.notify_all();
    }

    /// Pick a Lifeline: none, email (kept only for recovery) or code (shown once, stored as HMAC).
    pub fn lifeline_set(&self, a: Aid, j: &Json) -> R<String> {
        let mut tx = self.begin();
        let mut e = Js::new();
        e.obj().ks("a", &a.to_string()).key("rec").obj();
        let mut out = "{}".to_string();
        match j.s("k").unwrap_or("") {
            "none" => {
                e.ks("k", "none");
            }
            "email" => {
                let addr = j.s("e").unwrap_or("").trim();
                if !valid_email(addr) {
                    return Err(bad("That email address looks wrong."));
                }
                e.ks("k", "email").ks("e", addr);
            }
            "code" => {
                let code = lifeline_code();
                let v = hmac_sha256_parts(&self.pepper, &[b"lifeline:", normalize_code(&code).as_bytes()]);
                e.ks("k", "code").ks("v", &b64url_encode(&v));
                out = format!("{{\"code\":\"{code}\"}}");
            }
            _ => return Err(bad("Pick none, email or code.")),
        }
        e.end_obj().end_obj();
        tx.emit(kind::ACCOUNT_SET, e)?;
        Self::push_me(&mut tx, a);
        tx.commit()?;
        Ok(out)
    }

    /// New password; every device signed out; a code Lifeline is spent.
    fn reset_password(&self, mut tx: Tx, a: Aid, salt: &[u8], key: &[u8], label: &str, spend_code: bool) -> R<(Aid, String)> {
        if salt.len() != 16 || key.len() != 32 {
            return Err(bad("bad key"));
        }
        let mut j = Js::new();
        j.obj().ks("a", &a.to_string()).key("pw").obj();
        j.ks("s", &b64url_encode(salt)).ks("v", &b64url_encode(&self.verifier(key))).end_obj();
        if spend_code {
            j.key("rec").obj().ks("k", "none").end_obj();
        }
        j.end_obj();
        tx.emit(kind::ACCOUNT_SET, j)?;
        self.end_other_sessions(&mut tx, a, None)?;
        let token = self.new_session(&mut tx, a, label)?;
        tx.commit()?;
        Ok((a, token))
    }

    pub fn recover_code(&self, id: &str, code: &str, salt: &[u8], key: &[u8], label: &str, ip: IpAddr) -> R<(Aid, String)> {
        if !self.bouncer(6, &ip.to_string(), 5.0, 10.0) {
            return Err(Fail(429, "Too many attempts. Wait a few minutes.".into()));
        }
        let wrong = || Fail(403, "That callsign and Lifeline code don't match.".into());
        let a = Aid::parse(id).ok_or_else(wrong)?;
        if !self.bouncer(7, &a.to_string(), 2.0, 5.0) {
            return Err(Fail(429, "Too many attempts for this callsign. Wait a few minutes.".into()));
        }
        let v = hmac_sha256_parts(&self.pepper, &[b"lifeline:", normalize_code(code).as_bytes()]);
        let tx = self.begin();
        let acc = tx.st.accounts.get(&a).ok_or_else(wrong)?;
        match &acc.recovery {
            Recovery::Code(h) if ct_eq(h, &v) => {}
            _ => return Err(wrong()),
        }
        self.reset_password(tx, a, salt, key, label, true)
    }

    /// Always answers the same way (no oracle). Emails a 30-minute link when it can.
    pub fn recover_email(&self, id: &str, base_url: &str, ip: IpAddr) -> R<String> {
        if !self.bouncer(6, &ip.to_string(), 5.0, 10.0) {
            return Err(Fail(429, "Too many attempts. Wait a few minutes.".into()));
        }
        let sent = "{\"ok\":true}".to_string();
        let smtp = lock(&self.st).settings.smtp.clone();
        if smtp.host.is_empty() {
            return Err(Fail(503, "Email Lifelines aren't switched on at this relay yet. Ask the admin.".into()));
        }
        let Some(a) = Aid::parse(id) else { return Ok(sent) };
        if !self.bouncer(8, &a.to_string(), 0.2, 3.0) {
            return Ok(sent);
        }
        let mut tx = self.begin();
        let Some(Recovery::Email(addr)) = tx.st.accounts.get(&a).map(|x| x.recovery.clone()) else { return Ok(sent) };
        let mut t = [0u8; 32];
        random_bytes(&mut t);
        let mut j = Js::new();
        j.obj().ks("a", &a.to_string()).ks("h", &b64url_encode(&sha256(&t))).ku("x", now_ms() + 30 * 60_000).end_obj();
        tx.emit(kind::RECOVERY, j)?;
        tx.commit()?;
        let link = format!("{base_url}/#lifeline={}", b64url_encode(&t));
        let body = format!(
            "Hi {a},\r\n\r\nSomeone (hopefully you) asked to reset the password of ESC callsign {a}.\r\n\r\nOpen this link within 30 minutes to choose a new password:\r\n{link}\r\n\r\nIf it wasn't you, ignore this email: nothing changes.\r\n\r\n-- ESC"
        );
        thread::spawn(move || {
            let tls = if smtp.tls.is_empty() { "ssl".to_string() } else { smtp.tls.clone() };
            let m = Mail { host: &smtp.host, port: smtp.port as u16, user: &smtp.user, pass: &smtp.pass, from: &smtp.from, tls: &tls };
            if let Err(e) = courier::send_mail(&m, &addr, "ESC Lifeline: reset your password", &body) {
                eprintln!("esc: lifeline email failed: {e}");
            }
        });
        Ok(sent)
    }

    pub fn recover_token(&self, token: &str, salt: &[u8], key: &[u8], label: &str) -> R<(Aid, String)> {
        let t = b64url_decode(token).filter(|v| v.len() == 32).ok_or_else(|| bad("That link is broken."))?;
        let h = sha256(&t);
        let mut tx = self.begin();
        let gone = || Fail(410, "That link was already used or has expired. Ask for a new one.".into());
        let (a, exp) = tx.st.recov.get(&h).copied().ok_or_else(gone)?;
        if exp < now_ms() {
            return Err(gone());
        }
        let mut j = Js::new();
        j.obj().ks("h", &b64url_encode(&h)).end_obj();
        tx.emit(kind::RECOVERY_USE, j)?;
        self.reset_password(tx, a, salt, key, label, false)
    }

    pub fn mail_test(&self, _a: Aid, to: &str) -> R<String> {
        let smtp = lock(&self.st).settings.smtp.clone();
        if smtp.host.is_empty() {
            return Err(bad("Set the SMTP host first."));
        }
        let tls = if smtp.tls.is_empty() { "ssl".to_string() } else { smtp.tls.clone() };
        let m = Mail { host: &smtp.host, port: smtp.port as u16, user: &smtp.user, pass: &smtp.pass, from: &smtp.from, tls: &tls };
        courier::send_mail(&m, to, "ESC test email", "If you can read this, Email Lifelines work on this relay.")
            .map_err(|e| Fail(502, format!("Mail failed: {e}")))?;
        Ok("{}".into())
    }

    /// A one-time Spark (24 h) that lets a new machine join the Ring as a Star.
    pub fn spark_create(&self) -> R<String> {
        let code = new_code(16);
        lock(&self.sparks).insert(code.clone(), now_ms() + DAY_MS);
        let mut js = Js::new();
        js.obj().ks("spark", &code).ks("url", &lock(&self.url)).end_obj();
        Ok(js.finish())
    }

    /// `POST /ring/join` from a new Star: spend the Spark, announce the Star, hand over the secrets.
    pub fn ring_join(&self, j: &Json) -> R<String> {
        let spark = j.s("t").unwrap_or("");
        match lock(&self.sparks).remove(spark) {
            Some(exp) if exp > now_ms() => {}
            _ => return Err(Fail(403, "That Spark is not valid (a Spark works once, within 24 h).".into())),
        }
        let id = j.s("id").and_then(parse_id64).ok_or_else(|| bad("bad star id"))?;
        let url = j.s("u").unwrap_or("").trim().to_string();
        let mut tx = self.begin();
        let used: Vec<String> = tx.st.relays.values().map(|r| r.name.clone()).collect();
        let name: String = match j.s("n").map(str::trim).filter(|n| !n.is_empty()) {
            Some(n) => n.chars().take(40).collect(),
            None => STARS
                .iter()
                .find(|s| !used.iter().any(|u| u == *s))
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("Star {}", used.len() + 1)),
        };
        let mut e = Js::new();
        e.obj().ks("r", &id64(id)).ks("n", &name).ks("u", &url).end_obj();
        tx.emit(kind::RELAY, e)?;
        let mut out = Js::new();
        out.obj().ks("key", &hex(&self.ring_key)).ks("pepper", &hex(&self.pepper)).ks("name", &name).key("peers").arr();
        for r in tx.st.relays.values().filter(|r| r.id != id) {
            out.obj().ks("id", &id64(r.id)).ks("n", &r.name).ks("u", &r.url).end_obj();
        }
        out.end_arr().end_obj();
        tx.commit()?;
        Ok(out.finish())
    }

    /// Announce this Star's public URL to the Ring (quick tunnels get a new one on every restart).
    pub fn announce_url(&self, url: &str) -> R<()> {
        *lock(&self.url) = url.to_string();
        let mut tx = self.begin();
        if tx.st.relays.get(&self.me).is_some_and(|r| r.url == url) {
            return Ok(());
        }
        let mut e = Js::new();
        e.obj().ks("r", &id64(self.me)).ks("n", &self.name).ks("u", url).end_obj();
        tx.emit(kind::RELAY, e)?;
        tx.commit()
    }

    fn origin_index(st: &State, origin: u64) -> Option<usize> {
        st.origins.iter().position(|&o| o == origin)
    }

    /// Pushes for events that arrived from another Star, so people see them live.
    fn remote_pushes(&self, st: &State, evs: &[(u16, u64, u16, Json)]) -> Vec<(Aid, String)> {
        let mut out: Vec<(Aid, String)> = Vec::new();
        let mut resync: Vec<Aid> = Vec::new();
        for (origin, off, kind_, j) in evs {
            let c = j.s("c").and_then(parse_id64);
            match *kind_ {
                kind::MSG => {
                    let (Some(c), Some(from)) = (c, j.s("f").and_then(Aid::parse)) else { continue };
                    let Some(conv) = st.convs.get(&c) else { continue };
                    let Some(m) = conv.msgs.iter().rev().find(|m| m.origin == *origin && m.off == *off).copied() else { continue };
                    for a in conv.joined_ids() {
                        let mut js = Js::new();
                        js.obj().ks("p", "msg").key("m");
                        if self.msg_json(st, &m, c, a, &mut js).is_ok() {
                            js.end_obj();
                            out.push((a, js.finish()));
                        }
                        if a == from || j.get("d").is_some() {
                            resync.push(a);
                        }
                    }
                }
                kind::CONV | kind::MEMBER | kind::CONV_SET => {
                    if let Some(conv) = c.and_then(|c| st.convs.get(&c)) {
                        let mut who: Vec<Aid> = conv.joined_ids().collect();
                        who.extend(j.s("a").and_then(Aid::parse));
                        for a in who {
                            if let Some(m) = Self::conv_push(st, conv.id, a) {
                                out.push((a, m));
                            }
                        }
                    }
                }
                kind::SETTING => resync.extend(st.accounts.keys().copied()),
                _ => {
                    for k in ["a", "f", "t", "by"] {
                        resync.extend(j.s(k).and_then(Aid::parse));
                    }
                    if let Some(r) = j.s("r").and_then(parse_id64).and_then(|r| st.reqs.get(&r)) {
                        resync.push(r.from);
                        resync.push(r.to);
                    }
                }
            }
        }
        resync.sort();
        resync.dedup();
        for a in resync {
            out.push((a, "{\"p\":\"sync\"}".to_string()));
        }
        out
    }
}

impl crate::ring::Host for Bridge {
    fn me(&self) -> u64 {
        self.me
    }

    fn ring_key(&self) -> Hash {
        self.ring_key
    }

    fn have(&self) -> Vec<(u64, u64)> {
        let st = lock(&self.st);
        let logs = self.logs.read().unwrap_or_else(|e| e.into_inner());
        st.origins.iter().zip(logs.iter()).map(|(id, l)| (*id, l.end())).collect()
    }

    fn read(&self, origin: u64, from: u64, max: usize) -> io::Result<Vec<u8>> {
        let idx = Self::origin_index(&lock(&self.st), origin).ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        let lg = self.logs.read().unwrap_or_else(|e| e.into_inner())[idx].clone();
        lg.read_raw(from, max)
    }

    fn append(&self, origin: u64, at: u64, bytes: &[u8]) -> io::Result<u64> {
        if origin == self.me {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "this Star is the only writer of its own Logbook"));
        }
        let mut st = lock(&self.st);
        let idx = match Self::origin_index(&st, origin) {
            Some(i) => i,
            None => {
                let path = self.data.join("log").join(format!("{}.log", id64(origin)));
                let (lg, _) = Log::open(&path, |_, _, _, _| {})?;
                let mut logs = self.logs.write().unwrap_or_else(|e| e.into_inner());
                logs.push(Arc::new(lg));
                st.origins.push(origin);
                logs.len() - 1
            }
        };
        let lg = self.logs.read().unwrap_or_else(|e| e.into_inner())[idx].clone();
        let new_end = lg.append_raw(at, bytes)?;
        let mut applied = Vec::new();
        let (mut off, mut rest) = (at, bytes);
        while let Ok(Some((n, ts, kind_, json))) = logbook::decode(rest) {
            if let Ok(j) = json::parse_bytes(json) {
                st.apply(Ev { origin: idx as u16, off, ts, kind: kind_, j: j.clone() });
                applied.push((idx as u16, off, kind_, j));
            }
            off += n as u64;
            rest = &rest[n..];
        }
        let dead = std::mem::take(&mut st.dead_blobs);
        let pushes = self.remote_pushes(&st, &applied);
        drop(st);
        lg.sync(new_end)?;
        for (a, m) in pushes {
            self.board.send(a, &Arc::from(m));
        }
        for h in dead {
            let _ = self.store.remove(&h);
        }
        self.notify_grew();
        Ok(new_end)
    }

    fn wait(&self, have: &[(u64, u64)], timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        let (m, cv) = &self.grew;
        let mut g = lock(m);
        loop {
            let now = self.have_now();
            let grown = now.iter().any(|(o, len)| have.iter().find(|(h, _)| h == o).is_none_or(|(_, l)| len > l));
            if grown {
                return true;
            }
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return false;
            }
            g = cv.wait_timeout(g, left).unwrap_or_else(|e| e.into_inner()).0;
        }
    }

    fn peers(&self) -> Vec<(u64, String)> {
        let st = lock(&self.st);
        let mut out: Vec<(u64, String)> =
            st.relays.values().filter(|r| r.id != self.me && !r.url.is_empty()).map(|r| (r.id, r.url.clone())).collect();
        for s in &self.seeds {
            let mine = st.relays.get(&self.me).is_some_and(|r| r.url == *s);
            if !mine && !out.iter().any(|(_, u)| u == s) {
                out.push((0, s.clone()));
            }
        }
        out
    }

    fn missing_blobs(&self) -> Vec<(Hash, u64)> {
        let st = lock(&self.st);
        st.blobs.iter().filter(|(h, b)| (b.refs > 0 || b.pending > 0) && !self.store.has(h)).map(|(h, b)| (*h, b.z)).collect()
    }

    fn import_blob(&self, h: &Hash, len: u64, mut r: &mut dyn Read) -> io::Result<()> {
        self.store.import(h, len, &mut r)
    }

    fn open_blob(&self, h: &Hash) -> io::Result<(fs::File, u64)> {
        self.store.open_blob(h)
    }
}

impl Bridge {
    fn have_now(&self) -> Vec<(u64, u64)> {
        crate::ring::Host::have(self)
    }
}

fn mask_email(e: &str) -> String {
    match e.split_once('@') {
        Some((u, d)) => format!("{}***@{d}", u.chars().next().unwrap_or('*')),
        None => "***".into(),
    }
}

/// Human-friendly code from the ID alphabet (no vowels, no look-alikes).
pub fn new_code(n: usize) -> String {
    let mut b = vec![0u8; n];
    random_bytes(&mut b);
    b.iter().map(|x| crate::ids::ALPHABET[(*x as usize) % 27] as char).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::parse;

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("esc-Bridge-{name}-{}-{}", std::process::id(), new_id64()));
        let _ = fs::remove_dir_all(&p);
        p
    }

    fn signup(bridge: &Bridge, inv: &str) -> Aid {
        bridge.signup(inv, &[7u8; 16], &[9u8; 32], "Ann", "test").unwrap().0
    }

    fn open_signup(bridge: &Bridge) {
        let admin = bridge.st.lock().unwrap().accounts.values().find(|a| a.admin).map(|a| a.id);
        let a = admin.unwrap();
        bridge.admin_set(a, &parse(r#"{"set":{"signup":"open"}}"#).unwrap()).unwrap();
    }

    #[test]
    fn genesis_signup_login_and_replay() {
        let dir = tmp("genesis");
        let o = Bridge::open(&dir, None).unwrap();
        let code = o.admin_invite.clone().unwrap();
        assert!(o.bridge.signup("wrong", &[1; 16], &[2; 32], "", "x").is_err(), "invite-only by default");
        let (admin, token) = o.bridge.signup(&code, &[1; 16], &[2; 32], "Boss", "x").unwrap();
        assert!(o.bridge.is_admin(admin));
        assert!(o.bridge.signup(&code, &[1; 16], &[2; 32], "", "x").is_err(), "one-time invite");
        assert_eq!(o.bridge.auth(&token).map(|x| x.0), Some(admin));
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        assert!(o.bridge.login(&admin.to_string(), &[3; 32], "x", ip).is_err());
        assert!(o.bridge.login(&admin.to_string(), &[2; 32], "x", ip).is_ok());
        assert_eq!(o.bridge.pre(&admin.to_string()), b64url_encode(&[1; 16]));
        assert_eq!(o.bridge.pre("ZZZZ"), o.bridge.pre("zzzz"), "fake salts are stable");
        drop(o);
        let o2 = Bridge::open(&dir, None).unwrap();
        assert!(o2.admin_invite.is_none());
        assert_eq!(o2.bridge.auth(&token).map(|x| x.0), Some(admin), "sessions survive restart");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn requests_groups_messages() {
        let dir = tmp("talk");
        let o = Bridge::open(&dir, None).unwrap();
        let bridge = &o.bridge;
        let a = signup(bridge, &o.admin_invite.clone().unwrap());
        open_signup(bridge);
        let b = signup(bridge, "");
        let c = signup(bridge, "");
        // Unknown IDs, disabled requests and real IDs all answer the same way.
        let r_unknown = bridge.req_send(a, "XXXX", "hi").unwrap();
        assert!(r_unknown.contains("\"r\""));
        bridge.me_set(c, &parse(r#"{"acc":false}"#).unwrap()).unwrap();
        let r_off = bridge.req_send(a, &c.to_string(), "").unwrap();
        assert!(r_off.contains("\"r\""));
        assert!(bridge.st.lock().unwrap().accounts[&c].req_in.is_empty());
        // b accepts a's request -> direct conversation.
        bridge.req_send(a, &b.to_string(), "it's me").unwrap();
        let rin = bridge.st.lock().unwrap().accounts[&b].req_in.clone();
        assert_eq!(rin.len(), 1);
        let res = bridge.req_answer(b, &id64(rin[0]), "accept").unwrap();
        let conv = parse(&res).unwrap().s("c").unwrap().to_string();
        bridge.send(a, &conv, "hello b", &[], Some(1)).unwrap();
        let again = bridge.send(a, &conv, "hello b", &[], Some(1)).unwrap();
        let h = bridge.history(b, &conv, None, 50).unwrap();
        assert_eq!(parse(&h).unwrap().a("m").len(), 1, "nonce dedup: {again}");
        assert!(bridge.history(c, &conv, None, 50).is_err());
        // Group: c has requests off and shares nothing with a -> void invite; b joins.
        let g = bridge.group_new(a, "crew", &[Json::Str(b.to_string()), Json::Str(c.to_string())]).unwrap();
        let g = parse(&g).unwrap().s("c").unwrap().to_string();
        assert!(bridge.group_answer(c, &g, true).is_err(), "void invite is invisible");
        bridge.group_answer(b, &g, true).unwrap();
        bridge.send(b, &g, "in", &[], None).unwrap();
        assert_eq!(parse(&bridge.history(a, &g, None, 50).unwrap()).unwrap().a("m").len(), 1);
        let hello = bridge.hello(b).unwrap();
        assert_eq!(parse(&hello).unwrap().a("convs").len(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn quota_rules_receive_refuse_recycle() {
        let dir = tmp("quota");
        let o = Bridge::open(&dir, None).unwrap();
        let bridge = &o.bridge;
        let a = signup(bridge, &o.admin_invite.clone().unwrap());
        open_signup(bridge);
        let b = signup(bridge, "");
        bridge.admin_set(a, &parse(r#"{"set":{"default_quota":1000,"flex_pct":10}}"#).unwrap()).unwrap();
        bridge.req_send(a, &b.to_string(), "").unwrap();
        let r = bridge.st.lock().unwrap().accounts[&b].req_in[0];
        let conv = parse(&bridge.req_answer(b, &id64(r), "accept").unwrap()).unwrap().s("c").unwrap().to_string();
        // Upload helper: real uploads through the store.
        let up = |owner: Aid, data: &[u8]| -> String {
            let u = parse(&bridge.upload_begin(owner, data.len() as u64).unwrap()).unwrap().s("u").unwrap().to_string();
            let mut rd = data;
            let done = bridge.upload_put(owner, &u, 0, data.len() as u64, &mut rd).unwrap();
            parse(&done).unwrap().s("b").unwrap().to_string()
        };
        let att = |b: &str, z: usize| parse(&format!(r#"{{"b":"{b}","n":"f.bin","m":"application/octet-stream","z":{z}}}"#)).unwrap();
        // a sends 3 files of 400 bytes; b (auto-recycle on) receives all, oldest recycled to stay <= 1000.
        for i in 0..3u8 {
            let h = up(a, &[i; 400]);
            if i == 2 {
                // a is at 800/1000 with auto-recycle: sending a third recycles a's oldest.
            }
            bridge.send(a, &conv, "", &[att(&h, 400)], None).unwrap();
        }
        {
            let st = bridge.st.lock().unwrap();
            assert_eq!(st.accounts[&b].usage, 800, "b holds the two newest, recycled the oldest");
            assert_eq!(st.accounts[&a].usage, 800);
        }
        // b turns auto-recycle off and fills up: the next file is refused, text still arrives.
        bridge.me_set(b, &parse(r#"{"ar":false}"#).unwrap()).unwrap();
        let h = up(a, &[9u8; 300]);
        bridge.send(a, &conv, "big one", &[att(&h, 300)], None).unwrap();
        {
            let st = bridge.st.lock().unwrap();
            let hash: Hash = unhex(&h).unwrap().try_into().unwrap();
            assert_eq!(st.accounts[&b].files[&hash].state, FState::Refused);
            assert_eq!(st.accounts[&b].usage, 800);
        }
        // b frees space and receives it after all ("Receive now").
        let files = parse(&bridge.files(b).unwrap()).unwrap();
        let oldest: Vec<Json> = files.a("f").iter().filter(|f| f.s("s") == Some("ok")).take(1).map(|f| Json::Str(f.s("b").unwrap().into())).collect();
        bridge.file_op(b, "delete", &oldest).unwrap();
        bridge.file_op(b, "get", &[Json::Str(h.clone())]).unwrap();
        assert_eq!(bridge.st.lock().unwrap().accounts[&b].usage, 700);
        // Turning auto-recycle back on while over quota schedules a cleanup (R5).
        bridge.admin_set(a, &parse(r#"{"set":{"default_quota":500}}"#).unwrap()).unwrap();
        bridge.me_set(b, &parse(r#"{"ar":true}"#).unwrap()).unwrap();
        assert!(bridge.st.lock().unwrap().accounts[&b].cleanup_at.is_some());
        let n = parse(&bridge.file_op(b, "cleanup", &[]).unwrap()).unwrap().u("n").unwrap();
        assert!(n >= 1);
        let st = bridge.st.lock().unwrap();
        assert!(st.accounts[&b].usage <= 500);
        assert!(st.accounts[&b].cleanup_at.is_none());
        drop(st);
        let _ = fs::remove_dir_all(&dir);
    }
}
