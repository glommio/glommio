//! What a crowd of tasks waiting on one initialisation costs.
//!
//! The single-caller path is a check and a return, so it is not where a design
//! choice shows up. The queue is: while one task initialises, every other task
//! that reaches the cell parks on the semaphore, and how they are released
//! again is the thing worth measuring.
//!
//! The cell releases waiters by closing the semaphore, which drains its list in
//! one pass. Releasing them one at a time instead costs about 15ns per waiter,
//! so the `waiters` rungs are what catches that being reintroduced. The
//! `permit acquire and drop` rung is the unit that difference is made of.
//!
//! ```bash
//! cargo bench --bench once_cell -- --save-baseline before
//! # change something
//! cargo bench --bench once_cell -- --baseline before
//! ```

mod common;

use common::Glommio;
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use glommio::{spawn_local, sync::OnceCell};
use std::{hint::black_box, rc::Rc};

fn waiters(c: &mut Criterion) {
    let ex = Glommio::default();
    let mut group = c.benchmark_group("once_cell");

    for count in [2usize, 8, 32, 128, 512] {
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::new("waiters", count), &count, |b, &count| {
            b.to_async(&ex).iter(|| async move {
                let cell: Rc<OnceCell<u32>> = Rc::new(OnceCell::new());

                // The initialiser yields, so every other task reaches the cell
                // and parks before the value is published.
                let first = spawn_local({
                    let cell = cell.clone();
                    async move {
                        *cell
                            .get_or_init(|| async {
                                glommio::executor().yield_now().await;
                                7u32
                            })
                            .await
                    }
                });

                let rest: Vec<_> = (0..count - 1)
                    .map(|_| {
                        let cell = cell.clone();
                        spawn_local(async move { *cell.get_or_init(|| async { 9u32 }).await })
                    })
                    .collect();

                black_box(first.await);
                for task in rest {
                    black_box(task.await);
                }
            })
        });
    }

    // For scale: the same crowd against a cell that is already full, which
    // never touches the semaphore at all.
    group.bench_function("already initialised, 512 readers", |b| {
        b.to_async(&ex).iter(|| async {
            let cell: Rc<OnceCell<u32>> = Rc::new(OnceCell::from(7u32));
            let tasks: Vec<_> = (0..512)
                .map(|_| {
                    let cell = cell.clone();
                    spawn_local(async move { *cell.get_or_init(|| async { 9u32 }).await })
                })
                .collect();
            for task in tasks {
                black_box(task.await);
            }
        })
    });

    // The work the close variant removes per waiter: taking a permit and
    // dropping it, which signals the semaphore and walks its waiter list.
    group.bench_function("permit acquire and drop", |b| {
        let sem = glommio::sync::Semaphore::new(1);
        b.to_async(&ex).iter(|| async {
            let permit = sem.acquire_permit(1).await.unwrap();
            black_box(&permit);
        })
    });

    group.finish();
}

criterion_group!(benches, waiters);
criterion_main!(benches);
