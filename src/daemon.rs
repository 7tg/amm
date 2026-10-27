//! The amm daemon: one per home directory, found through `daemon.lock` ({pid, port, token}).
use crate::config::{DEFAULT_PORT, Env, amm_home, load_limits};
use crate::http::{Activity, AppState, router};
use crate::service::Amm;
use crate::store::Store;
use crate::types::Clock;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use serde::{Deserialize, Serialize};
use std::fs::{DirBuilder, OpenOptions, Permissions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinSet;

pub type DaemonResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

const SWEEP_MS: u64 = 30_000;
const PORT_TRIES: u32 = 20;
/// Connections served at once; past this, new ones are accepted and dropped.
const MAX_CONNS: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lock {
    pub pid: u32,
    pub port: u16,
    pub token: String,
}

pub fn lock_path(home: &Path) -> PathBuf {
    home.join("daemon.lock")
}

pub fn read_lock(home: &Path) -> Option<Lock> {
    serde_json::from_str(&std::fs::read_to_string(lock_path(home)).ok()?).ok()
}

/// Whether the daemon the lock names answers its health check with its own pid. Blocking (1 s timeout).
pub fn is_healthy(lock: &Lock) -> bool {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(1)))
        .http_status_as_error(false)
        .proxy(None)
        .max_redirects(0)
        .build()
        .into();
    let url = format!("http://127.0.0.1:{}/health", lock.port);
    let Ok(mut r) = agent.get(&url).header("x-amm-token", &lock.token).call() else { return false };
    if r.status() != 200 {
        return false;
    }
    let Ok(text) = r.body_mut().with_config().limit(64 * 1024).read_to_string() else { return false };
    serde_json::from_str::<serde_json::Value>(&text).is_ok_and(|v| v["pid"].as_u64() == Some(u64::from(lock.pid)))
}

async fn healthy(lock: Option<Lock>) -> bool {
    let Some(lock) = lock else { return false };
    tokio::task::spawn_blocking(move || is_healthy(&lock)).await.unwrap_or(false)
}

pub struct DaemonOpts {
    pub home: PathBuf,
    /// First port to try; the next 19 are fallbacks. 0 lets the OS pick.
    pub port: u16,
    /// How often leases are swept and idleness checked (default 30 s).
    pub sweep_ms: Option<u64>,
    /// Overrides `Limits::idle_shutdown_ms`.
    pub idle_shutdown_ms: Option<i64>,
}

impl Default for DaemonOpts {
    fn default() -> Self {
        let env: Env = std::env::vars().collect();
        DaemonOpts { home: amm_home(&env), port: DEFAULT_PORT, sweep_ms: None, idle_shutdown_ms: None }
    }
}

pub struct DaemonHandle {
    port: u16,
    shutdown: Arc<watch::Sender<bool>>,
    done: watch::Receiver<bool>,
}

impl DaemonHandle {
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Stops accepting, drops every open connection, and removes the lock if it is still ours.
    pub async fn close(&self) {
        self.shutdown.send_replace(true);
        self.closed().await;
    }

