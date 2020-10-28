use crossbeam::sync::ShardedLock;
use futures::Future;
use histogram::Histogram;
use std::any::Any;
use std::collections::HashMap;
use std::fmt::Write;
use std::sync::{
    atomic::{AtomicU64, AtomicUsize, Ordering},
    Arc, RwLock,
};
use std::thread;
use std::time::{Duration, Instant, SystemTime};
use ureq;

/// How to flush the values from a metric and make it ready for a new period.
trait ToValues {
    fn flush_values(&self, interval: &Duration) -> Vec<(&'static str, f64)>;
    fn as_any(&self) -> &dyn Any;
}

#[inline]
fn per_second(interval: &Duration, x: f64) -> f64 {
    let seconds = interval.as_secs() as f64;
    x / seconds
}

/// Can be cloned and shared between threads while still counting the
/// same counter.
#[derive(Clone)]
pub struct Counter {
    inner: Arc<AtomicUsize>,
}

impl Counter {
    fn new() -> Self {
        Self {
            inner: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn reset(&self) {
        self.inner.store(0, Ordering::Relaxed);
    }

    pub fn inc_by(&self, val: usize) {
        self.inner.fetch_add(val, Ordering::Relaxed);
    }

    pub fn inc(&self) {
        self.inc_by(1);
    }
}

impl ToValues for Counter {
    fn flush_values(&self, interval: &Duration) -> Vec<(&'static str, f64)> {
        let count = self.inner.load(Ordering::Relaxed);
        self.reset();
        vec![("count", per_second(interval, count as f64))]
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Gauge
#[derive(Clone)]
pub struct Gauge {
    inner: Arc<AtomicU64>,
}

impl Gauge {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(AtomicU64::new(f64::to_bits(0.0))),
        }
    }

    pub fn set(&self, value: f64) {
        self.inner.store(f64::to_bits(value), Ordering::Relaxed);
    }
}

impl ToValues for Gauge {
    fn flush_values(&self, _interval: &Duration) -> Vec<(&'static str, f64)> {
        vec![("value", f64::from_bits(self.inner.load(Ordering::Relaxed)))]
    }
    fn as_any(&self) -> &dyn Any {
        self
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
    fn flush_values(&self, interval: &Duration) -> Vec<(&'static str, f64)> {
        let mut histogram = self.inner.write().unwrap();
        let mut result = vec![("count", per_second(interval, histogram.entries() as f64))];
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
    fn as_any(&self) -> &dyn Any {
        self
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

    pub fn success(&self, start: Instant) {
        self.success.inc();
        self.timer.time(start);
    }

    pub fn failure(&self, start: Instant) {
        self.failure.inc();
        self.timer.time(start);
    }
}

impl ToValues for ResultTimer {
    fn flush_values(&self, interval: &Duration) -> Vec<(&'static str, f64)> {
        let mut result = self.timer.flush_values(interval);
        result.push(("success", self.success.flush_values(interval)[0].1));
        result.push(("failure", self.failure.flush_values(interval)[0].1));
        result
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// HitRatio
#[derive(Clone)]
pub struct HitRatio {
    hits: Counter,
    misses: Counter,
}

impl HitRatio {
    pub fn new() -> Self {
        Self {
            hits: Counter::new(),
            misses: Counter::new(),
        }
    }

    pub fn hit(&self) {
        self.hits.inc();
    }

    pub fn miss(&self) {
        self.misses.inc();
    }
}

impl ToValues for HitRatio {
    fn flush_values(&self, interval: &Duration) -> Vec<(&'static str, f64)> {
        let mut result = vec![];
        result.push(("hits", self.hits.flush_values(interval)[0].1));
        result.push(("misses", self.misses.flush_values(interval)[0].1));
        result
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Manages a set of metrics and sends measurements to InfluxDB in
/// regular intervals.
pub struct Registry {
    inner: ShardedLock<HashMap<Metric, Box<dyn ToValues + Sync + Send>>>,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            inner: ShardedLock::new(HashMap::new()),
        }
    }

    fn register<W: 'static + ToValues + Send + Sync + Clone>(
        &self,
        name: &str,
        tags: Vec<(&str, &str)>,
        widget: W,
    ) -> W {
        let metric = Metric {
            name: name.to_string(),
            tags: tags
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<Vec<_>>(),
        };
        let existing = self.inner.read().unwrap().get(&metric).map(|w| {
            w.as_any()
                .downcast_ref::<W>()
                .expect("Tried to store diffferent widgets under same name/tags.")
                .clone()
        });
        match existing {
            Some(w) => w,
            None => {
                let mut metrics = self.inner.write().unwrap();
                metrics.insert(metric, Box::new(widget.clone()));
                widget
            }
        }
    }

    pub fn counter(&self, name: &str, tags: Vec<(&str, &str)>) -> Counter {
        self.register(name, tags, Counter::new())
    }

    pub fn gauge(&self, name: &str, tags: Vec<(&str, &str)>) -> Gauge {
        self.register(name, tags, Gauge::new())
    }

    pub fn timer(&self, name: &str, tags: Vec<(&str, &str)>) -> Timer {
        self.register(name, tags, Timer::new())
    }

    pub fn result_timer(&self, name: &str, tags: Vec<(&str, &str)>) -> ResultTimer {
        self.register(name, tags, ResultTimer::new())
    }

    pub fn hit_ratio(&self, name: &str, tags: Vec<(&str, &str)>) -> HitRatio {
        self.register(name, tags, HitRatio::new())
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

            for (metric, widget) in self.inner.read().unwrap().iter() {
                let mut name_tags = metric.name.clone();
                for (k, v) in metric.tags.iter() {
                    write!(name_tags, ",{}={}", k, v).unwrap();
                }
                let values = widget
                    .flush_values(&interval)
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

#[derive(PartialEq, Eq, Hash)]
struct Metric {
    name: String,
    tags: Vec<(String, String)>,
}
