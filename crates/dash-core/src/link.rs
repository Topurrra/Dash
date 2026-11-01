//! One owner, others follow. Apps on these engines share one folder; the
//! first to start takes the owner lock and runs the engines (indexing,
//! clipboard capture). The other is a follower: each engine call it makes
//! is sent to the owner over a local connection, and the owner's events
//! come back to it. When the owner quits, the follower takes over.
//!
//! Wire format: one JSON object per line over `127.0.0.1`. Requests carry
//! a token from `owner.json` (readable only by this user), so other local
//! programs can't drive the engines.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};

const LOCK_FILE: &str = "owner.lock";
const OWNER_FILE: &str = "owner.json";
/// Longest request line the owner accepts; longer ones drop the connection.
const MAX_LINE: usize = 1 << 20;
/// How long a new connection may take to send its first (token) line.
const AUTH_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the owner waits on one follower's event write.
const EVENT_WRITE_TIMEOUT: Duration = Duration::from_millis(500);
/// Sent to the app's own listener when this process becomes the owner.
pub const BECAME_OWNER_EVENT: &str = "dash-became-owner";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// `claim` not called (tests, tools): engines run locally.
    Alone,
    Owner,
    Follower,
}

/// Who owns the engines (written by the owner).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OwnerInfo {
    pub app: String,
    pub pid: u32,
    pub port: u16,
    pub token: String,
}

/// Answers an engine call on the owner: `None` when the op isn't this engine's.
pub type Handler = Arc<dyn Fn(&str, Value) -> Option<Result<Value, String>> + Send + Sync>;
/// Receives the owner's events on a follower (and `BECAME_OWNER_EVENT`).
pub type Listener = Arc<dyn Fn(&str, Value) + Send + Sync>;

struct State {
    role: Role,
    dir: PathBuf,
    app: String,
    _lock: Option<File>,
    client: Option<Arc<Client>>,
    subscribers: Vec<Arc<Mutex<TcpStream>>>,
    when_owner: Vec<Box<dyn FnOnce() + Send>>,
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(|| {
        Mutex::new(State {
            role: Role::Alone,
            dir: PathBuf::new(),
            app: String::new(),
            _lock: None,
            client: None,
            subscribers: Vec::new(),
            when_owner: Vec::new(),
        })
    })
}

fn lock_state() -> std::sync::MutexGuard<'static, State> {
    state().lock().unwrap_or_else(|p| p.into_inner())
}

fn handlers() -> &'static Mutex<Vec<Handler>> {
    static HANDLERS: OnceLock<Mutex<Vec<Handler>>> = OnceLock::new();
    HANDLERS.get_or_init(Default::default)
}

static LISTENER: OnceLock<Listener> = OnceLock::new();

/// Lets an engine answer calls when this process owns it.
pub fn register(handler: Handler) {
    handlers().lock().unwrap_or_else(|p| p.into_inner()).push(handler);
}

pub fn role() -> Role {
    lock_state().role
}

pub fn is_follower() -> bool {
    role() == Role::Follower
}

/// Runs `start` now, or when this process becomes the owner.
pub fn when_owner(start: impl FnOnce() + Send + 'static) {
    let mut st = lock_state();
    if st.role == Role::Follower {
        st.when_owner.push(Box::new(start));
    } else {
        drop(st);
        start();
    }
}

