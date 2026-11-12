//! Cross-thread reply latency and operation throughput under local runnable pressure.
//! Run the built binary with positional arguments: tasks local_percent work_iterations seconds.
//! For example: cross_wake_completion 1024 30 200 3.
//! Local operations model synchronous work; remote operations await a reply from
//! another pinned thread. Every response is observed before the next submission
//! yields to ordinary work. Report send-to-receipt and send-to-caller latency
//! separately so a relay cannot conceal delayed caller scheduling.
use glommio::{Latency, LocalExecutorBuilder, Placement, Shares};
use std::{
    cell::Cell,
    hint::black_box,
    rc::Rc,
    sync::mpsc,
    time::{Duration, Instant},
};

fn pin(cpu: usize) {
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_ZERO(&mut set);
        libc::CPU_SET(cpu, &mut set);
        assert_eq!(
            libc::sched_setaffinity(0, std::mem::size_of_val(&set), &set),
            0
        );
    }
}

fn work(seed: u64, iterations: u64) {
    let mut x = black_box(seed);
    for _ in 0..iterations {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
    }
    black_box(x);
}

fn percentile(xs: &mut [u64], p: f64) -> f64 {
    xs.sort_unstable();
    xs[((xs.len() - 1) as f64 * p) as usize] as f64 / 1000.0
}

fn main() {
    let args: Vec<_> = std::env::args().collect();
    let lanes: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(1024);
    let hits: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(30);
    let iterations: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(200);
    let seconds: u64 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(2);
    let mut mask: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { libc::sched_getaffinity(0, std::mem::size_of_val(&mask), &mut mask) },
        0
    );
    let cpus: Vec<_> = (0..libc::CPU_SETSIZE as usize)
        .filter(|&cpu| unsafe { libc::CPU_ISSET(cpu, &mask) })
        .collect();
    assert!(
        cpus.len() >= 2,
        "cross-wake benchmark requires at least two permitted CPUs"
    );
    assert!(lanes > 0 && hits < 100 && seconds > 0);
    let worker_count = (cpus.len() - 1).min(4);
    let mut senders = Vec::new();
    let mut workers = Vec::new();
    for &cpu in &cpus[cpus.len() - worker_count..] {
        let (tx, rx) = mpsc::channel::<tokio::sync::oneshot::Sender<Instant>>();
        senders.push(tx);
        workers.push(std::thread::spawn(move || {
            pin(cpu);
            for reply in rx {
                let _ = reply.send(Instant::now());
            }
        }));
    }
    let ex = LocalExecutorBuilder::new(Placement::Fixed(cpus[0]))
        .make()
        .unwrap();
    let results = ex.run(async {
        let latency = Duration::from_millis(5);
        let queue = glommio::executor().create_task_queue(
            Shares::Static(50),
            Latency::Matters(latency),
            "lanes",
        );
        let start = Instant::now();
        let deadline = start + Duration::from_secs(seconds);
        let barrier = Rc::new(Cell::new(false));
        let mut tasks = Vec::new();
        for id in 0..lanes {
            let tx = senders[id % senders.len()].clone();
            let barrier = barrier.clone();
            tasks.push(
                glommio::spawn_local_into(
                    async move {
                        while !barrier.get() {
                            glommio::executor().yield_now().await;
                        }
                        let mut completed = 0u64;
                        let mut offloaded = 0u64;
                        let mut receipt = Vec::new();
                        let mut returned = Vec::new();
                        let mut random = (id as u64 + 1) * 1337;
                        while Instant::now() < deadline {
                            random = random
                                .wrapping_mul(6364136223846793005)
                                .wrapping_add(1442695040888963407);
                            work(random, iterations);
                            let miss = (random >> 32) % 100 >= hits;
                            if miss {
                                offloaded += 1;
                                let (reply, response) = tokio::sync::oneshot::channel();
                                tx.send(reply).unwrap();
                                let sent = response.await.unwrap();
                                let noticed = Instant::now();
                                let end = Instant::now();
                                if offloaded.is_multiple_of(16) {
                                    receipt.push(noticed.duration_since(sent).as_nanos() as u64);
                                    returned.push(end.duration_since(sent).as_nanos() as u64);
                                }
                            }
                            if miss {
                                glommio::executor().yield_now().await;
                            }
                            completed += 1;
                            if completed.is_multiple_of(64) {
                                glommio::executor().yield_if_needed().await;
                                glommio::executor().yield_now().await;
                            }
                        }
                        (completed, offloaded, receipt, returned)
                    },
                    queue,
                )
                .unwrap(),
            );
        }
        barrier.set(true);
        let mut total = 0;
        let mut misses = 0;
        let mut receipt = Vec::new();
        let mut returned = Vec::new();
        for task in tasks {
            let (count, offloaded, mut a, mut b) = task.await;
            total += count;
            misses += offloaded;
            receipt.append(&mut a);
            returned.append(&mut b);
        }
        (
            total,
            misses,
            receipt,
            returned,
            start.elapsed().as_secs_f64(),
        )
    });
    drop(senders);
    for worker in workers {
        worker.join().unwrap();
    }
    let (total, misses, mut receipt, mut returned, elapsed) = results;
    assert!(!receipt.is_empty());
    println!("tasks,local_percent,work_iterations,operations_per_sec,cross_wakes_per_sec,receipt_p50_us,receipt_p99_us,return_p50_us,return_p99_us,samples");
    println!(
        "{lanes},{hits},{iterations},{:.0},{:.0},{:.3},{:.3},{:.3},{:.3},{}",
        total as f64 / elapsed,
        misses as f64 / elapsed,
        percentile(&mut receipt, 0.5),
        percentile(&mut receipt, 0.99),
        percentile(&mut returned, 0.5),
        percentile(&mut returned, 0.99),
        receipt.len()
    );
}
