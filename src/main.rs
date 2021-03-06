use morics::*;
use std::time::Duration;
use std::thread;

fn main() {
    let registry: &'static Registry = Box::leak(Box::new(Registry::new()));

    let metrics_daemon = registry.spawn(Duration::from_secs(1), "http://localhost:8086/write?db=metrics");

    // let c = registry.counter("c", vec![]);
    // let t = registry.result_timer("rt", vec![]);

    // thread::spawn(move || {
    //     loop {
    //         t.record::<_, _, ()>(|| {
    //             Ok(())
    //         }).unwrap();
    //     }
    // });
    // thread::spawn(move || {
    //     loop {
    //         c.inc();
    //     }
    // });

    let registry1 = registry.clone();

    thread::spawn(move || {
        loop {
            registry.counter("c", vec![]).inc();
        }
    });

    thread::spawn(move || {
        loop {
            registry1.counter("c", vec![]).inc();
        }
    });


    metrics_daemon.join().unwrap();
}