/// Becomes the owner of `dir` if nobody is, else a follower of the owner.
/// `prepare` runs each time this process becomes the owner, before any
/// engine starts (e.g. moving old data in).
pub fn claim(dir: &Path, app: &str, prepare: impl Fn() + Send + Sync + 'static, listener: Listener) -> Role {
    let _ = std::fs::create_dir_all(dir);
    let _ = LISTENER.set(listener);
    {
        let mut st = lock_state();
        st.dir = dir.to_path_buf();
        st.app = app.to_string();
    }
    if let Some(lock) = try_lock(dir) {
        prepare();
        become_owner(lock);
        return Role::Owner;
    }
    lock_state().role = Role::Follower;
    connect_follower();
    let dir = dir.to_path_buf();
    let _ = std::thread::Builder::new().name("dash-link-takeover".into()).spawn(move || loop {
        std::thread::sleep(Duration::from_secs(2));
        if let Some(lock) = try_lock(&dir) {
            prepare();
            become_owner(lock);
            let starts = std::mem::take(&mut lock_state().when_owner);
            for start in starts {
                start();
            }
            if let Some(listener) = LISTENER.get() {
                listener(BECAME_OWNER_EVENT, Value::Null);
            }
            break;
        }
    });
    Role::Follower
}

/// Who owns the shared folder now, if anyone.
pub fn owner(dir: &Path) -> Option<OwnerInfo> {
    let info = read_owner_file(dir)?;
    // A lock nobody holds means that owner is gone.
    match try_lock(dir) {
        Some(_) => None,
        None => Some(info),
    }
}

fn try_lock(dir: &Path) -> Option<File> {
    let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(dir.join(LOCK_FILE)).ok()?;
    file.try_lock().ok()?;
    Some(file)
}

fn become_owner(lock: File) {
    let (dir, app) = {
        let st = lock_state();
        (st.dir.clone(), st.app.clone())
    };
    let bound = random_token()
        .and_then(|token| TcpListener::bind(("127.0.0.1", 0)).map(|l| (token, l)).map_err(|e| e.to_string()));
    let (token, listener) = match bound {
        Ok(bound) => bound,
        Err(e) => {
            eprintln!("dash-link: can't listen: {e}");
            let mut st = lock_state();
            st.role = Role::Owner;
            st._lock = Some(lock);
            st.client = None;
            return;
        }
    };
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
    let info = OwnerInfo { app, pid: std::process::id(), port, token: token.clone() };
    let tmp = dir.join(format!("{OWNER_FILE}.tmp"));
    if std::fs::write(&tmp, serde_json::to_vec(&info).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, dir.join(OWNER_FILE));
    }
    {
        let mut st = lock_state();
        st.role = Role::Owner;
        st._lock = Some(lock);
        st.client = None;
    }
    let _ = std::thread::Builder::new().name("dash-link-server".into()).spawn(move || {
        for stream in listener.incoming().flatten() {
            let token = token.clone();
            let _ = std::thread::Builder::new().name("dash-link-conn".into()).spawn(move || serve(stream, &token));
        }
    });
}

#[derive(Serialize, Deserialize)]
struct Request {
    token: String,
    op: String,
    #[serde(default)]
    args: Value,
}

/// Reads one line of at most `max` bytes (newline included). `None` on end
/// of stream, a read error or timeout, or a line that is too long.
fn read_capped_line(reader: &mut impl BufRead, max: usize) -> Option<String> {
    let mut line = String::new();
    let read = reader.by_ref().take(max as u64 + 1).read_line(&mut line).ok()?;
    if read == 0 || read > max {
        return None;
    }
    Some(line)
}

fn serve(stream: TcpStream, token: &str) {
    // Until the peer has shown the token it gets a short read timeout.
    let _ = stream.set_read_timeout(Some(AUTH_TIMEOUT));
    let Ok(mut writer) = stream.try_clone() else { return };
    let mut reader = BufReader::new(stream);
    let mut authenticated = false;
    while let Some(line) = read_capped_line(&mut reader, MAX_LINE) {
        let Ok(request) = serde_json::from_str::<Request>(&line) else { return };
        if request.token != token {
            return;
        }
        if !authenticated {
            authenticated = true;
            // A follower's call connection idles between calls.
            let _ = reader.get_ref().set_read_timeout(None);
        }
        if request.op == "subscribe" {
            let _ = writer.set_write_timeout(Some(EVENT_WRITE_TIMEOUT));
            lock_state().subscribers.push(Arc::new(Mutex::new(writer)));
            return;
        }
        let answer = dispatch(&request.op, request.args);
        let reply = match answer {
            Ok(value) => json!({ "ok": value }),
            Err(error) => json!({ "err": error }),
        };
        if writeln!(writer, "{reply}").is_err() {
            return;
        }
    }
}

