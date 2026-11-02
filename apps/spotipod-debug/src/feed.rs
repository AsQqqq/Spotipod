//! Connects to SpotiPod's debug feed (JSON lines, see `spotipod_core::telemetry`), keeps
//! reconnecting, and samples the SpotiPod process (CPU, memory).

use std::{
    collections::VecDeque,
    io::{BufRead, BufReader},
    net::{SocketAddr, TcpStream},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde_json::Value;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

/// Must match `spotipod_core::telemetry::PORT`.
pub const PORT: u16 = 47_823;
const RETRY: Duration = Duration::from_secs(1);
const SAMPLE_EVERY: Duration = Duration::from_millis(500);
const MAX_REQUESTS: usize = 20_000;
const MAX_LOGS: usize = 20_000;
const MAX_SAMPLES: usize = 7_200;

pub struct Session {
    pub pid: u32,
    pub version: String,
    /// Unix ms.
    pub started_at: u64,
}

impl Session {
    /// Seconds since SpotiPod started (the time axis of every chart).
    pub fn now(&self) -> f64 {
        (unix_ms().saturating_sub(self.started_at)) as f64 / 1000.0
    }
}

pub struct Request {
    pub t: f64,
    pub endpoint: String,
    pub ms: u64,
    pub ok: bool,
    pub bytes: usize,
    pub items: usize,
    pub error: Option<String>,
}

pub struct Load {
    pub t: f64,
    pub stage: String,
    pub detail: String,
    pub items: usize,
    pub ms: u64,
}

pub struct Resolve {
    pub t: f64,
    pub requested: usize,
    pub cached: usize,
    pub fetched: usize,
    pub failed: usize,
    pub ms: u64,
}

pub struct Log {
    pub t: f64,
    pub level: String,
    pub target: String,
    pub message: String,
}

pub struct Sample {
    pub t: f64,
    /// Percent of one core.
    pub cpu: f32,
    pub memory_mb: f64,
}

#[derive(Default)]
pub struct Model {
    pub connected: bool,
    /// Last SpotiPod seen (kept after it stops).
    pub session: Option<Session>,
    pub requests: VecDeque<Request>,
    pub loads: Vec<Load>,
    pub resolves: Vec<Resolve>,
    pub logs: VecDeque<Log>,
    pub samples: VecDeque<Sample>,
    pub state: Option<Value>,
}

impl Model {
    pub fn clear(&mut self) {
        self.requests.clear();
        self.loads.clear();
        self.resolves.clear();
        self.logs.clear();
        self.samples.clear();
    }

    fn apply(&mut self, v: Value) {
        let t = v["t"].as_f64().unwrap_or(0.0) / 1000.0;
        let s = |k: &str| v[k].as_str().unwrap_or_default().to_owned();
        let n = |k: &str| v[k].as_u64().unwrap_or(0);
        match v["kind"].as_str() {
            Some("hello") => {
                let pid = n("pid") as u32;
                // Every connection replays the backlog: start over, keep CPU history of
                // the same process.
                let same = self.session.as_ref().is_some_and(|x| x.pid == pid);
                let samples = std::mem::take(&mut self.samples);
                self.clear();
                if same {
                    self.samples = samples;
                }
                self.state = None;
                self.session = Some(Session {
                    pid,
                    version: s("version"),
                    started_at: n("started_at"),
                });
            }
            Some("request") => push(
                &mut self.requests,
                MAX_REQUESTS,
                Request {
                    t,
                    endpoint: s("endpoint"),
                    ms: n("ms"),
                    ok: v["ok"].as_bool().unwrap_or(false),
                    bytes: n("bytes") as usize,
                    items: n("items") as usize,
                    error: v["error"].as_str().map(str::to_owned),
                },
            ),
            Some("load") => self.loads.push(Load {
                t,
                stage: s("stage"),
                detail: s("detail"),
                items: n("items") as usize,
                ms: n("ms"),
            }),
            Some("resolve") => self.resolves.push(Resolve {
                t,
                requested: n("requested") as usize,
                cached: n("cached") as usize,
                fetched: n("fetched") as usize,
                failed: n("failed") as usize,
                ms: n("ms"),
            }),
            Some("log") => push(
                &mut self.logs,
                MAX_LOGS,
                Log {
                    t,
                    level: s("level"),
                    target: s("target"),
                    message: s("message"),
                },
            ),
            Some("state") => self.state = Some(v["value"].clone()),
            _ => {}
        }
    }
}

fn push<T>(q: &mut VecDeque<T>, max: usize, item: T) {
    if q.len() == max {
        q.pop_front();
    }
    q.push_back(item);
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

pub type Shared = Arc<Mutex<Model>>;

/// Starts the feed reader and the process sampler; `repaint` is called on new data.
pub fn start(model: Shared, repaint: impl Fn() + Send + Sync + 'static) {
    let repaint = Arc::new(repaint);
    {
        let model = model.clone();
        let repaint = repaint.clone();
        thread::spawn(move || read_feed(model, repaint));
    }
    thread::spawn(move || sample(model, repaint));
}

fn read_feed(model: Shared, repaint: Arc<dyn Fn() + Send + Sync>) {
    let addr = SocketAddr::from(([127, 0, 0, 1], PORT));
    loop {
        let Ok(stream) = TcpStream::connect_timeout(&addr, RETRY) else {
            thread::sleep(RETRY);
            continue;
        };
        model.lock().unwrap().connected = true;
        repaint();
        for line in BufReader::new(stream).lines() {
            let Ok(line) = line else { break };
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                model.lock().unwrap().apply(v);
                repaint();
            }
        }
        model.lock().unwrap().connected = false;
        repaint();
        thread::sleep(RETRY);
    }
}

fn sample(model: Shared, repaint: Arc<dyn Fn() + Send + Sync>) {
    let mut sys = match System::new() {
        Ok(sys) => sys,
        Err(e) => {
            eprintln!("process stats unavailable: {e}");
            return;
        }
    };
    loop {
        thread::sleep(SAMPLE_EVERY);
        let pid = {
            let m = model.lock().unwrap();
            match &m.session {
                Some(s) if m.connected => Some((s.pid, s.now())),
                _ => None,
            }
        };
        let Some((pid, t)) = pid else { continue };
        let pid = Pid::from_u32(pid);
        sys.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            true,
            ProcessRefreshKind::nothing().with_cpu().with_memory(),
        );
        let Some(p) = sys.process(pid) else { continue };
        let s = Sample {
            t,
            cpu: p.cpu_usage(),
            memory_mb: p.memory() as f64 / 1_048_576.0,
        };
        push(&mut model.lock().unwrap().samples, MAX_SAMPLES, s);
        repaint();
    }
}
