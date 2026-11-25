//! A timer can be built from, moved to and asked for an `Instant`, and a
//! repeating action's generator may keep state between runs.

use glommio::{
    timer::{Timer, TimerActionRepeat},
    LocalExecutorBuilder,
};
use std::time::{Duration, Instant};

#[test]
fn a_timer_is_set_and_read_by_instant() {
    LocalExecutorBuilder::default()
        .spawn(|| async move {
            let when = Instant::now() + Duration::from_millis(10);
            let timer = Timer::at(when);
            assert_eq!(timer.deadline(), Some(when));
            assert_eq!(timer.await, when);

            let mut timer = Timer::new(Duration::MAX);
            assert_eq!(timer.deadline(), None);
            let when = Instant::now() + Duration::from_millis(10);
            timer.reset_at(when);
            assert_eq!(timer.deadline(), Some(when));
            assert_eq!(timer.await, when);
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn a_repeat_generator_may_be_fn_mut() {
    LocalExecutorBuilder::default()
        .spawn(|| async move {
            let mut left = 3;
            TimerActionRepeat::repeat(move || {
                left -= 1;
                let again = left > 0;
                async move { again.then_some(Duration::from_millis(1)) }
            })
            .join()
            .await
            .expect("the repeat action was cancelled");
        })
        .unwrap()
        .join()
        .unwrap();
}
