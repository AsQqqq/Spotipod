//! Under-the-hood data for `spotipod-debug`: requests, loading, logs and UI state as JSON
//! lines on `127.0.0.1:PORT`. A recent backlog is replayed to every new client, so the
//! debug window can be opened at any time.

use std::{
    collections::VecDeque,
    sync::{Mutex, OnceLock},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use serde::Serialize;
use serde_json::Value;
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
    sync::broadcast,
};

/// Debug feed port (localhost only).
pub const PORT: u16 = 47_823;
/// Records replayed to a new client.
const BACKLOG: usize = 5_000;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Record {
    /// First line of every connection.
    Hello {
        pid: u32,
        version: &'static str,
        /// Unix time of process start, ms.
        started_at: u64,
    },
    /// One request to Spotify.
    Request {
        endpoint: String,
        ms: u64,
        ok: bool,
        /// Response size.
        bytes: usize,
        /// Items in the response (tracks, URIs, ...), when known.
        items: usize,
        error: Option<String>,
    },
    /// A loading step: boot stages, lists, batches, warm-up.
    Load {
        stage: String,
        detail: String,
        items: usize,
        ms: u64,
    },
    /// Track metadata lookup: cache hits vs. fetched.
    Resolve {
        requested: usize,
        cached: usize,
        fetched: usize,
        failed: usize,
        ms: u64,
    },
    Log {
        level: String,
        target: String,
        message: String,
    },
    /// Latest UI state (not kept in the backlog; the newest is sent on connect).
    State { value: Value },
}

#[derive(Serialize)]
struct Line<'a> {
    /// ms since process start.
    t: u64,
    #[serde(flatten)]
    record: &'a Record,
}

struct Hub {
    start: Instant,
    started_at: u64,
    tx: broadcast::Sender<String>,
    backlog: Mutex<VecDeque<String>>,
    state: Mutex<Option<String>>,
}

fn hub() -> &'static Hub {
    static HUB: OnceLock<Hub> = OnceLock::new();
    HUB.get_or_init(|| Hub {
        start: Instant::now(),
        started_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64),
        tx: broadcast::channel(4_096).0,
        backlog: Mutex::new(VecDeque::with_capacity(BACKLOG)),
        state: Mutex::new(None),
    })
}

pub fn emit(record: Record) {
    let hub = hub();
    let line = Line {
        t: hub.start.elapsed().as_millis() as u64,
        record: &record,
    };
    let Ok(json) = serde_json::to_string(&line) else {
        return;
    };
    if matches!(record, Record::State { .. }) {
        *hub.state.lock().unwrap() = Some(json.clone());
    } else {
        let mut backlog = hub.backlog.lock().unwrap();
        if backlog.len() == BACKLOG {
            backlog.pop_front();
        }
        backlog.push_back(json.clone());
    }
    _ = hub.tx.send(json);
}

pub fn load(stage: &str, detail: impl Into<String>, items: usize, started: Instant) {
    emit(Record::Load {
        stage: stage.into(),
        detail: detail.into(),
        items,
        ms: started.elapsed().as_millis() as u64,
    });
}

pub fn state(value: Value) {
    emit(Record::State { value });
}

/// Times one request; dropped without `ok`/`fail` it records nothing.
pub struct Span {
    endpoint: String,
    start: Instant,
}

pub fn span(endpoint: impl Into<String>) -> Span {
    Span {
        endpoint: endpoint.into(),
        start: Instant::now(),
    }
}

impl Span {
    pub fn ok(self, bytes: usize, items: usize) {
        self.finish(true, bytes, items, None);
    }

    pub fn fail(self, error: &dyn std::fmt::Display) {
        self.finish(false, 0, 0, Some(error.to_string()));
    }

    /// Records a result and passes it through.
    pub fn result<T, E: std::fmt::Display>(
        self,
        r: std::result::Result<T, E>,
        size: impl FnOnce(&T) -> (usize, usize),
    ) -> std::result::Result<T, E> {
        match &r {
            Ok(v) => {
                let (bytes, items) = size(v);
                self.ok(bytes, items);
            }
            Err(e) => self.fail(e),
        }
        r
    }

    fn finish(self, ok: bool, bytes: usize, items: usize, error: Option<String>) {
        emit(Record::Request {
            endpoint: self.endpoint,
            ms: self.start.elapsed().as_millis() as u64,
            ok,
            bytes,
            items,
            error,
        });
    }
}

/// Serves the feed until the runtime stops. A busy port (another SpotiPod) is logged.
pub async fn serve() {
    let listener = match TcpListener::bind(("127.0.0.1", PORT)).await {
        Ok(l) => l,
        Err(e) => {
            log::warn!("debug feed disabled: port {PORT}: {e}");
            return;
        }
    };
    loop {
        match listener.accept().await {
            Ok((stream, _)) => _ = tokio::spawn(client(stream)),
            Err(e) => log::debug!("debug feed accept failed: {e}"),
        }
    }
}

async fn client(mut stream: TcpStream) {
    let hub = hub();
    // Subscribe before the replay so nothing falls in between.
    let mut rx = hub.tx.subscribe();
    let hello = Record::Hello {
        pid: std::process::id(),
        version: env!("CARGO_PKG_VERSION"),
        started_at: hub.started_at,
    };
    let mut head = serde_json::to_string(&Line {
        t: hub.start.elapsed().as_millis() as u64,
        record: &hello,
    })
    .unwrap_or_default();
    head.push('\n');
    for line in hub.backlog.lock().unwrap().iter() {
        head.push_str(line);
        head.push('\n');
    }
    if let Some(state) = hub.state.lock().unwrap().as_ref() {
        head.push_str(state);
        head.push('\n');
    }
    if stream.write_all(head.as_bytes()).await.is_err() {
        return;
    }
    loop {
        let line = match rx.recv().await {
            Ok(line) => line,
            Err(broadcast::error::RecvError::Lagged(n)) => {
                format!(
                    r#"{{"t":0,"kind":"log","level":"WARN","target":"telemetry","message":"debug client lagged, {n} records dropped"}}"#
                )
            }
            Err(broadcast::error::RecvError::Closed) => return,
        };
        if stream.write_all(line.as_bytes()).await.is_err()
            || stream.write_all(b"\n").await.is_err()
        {
            return;
        }
    }
}

/// Forwards log records to `inner` and to the feed.
pub struct TeeLogger<L>(pub L);

impl<L: log::Log> log::Log for TeeLogger<L> {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        self.0.enabled(metadata)
    }

    fn log(&self, record: &log::Record) {
        if !self.0.enabled(record.metadata()) {
            return;
        }
        self.0.log(record);
        emit(Record::Log {
            level: record.level().to_string(),
            target: record.target().to_owned(),
            message: record.args().to_string(),
        });
    }

    fn flush(&self) {
        self.0.flush();
    }
}
