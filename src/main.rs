use morics::*;
use std::time::Duration;
use std::thread;

fn main() {
    let registry: &'static Registry = Box::leak(Box::new(Registry::new()));

    let metrics_daemon = registry.spawn(Duration::from_secs(1), "http://localhost:8086/write?db=metrics");

    let t = registry.result_timer("rt", vec![]);

    t.record::<_, _, ()>(|| {
        thread::sleep(Duration::from_millis(20));
        Ok(())
    });
    t.record::<_, (), _>(|| {
        thread::sleep(Duration::from_millis(10));
        Err(())
    });

    metrics_daemon.join().unwrap();
}