fn dispatch(op: &str, args: Value) -> Result<Value, String> {
    let handlers: Vec<Handler> = handlers().lock().unwrap_or_else(|p| p.into_inner()).clone();
    for handler in handlers {
        if let Some(result) = handler(op, args.clone()) {
            return result;
        }
    }
    Err(format!("Unknown engine call: {op}"))
}

/// Owner: sends an engine event to every follower.
/// Writes happen outside the link lock (each has a timeout), so a stalled
/// follower can't hold up the owner; followers that fail are dropped.
pub fn broadcast(event: &str, payload: &Value) {
    let subscribers = {
        let st = lock_state();
        if st.role != Role::Owner || st.subscribers.is_empty() {
            return;
        }
        st.subscribers.clone()
    };
    let line = json!({ "event": event, "payload": payload }).to_string();
    let failed: Vec<Arc<Mutex<TcpStream>>> = subscribers
        .into_iter()
        .filter(|s| writeln!(s.lock().unwrap_or_else(|p| p.into_inner()), "{line}").is_err())
        .collect();
    if !failed.is_empty() {
        lock_state().subscribers.retain(|s| !failed.iter().any(|f| Arc::ptr_eq(s, f)));
    }
}

/// Follower: runs `op` on the owner. `None` when this process isn't a
/// follower (run it locally).
pub fn forward<T: DeserializeOwned>(op: &str, args: Value) -> Option<Result<T, String>> {
    let client = {
        let st = lock_state();
        if st.role != Role::Follower {
            return None;
        }
        st.client.clone()
    };
    // Missed the owner at start (or lost it): look for it again.
    let Some(client) = client.or_else(|| attach(None)) else {
        return Some(Err("Search and clipboard are changing hands — try again in a moment.".into()));
    };
    let result = match client.call(op, args.clone()) {
        // Never reached the owner: it may have changed since; try the new one.
        Err(CallError::Unreachable(e)) => match attach(Some(&client)) {
            Some(fresh) => fresh.call(op, args),
            None => Err(CallError::Unreachable(e)),
        },
        other => other,
    };
    let result = result.map_err(|e| match e {
        CallError::Unreachable(e) | CallError::Failed(e) => e,
    });
    Some(result.and_then(|v| serde_json::from_value(v).map_err(|e| e.to_string())))
}

/// Why a forwarded call failed.
enum CallError {
    /// The request was never sent (no connection to the owner).
    Unreachable(String),
    /// Sent, but no usable answer came back, or the engine returned an error.
    Failed(String),
}

struct Client {
    info: OwnerInfo,
    conn: Mutex<Option<BufReader<TcpStream>>>,
}

impl Client {
    fn call(&self, op: &str, args: Value) -> Result<Value, CallError> {
        let mut conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        if conn.is_none() {
            let stream = connect(&self.info)
                .map_err(|e| CallError::Unreachable(format!("{} isn't answering ({e})", self.info.app)))?;
            *conn = Some(BufReader::new(stream));
        }
        let result = (|| {
            let reader = conn.as_mut().unwrap();
            let request = Request { token: self.info.token.clone(), op: op.to_string(), args };
            writeln!(reader.get_mut(), "{}", serde_json::to_string(&request).unwrap_or_default())
                .map_err(|e| e.to_string())?;
            let mut line = String::new();
            if reader.read_line(&mut line).map_err(|e| e.to_string())? == 0 {
                return Err("closed".to_string());
            }
            let reply: Value = serde_json::from_str(&line).map_err(|e| e.to_string())?;
            match (reply.get("ok"), reply.get("err")) {
                (_, Some(Value::String(e))) => Ok(Err(e.clone())),
                (Some(v), _) => Ok(Ok(v.clone())),
                _ => Err("bad reply".to_string()),
            }
        })();
        match result {
            Ok(answer) => answer.map_err(CallError::Failed),
            Err(e) => {
                *conn = None;
                Err(CallError::Failed(format!("{} isn't answering ({e})", self.info.app)))
            }
        }
    }
}

