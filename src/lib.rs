use crossbeam::atomic::AtomicCell;
use futures::Future;
use histogram::Histogram;
use std::collections::HashMap;
use std::fmt::Write;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime};
use ureq;

/// How to flush the values from a metric and make it ready for a new period.
trait ToValues {
    fn flush_values(&self) -> Vec<(&'static str, f64)>;
}

/// Can be cloned and shared between threads while still counting the
/// same counter.
#[derive(Clone)]
pub struct Counter {
    inner: Arc<AtomicCell<usize>>,
}

impl Counter {
    fn new() -> Self {
        Self {
            inner: Arc::new(AtomicCell::new(0)),
        }
    }

    fn reset(&self) {
        self.inner.store(0);
    }

    pub fn inc_by(&self, val: usize) {
        self.inner.fetch_add(val);
    }

    pub fn inc(&self) {
        self.inc_by(1);
    }
}

impl ToValues for Counter {
    fn flush_values(&self) -> Vec<(&'static str, f64)> {
        let count = self.inner.load();
        self.reset();
        vec![("count", count as f64)]
    }
}

/// Gauge
#[derive(Clone)]
pub struct Gauge {
    inner: Arc<AtomicCell<f64>>,
}

impl Gauge {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(AtomicCell::new(0.0)),
        }
    }

    pub fn set(&self, value: f64) {
        self.inner.store(value);
    }
}

impl ToValues for Gauge {
    fn flush_values(&self) -> Vec<(&'static str, f64)> {
        vec![("value", self.inner.load())]
    }
}

/// Timer
#[derive(Clone)]
pub struct Timer {
    inner: Arc<RwLock<Histogram>>,
}

impl Timer {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(Histogram::new())),
        }
    }

    pub fn time(&self, start: Instant) {
        (*self.inner.write().unwrap())
            .increment(start.elapsed().as_millis() as u64)
            .unwrap();
    }

    pub fn record<F: FnOnce() -> A, A>(&self, f: F) -> A {
        let start = Instant::now();
        let result = f();
        self.time(start);
        result
    }
}

impl ToValues for Timer {
    fn flush_values(&self) -> Vec<(&'static str, f64)> {
        let mut histogram = self.inner.write().unwrap();
        let mut result = vec![("count", histogram.entries() as f64)];
        if let Ok(min) = histogram.minimum() {
            result.push(("min", min as f64));
        }
        if let Ok(max) = histogram.maximum() {
            result.push(("max", max as f64));
        }
        if let Ok(mean) = histogram.mean() {
            result.push(("mean", mean as f64));
        }
        if let Ok(p50) = histogram.percentile(50.0) {
            result.push(("50", p50 as f64));
        }
        if let Ok(p95) = histogram.percentile(95.0) {
            result.push(("95", p95 as f64));
        }
        if let Ok(p99) = histogram.percentile(99.0) {
            result.push(("99", p99 as f64));
        }
        histogram.clear();
        result
    }
}

/// ResultTimer
#[derive(Clone)]
pub struct ResultTimer {
    success: Counter,
    failure: Counter,
    timer: Timer,
}

impl ResultTimer {
    pub fn new() -> Self {
        Self {
            success: Counter::new(),
            failure: Counter::new(),
            timer: Timer::new(),
        }
    }

    pub fn record_fut<'a, A, E, F>(&'a self, f: F) -> Box<dyn Future<Item = A, Error = E> + 'a>
    where
        F: FnOnce() -> Box<dyn Future<Item = A, Error = E> + 'a>,
        E: 'a,
        A: 'a,
    {
        let start = Instant::now();
        Box::new(
            f().map_err(move |e| {
                self.timer.time(start);
                self.failure.inc();
                e
            })
            .map(move |a| {
                self.timer.time(start);
                self.success.inc();
                a
            }),
        )
    }

    pub fn record<F: FnOnce() -> Result<A, E>, A, E>(&self, f: F) -> Result<A, E> {
        let result = self.timer.record(f);
        if result.is_ok() {
            self.success.inc();
        } else {
            self.failure.inc();
        }
        result
    }
}

impl ToValues for ResultTimer {
    fn flush_values(&self) -> Vec<(&'static str, f64)> {
        let mut result = self.timer.flush_values();
        result.push(("success", self.success.flush_values()[0].1));
        result.push(("failure", self.failure.flush_values()[0].1));
        result
    }
}

/// Manages a set of metrics and sends measurements to InfluxDB in
/// regular intervals.
pub struct Registry {
    inner: RwLock<HashMap<String, Metric>>,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
        }
    }

    fn register<W: 'static + ToValues + Send + Sync + Clone>(
        &self,
        name: &str,
        tags: Vec<(String, String)>,
        widget: W,
    ) -> W {
        let metric = Metric {
            tags,
            widget: Box::new(widget.clone()),
        };
        let mut metrics = self.inner.write().unwrap();
        metrics.insert(name.to_string(), metric);
        widget
    }

    pub fn counter(&self, name: &str, tags: Vec<(String, String)>) -> Counter {
        self.register(name, tags, Counter::new())
    }

    pub fn gauge(&self, name: &str, tags: Vec<(String, String)>) -> Gauge {
        self.register(name, tags, Gauge::new())
    }

    pub fn timer(&self, name: &str, tags: Vec<(String, String)>) -> Timer {
        self.register(name, tags, Timer::new())
    }

    pub fn result_timer(&self, name: &str, tags: Vec<(String, String)>) -> ResultTimer {
        self.register(name, tags, ResultTimer::new())
    }

    pub fn spawn(&'static self, interval: Duration, url: &str) -> thread::JoinHandle<()> {
        let url = url.to_string();
        thread::spawn(move || loop {
            thread::sleep(interval);

            let timestamp = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos();

            let mut body = String::new();

            for (name, metric) in self.inner.read().unwrap().iter() {
                let mut name_tags = name.clone();
                for (k, v) in metric.tags.iter() {
                    write!(name_tags, ",{}={}", k, v).unwrap();
                }
                let values = metric
                    .widget
                    .flush_values()
                    .into_iter()
                    .map(|(k, v)| format!("{}={}", k, v))
                    .collect::<Vec<String>>()
                    .join(",");
                write!(body, "{} {} {}\n", name_tags, values, timestamp).unwrap();
            }

            let resp = ureq::post(&url).send_string(&body);
            if !resp.ok() {
                println!(
                    "Error posting to InfluxDB. Error {}: {}",
                    resp.status(),
                    resp.into_string().unwrap()
                );
            }
        })
    }
}

struct Metric {
    tags: Vec<(String, String)>,
    widget: Box<dyn ToValues + Send + Sync>,
}