    /// Resolves once the daemon has stopped, whether through `close` or an idle shutdown.
    pub async fn closed(&self) {
        let mut done = self.done.clone();
        let _ = done.wait_for(|d| *d).await;
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Polls the home's lock for a healthy daemon: 5 tries, 200 ms apart.
async fn wait_healthy(home: &Path) -> bool {
    for _ in 0..5 {
        if healthy(read_lock(home)).await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    false
}

fn is_busy(e: &rusqlite::Error) -> bool {
    e.sqlite_error_code().is_some_and(|c| matches!(c, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked))
}

/// Binds the first free port from `start`; `None` if another amm daemon turns out to own this home.
async fn listen(start: u16, home: &Path) -> DaemonResult<Option<TcpListener>> {
    let end = (u32::from(start) + PORT_TRIES).min(65536);
    for port in u32::from(start)..end {
        if let Ok(l) = TcpListener::bind(("127.0.0.1", port as u16)).await {
            return Ok(Some(l));
        }
        // The port holder may be an amm daemon that has not written its lock yet.
        if wait_healthy(home).await {
            return Ok(None);
        }
    }
    Err(format!("amm: no free port in {start}-{}", end - 1).into())
}

/// Writes the lock through a private temp file and a rename, so readers never see a partial lock.
fn write_lock(home: &Path, lock: &Lock) -> std::io::Result<()> {
    let tmp = home.join(format!("daemon.lock.{}.tmp", lock.pid));
    let mut f = OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
    f.set_permissions(Permissions::from_mode(0o600))?;
    f.write_all(serde_json::to_string(lock).map_err(std::io::Error::other)?.as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, lock_path(home))
}

/// Starts the daemon for `opts.home`. `Ok(None)` when a healthy daemon already owns that home.
pub async fn run_daemon(opts: DaemonOpts) -> DaemonResult<Option<DaemonHandle>> {
    let home = opts.home;
    DirBuilder::new().recursive(true).mode(0o700).create(&home)?;
    std::fs::set_permissions(&home, Permissions::from_mode(0o700))?;
    if healthy(read_lock(&home)).await {
        return Ok(None);
    }
    let env: Env = std::env::vars().collect();
    let limits = load_limits(&env);
    let idle_ms = opts.idle_shutdown_ms.unwrap_or(limits.idle_shutdown_ms);
    let db = home.join("amm.db");
    let store = match tokio::task::spawn_blocking(move || Store::open(&db)).await? {
        Ok(s) => s,
        // Another daemon starting for this home holds the db: if it comes up healthy, step aside quietly.
        Err(e) if is_busy(&e) => {
            if wait_healthy(&home).await {
                return Ok(None);
            }
            return Err(e.into());
        }
        Err(e) => return Err(e.into()),
    };
    let clock: Clock = Arc::new(now_ms);
    let amm = Arc::new(Mutex::new(Amm::new(store, limits, clock, env)));
    let token = hex(&rand::random::<[u8; 16]>());

    let Some(listener) = listen(opts.port, &home).await? else { return Ok(None) };
    let port = listener.local_addr()?.port();
    let pid = std::process::id();
    write_lock(&home, &Lock { pid, port, token: token.clone() })?;

    let activity = Arc::new(Activity::default());
    let state = AppState {
        amm: amm.clone(),
        token: Arc::from(token),
        port,
        activity: activity.clone(),
    };
    let app = router(state);

    let (shutdown, shutdown_rx) = watch::channel(false);
    let shutdown = Arc::new(shutdown);
    let (done_tx, done) = watch::channel(false);

    let sweeper = {
        let shutdown = shutdown.clone();
        let period = Duration::from_millis(opts.sweep_ms.unwrap_or(SWEEP_MS).max(1));
        tokio::spawn(async move {
            let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
            loop {
                tick.tick().await;
                let (amm, activity) = (amm.clone(), activity.clone());
                // Sweeping writes to sqlite: keep it off the async workers. A panic surfaces as a JoinError.
                let shared = amm.clone();
                let idle = tokio::task::spawn_blocking(move || {
                    let mut a = shared.lock().unwrap_or_else(|e| e.into_inner());
                    let idle = match a.sweep() {
                        Err(e) => {
                            eprintln!("amm: sweep failed: {e}");
                            false
                        }
                        Ok(()) => {
                            activity.idle_ms().is_some_and(|ms| ms > idle_ms)
                                && a.active_workers(None).is_empty()
                                && a.merging.is_empty()
                        }
                    };
                    // Resumes merge queues that stalled or were cut off by a restart.
                    let runs = a.runs_to_integrate();
                    (idle && runs.is_empty(), runs)
                })
                .await;
                match idle {
                    Ok((idle, runs)) => {
                        for run_id in runs {
                            tokio::spawn(crate::integration::integrate(amm.clone(), run_id));
                        }
                        if idle {
                            shutdown.send_replace(true);
                            return;
                        }
                    }
                    Err(e) => eprintln!("amm: sweep failed: {e}"),
                }
            }
        })
    };

    let home2 = home.clone();
    tokio::spawn(async move {
        serve(listener, app, shutdown_rx).await;
        sweeper.abort();
        let _ = sweeper.await;
        if read_lock(&home2).is_some_and(|l| l.pid == pid && l.port == port) {
            let _ = std::fs::remove_file(lock_path(&home2));
        }
        done_tx.send_replace(true);
    });

    Ok(Some(DaemonHandle { port, shutdown, done }))
}

/// Accepts connections until shutdown, then drops every connection still open (in-flight requests included).
async fn serve(listener: TcpListener, app: axum::Router, mut shutdown: watch::Receiver<bool>) {
    let mut conns = JoinSet::new();
    let permits = Arc::new(Semaphore::new(MAX_CONNS));
    loop {
        while conns.try_join_next().is_some() {}
        let stream = tokio::select! {
            () = stopped(&mut shutdown) => break,
            r = listener.accept() => match r {
                Ok((stream, _)) => stream,
                Err(e) => {
                    // Out of fds and the like: back off instead of spinning.
                    eprintln!("amm: accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            },
        };
        let Ok(permit) = permits.clone().try_acquire_owned() else {
            drop(stream);
            continue;
        };
        let svc = TowerToHyperService::new(app.clone());
        conns.spawn(async move {
            let _permit = permit;
            let conn = hyper::server::conn::http1::Builder::new()
                .timer(TokioTimer::new())
                .serve_connection(TokioIo::new(stream), svc);
            let _ = conn.await;
        });
    }
    drop(listener);
    conns.shutdown().await;
}

/// Resolves once shutdown is requested (or its sender is gone).
async fn stopped(rx: &mut watch::Receiver<bool>) {
    let _ = rx.wait_for(|s| *s).await;
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