fn read_owner_file(dir: &Path) -> Option<OwnerInfo> {
    serde_json::from_slice(&std::fs::read(dir.join(OWNER_FILE)).ok()?).ok()
}

/// Follower: connects to the owner named in `owner.json` and subscribes to
/// its events. `stale` is a client that couldn't reach its owner; then a new
/// client is made only if `owner.json` names a different owner.
fn attach(stale: Option<&Arc<Client>>) -> Option<Arc<Client>> {
    let dir = lock_state().dir.clone();
    let info = read_owner_file(&dir)?;
    if let Some(stale) = stale {
        if stale.info.port == info.port && stale.info.token == info.token {
            return None;
        }
    }
    let stream = connect(&info).ok()?;
    let client = Arc::new(Client { info, conn: Mutex::new(Some(BufReader::new(stream))) });
    {
        let mut st = lock_state();
        if st.role != Role::Follower {
            return None;
        }
        // Another thread attached first: use its client.
        if let Some(current) = &st.client {
            if stale.map_or(true, |s| !Arc::ptr_eq(current, s)) {
                return Some(current.clone());
            }
        }
        st.client = Some(client.clone());
    }
    subscribe(client.clone());
    Some(client)
}

fn connect(info: &OwnerInfo) -> Result<TcpStream, String> {
    let stream = TcpStream::connect_timeout(&([127, 0, 0, 1], info.port).into(), Duration::from_secs(2))
        .map_err(|e| e.to_string())?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(120)));
    let _ = stream.set_nodelay(true);
    Ok(stream)
}

/// Finds the owner (it may still be starting), then keeps an event stream.
/// Later calls retry through `forward` if the owner isn't found here.
fn connect_follower() {
    let found = (0..30).any(|_| {
        attach(None).is_some() || {
            std::thread::sleep(Duration::from_millis(100));
            false
        }
    });
    if !found {
        eprintln!("dash-link: the owner isn't answering yet");
    }
}

/// Keeps `client`'s event stream. When it ends (owner gone or dropped us),
/// the client is forgotten so the next call looks for the owner again.
fn subscribe(client: Arc<Client>) {
    let _ = std::thread::Builder::new().name("dash-link-events".into()).spawn(move || {
        let info = &client.info;
        if let Ok(stream) = connect(info) {
            let _ = stream.set_read_timeout(None);
            if let Ok(mut writer) = stream.try_clone() {
                let request = Request { token: info.token.clone(), op: "subscribe".into(), args: Value::Null };
                if writeln!(writer, "{}", serde_json::to_string(&request).unwrap_or_default()).is_ok() {
                    for line in BufReader::new(stream).lines() {
                        let Ok(line) = line else { break };
                        let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
                        if let (Some(Value::String(event)), Some(listener)) = (message.get("event"), LISTENER.get()) {
                            listener(event, message.get("payload").cloned().unwrap_or(Value::Null));
                        }
                    }
                }
            }
        }
        let mut st = lock_state();
        if st.client.as_ref().is_some_and(|c| Arc::ptr_eq(c, &client)) {
            st.client = None;
        }
    });
}

