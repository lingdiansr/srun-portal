//! `wait` and `promiseAny` (SPEC §1, §8).

use std::sync::mpsc;
use std::time::Duration;

/// `wait(ms)`: the reference's `setTimeout` wrapper.
pub fn wait_ms(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}

/// One task of a [`promise_any`] race.
pub type Task<T> = Box<dyn FnOnce() -> Result<T, String> + Send + 'static>;

/// `promiseAny(list, timeout)`: the first task to resolve wins; if every task
/// fails or the timeout fires first, the race is rejected with `Timeout`
/// (SPEC §8).
///
/// Rust has no timers to clean up, so the `finally` clause of the reference is
/// structural here. Tasks that have not resolved when the race is decided are
/// abandoned rather than cancelled — they only ever touch their own HTTP
/// response, so a late arrival is dropped with the channel.
pub fn promise_any<T: Send + 'static>(tasks: Vec<Task<T>>, timeout_ms: u64) -> Result<T, String> {
    let pending = tasks.len();
    if pending == 0 {
        return Err(TIMEOUT.to_string());
    }
    let (tx, rx) = mpsc::channel::<Result<T, String>>();
    for task in tasks {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(task());
        });
    }
    drop(tx);

    let mut failures = 0usize;
    loop {
        match rx.recv_timeout(Duration::from_millis(timeout_ms)) {
            Ok(Ok(value)) => return Ok(value),
            Ok(Err(_)) => {
                failures += 1;
                if failures >= pending {
                    return Err(TIMEOUT.to_string());
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => return Err(TIMEOUT.to_string()),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(TIMEOUT.to_string());
            }
        }
    }
}

/// The rejection string of a timed-out race (SPEC §11 `PortalError::Timeout`).
pub const TIMEOUT: &str = "Timeout";

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn first_success_wins_and_orders_nothing() {
        let tasks: Vec<Task<u32>> = vec![
            Box::new(|| {
                wait_ms(200);
                Ok(7)
            }),
            Box::new(|| Ok(9)),
        ];
        assert_eq!(promise_any(tasks, 3_000), Ok(9));
    }

    #[test]
    fn all_failing_rejects_with_timeout() {
        let tasks: Vec<Task<u32>> = vec![
            Box::new(|| Err("a".to_string())),
            Box::new(|| Err("b".to_string())),
        ];
        assert_eq!(promise_any(tasks, 3_000), Err("Timeout".to_string()));
    }

    #[test]
    fn slow_task_is_cut_off_by_the_timeout() {
        let tasks: Vec<Task<u32>> = vec![Box::new(|| {
            wait_ms(5_000);
            Ok(1)
        })];
        let started = Instant::now();
        assert_eq!(promise_any(tasks, 100), Err("Timeout".to_string()));
        assert!(started.elapsed().as_millis() < 3_000);
    }

    #[test]
    fn empty_race_times_out() {
        let tasks: Vec<Task<u32>> = Vec::new();
        assert_eq!(promise_any(tasks, 50), Err("Timeout".to_string()));
    }
}
