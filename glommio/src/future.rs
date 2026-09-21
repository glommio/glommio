// Unless explicitly stated otherwise all files in this repository are licensed
// under the MIT/Apache-2.0 License, at your convenience
//
//! Combinators over futures.

use crate::{timer::Timer, GlommioError, Result};
use futures_lite::FutureExt;
use std::{future::Future, time::Duration};

/// Races a future against a deadline, whatever the future returns.
///
/// Gives back the future's output if it finishes in time, or
/// [`GlommioError::TimedOut`] if the deadline arrives first. The output is
/// handed back untouched, so a future that itself returns a `Result` yields a
/// `Result` inside a `Result` and the caller decides what to do with the two
/// failures. [`timer::timeout`](crate::timer::timeout) is the narrower form
/// that accepts only futures returning glommio's own [`Result`] and flattens
/// them.
///
/// This is the shape `tokio::time::timeout` and `async_std::future::timeout`
/// have, so code arriving from either compiles here.
///
/// # Which side wins at the deadline
///
/// The future is polled before the timer, so one that completes exactly as the
/// deadline arrives reports its value rather than a timeout. A caller that has
/// an answer should be given it.
///
/// # Examples
///
/// ```
/// use glommio::{future::timeout, timer::sleep, LocalExecutor};
/// use std::time::Duration;
///
/// let ex = LocalExecutor::default();
/// ex.run(async {
///     // Any future at all, not only one returning a `Result`.
///     let answer = timeout(Duration::from_secs(10), async { 42 }).await;
///     assert_eq!(answer.unwrap(), 42);
///
///     let slow = timeout(Duration::from_millis(1), async {
///         sleep(Duration::from_secs(60)).await;
///     })
///     .await;
///     assert!(slow.is_err());
/// });
/// ```
pub async fn timeout<F, T>(dur: Duration, future: F) -> Result<T>
where
    F: Future<Output = T>,
{
    async move { Ok(future.await) }
        .or(async move {
            Timer::new(dur).await;
            Err(GlommioError::TimedOut(dur))
        })
        .await
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::timer::sleep;
    use std::{cell::Cell, rc::Rc};

    #[test]
    fn any_future_is_accepted() {
        test_executor!(async move {
            assert_eq!(
                timeout(Duration::from_secs(10), async { 42u32 })
                    .await
                    .unwrap(),
                42
            );

            // Somebody else's `Result` comes back whole rather than flattened,
            // so the caller still knows which of the two failures happened.
            let nested: Result<std::result::Result<u32, std::io::Error>> =
                timeout(Duration::from_secs(10), async {
                    Err(std::io::Error::other("theirs"))
                })
                .await;
            assert_eq!(nested.unwrap().unwrap_err().to_string(), "theirs");
        });
    }

    #[test]
    fn the_deadline_is_reported_as_timed_out() {
        test_executor!(async move {
            let dur = Duration::from_millis(10);
            let err = timeout(dur, async move {
                sleep(Duration::from_secs(60)).await;
            })
            .await
            .unwrap_err();

            match err {
                GlommioError::TimedOut(d) => assert_eq!(d, dur),
                other => unreachable!("{other}"),
            }
        });
    }

    /// A future that loses the race is dropped, not left running.
    #[test]
    fn the_losing_future_is_dropped() {
        test_executor!(async move {
            struct Marker(Rc<Cell<bool>>);
            impl Drop for Marker {
                fn drop(&mut self) {
                    self.0.set(true);
                }
            }

            let dropped = Rc::new(Cell::new(false));
            let marker = Marker(dropped.clone());
            let _ = timeout(Duration::from_millis(10), async move {
                sleep(Duration::from_secs(60)).await;
                drop(marker);
            })
            .await;

            assert!(
                dropped.get(),
                "the future that lost the race was not dropped"
            );
        });
    }

    /// A future ready at the deadline reports its value rather than a timeout.
    #[test]
    fn a_ready_future_beats_the_timer() {
        test_executor!(async move {
            assert_eq!(
                timeout(Duration::from_nanos(1), async { 7u32 })
                    .await
                    .unwrap(),
                7
            );
        });
    }
}