/// 128 bits from the OS's secure random source.
fn random_token() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| format!("no secure random source: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn request_lines_are_capped() {
        let mut ok = std::io::Cursor::new(b"{\"a\":1}\nnext\n".to_vec());
        assert_eq!(read_capped_line(&mut ok, 16).as_deref(), Some("{\"a\":1}\n"));
        assert_eq!(read_capped_line(&mut ok, 16).as_deref(), Some("next\n"));
        assert_eq!(read_capped_line(&mut ok, 16), None, "end of stream");

        let mut endless = std::io::Cursor::new(vec![b'a'; 100]);
        assert_eq!(read_capped_line(&mut endless, 16), None, "too long without a newline");
        assert!(endless.position() <= 17, "stops reading at the cap");
    }

    #[test]
    fn server_drops_an_over_long_line() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            serve(stream, "secret");
        });
        let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let chunk = vec![b'x'; 64 * 1024];
        // Stops being accepted once the server has hung up.
        let mut sent = 0;
        while sent <= MAX_LINE * 2 && client.write_all(&chunk).is_ok() {
            sent += chunk.len();
        }
        server.join().unwrap();
        let _ = client.set_read_timeout(Some(Duration::from_secs(5)));
        let mut rest = Vec::new();
        let _ = client.read_to_end(&mut rest);
        assert!(rest.is_empty(), "no reply, connection closed");
    }

    /// Two processes: this test re-runs itself as the owner, then follows it.
    #[test]
    fn follower_calls_owner_hears_events_and_takes_over() {
        if let Ok(dir) = std::env::var("DASH_LINK_OWNER_DIR") {
            register(Arc::new(|op: &str, a: Value| {
                (op == "add").then(|| Ok(json!(a["a"].as_i64().unwrap_or(0) + a["b"].as_i64().unwrap_or(0))))
            }));
            assert_eq!(claim(Path::new(&dir), "owner", || {}, Arc::new(|_: &str, _: Value| {})), Role::Owner);
            for _ in 0..300 {
                broadcast("tick", &json!(1));
                std::thread::sleep(Duration::from_millis(100));
            }
            return;
        }
        let dir = std::env::temp_dir().join(format!("dash-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "link::tests::follower_calls_owner_hears_events_and_takes_over", "--nocapture"])
            .env("DASH_LINK_OWNER_DIR", &dir)
            .spawn()
            .unwrap();
        let wait = |what: &dyn Fn() -> bool| (0..100).any(|_| what() || { std::thread::sleep(Duration::from_millis(100)); false });
        assert!(wait(&|| owner(&dir).is_some()), "owner started");

        static TICKED: AtomicBool = AtomicBool::new(false);
        static TOOK_OVER: AtomicBool = AtomicBool::new(false);
        let first = claim(&dir, "follower", || {}, Arc::new(|event: &str, _: Value| {
            match event {
                "tick" => TICKED.store(true, Ordering::SeqCst),
                BECAME_OWNER_EVENT => TOOK_OVER.store(true, Ordering::SeqCst),
                _ => {}
            }
        }));
        assert_eq!(first, Role::Follower);
        assert_eq!(owner(&dir).map(|o| o.app).as_deref(), Some("owner"));
        assert_eq!(forward::<i64>("add", json!({ "a": 2, "b": 3 })), Some(Ok(5)));
        assert!(matches!(forward::<i64>("nope", json!({})), Some(Err(_))));
        assert!(wait(&|| TICKED.load(Ordering::SeqCst)), "owner's events arrive");

        let started = Arc::new(AtomicBool::new(false));
        let flag = started.clone();
        when_owner(move || flag.store(true, Ordering::SeqCst));
        assert!(!started.load(Ordering::SeqCst), "waits while following");

        child.kill().unwrap();
        let _ = child.wait();
        assert!(wait(&|| role() == Role::Owner), "takes over");
        assert!(started.load(Ordering::SeqCst) && TOOK_OVER.load(Ordering::SeqCst));
        assert_eq!(forward::<i64>("add", json!({})), None, "owner runs calls itself");
        assert_eq!(owner(&dir).map(|o| o.app).as_deref(), Some("follower"));
    }
}
