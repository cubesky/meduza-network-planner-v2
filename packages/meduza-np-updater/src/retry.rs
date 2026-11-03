//! Short, bounded per-file retries. The daemon retains its longer backoff after these attempts are exhausted.
use std::time::Duration;

use meduza_np_runtime::jitter::{Jitter, OsJitter};

use crate::meta::MetaError;

pub const MAX_ATTEMPTS: u32 = 4;

#[derive(Debug, Clone)]
pub struct Attempt {
    pub number: u32,
    pub maximum: u32,
    pub retry_after: Option<Duration>,
    pub error: Option<String>,
}

fn transient(error: &MetaError) -> bool {
    if error.code != "fetch-failed" {
        return false;
    }
    let message = &error.message;
    if [
        "exceeds",
        "no downloader available",
        "only plain http(s)",
        "Permission denied",
        "No space left on device",
        "Read-only file system",
        "certificate verification failed",
    ]
    .iter()
    .any(|marker| message.contains(marker))
    {
        return false;
    }
    // Invalid URL/protocol, local I/O, unsupported options and untrusted/missing CA certificates need repair.
    if [1, 3, 23, 26, 48, 60, 77].iter().any(|code| message.contains(&format!("curl: ({code})"))) {
        return false;
    }
    if let Some(status) =
        message.split("returned error: ").nth(1).and_then(|s| s.split_whitespace().find_map(|word| word.parse::<u16>().ok()))
    {
        return matches!(status, 408 | 425 | 429 | 500..=599);
    }
    true
}

pub fn download<T>(transfer: impl FnMut() -> Result<T, MetaError>, notify: impl FnMut(Attempt)) -> Result<T, MetaError> {
    run(transfer, notify, &mut OsJitter::new(), &mut std::thread::sleep)
}

fn run<T>(
    mut transfer: impl FnMut() -> Result<T, MetaError>,
    mut notify: impl FnMut(Attempt),
    jitter: &mut dyn Jitter,
    sleep: &mut dyn FnMut(Duration),
) -> Result<T, MetaError> {
    for number in 1..=MAX_ATTEMPTS {
        notify(Attempt { number, maximum: MAX_ATTEMPTS, retry_after: None, error: None });
        match transfer() {
            Ok(value) => return Ok(value),
            Err(mut error) => {
                if !transient(&error) {
                    return Err(error);
                }
                if number == MAX_ATTEMPTS {
                    error.message = format!("download failed after {MAX_ATTEMPTS} attempts: {}", error.message);
                    return Err(error);
                }
                let base = Duration::from_secs(2u64.pow(number));
                let delay = base + jitter.uniform(base / 5);
                eprintln!("download attempt {number}/{MAX_ATTEMPTS} failed; retrying in {:.1}s: {error}", delay.as_secs_f64());
                notify(Attempt { number, maximum: MAX_ATTEMPTS, retry_after: Some(delay), error: Some(error.to_string()) });
                sleep(delay);
            }
        }
    }
    unreachable!("retry loop always returns")
}

#[cfg(test)]
mod tests {
    use super::*;
    use meduza_np_runtime::jitter::FixedJitter;
    use std::cell::RefCell;

    fn error(message: &str) -> MetaError {
        MetaError { code: "fetch-failed", message: message.into() }
    }

    #[test]
    fn retries_transient_errors_then_recovers_with_bounded_backoff() {
        let calls = RefCell::new(0);
        let mut sleeps = vec![];
        let mut events = vec![];
        let value = run(
            || {
                *calls.borrow_mut() += 1;
                if *calls.borrow() < 4 {
                    Err(error("curl: (56) TLS unexpected EOF"))
                } else {
                    Ok(42)
                }
            },
            |event| events.push(event),
            &mut FixedJitter::new(vec![0.5]),
            &mut |d| sleeps.push(d),
        )
        .unwrap();
        assert_eq!(value, 42);
        assert_eq!(*calls.borrow(), 4);
        assert_eq!(sleeps, vec![Duration::from_millis(2200), Duration::from_millis(4400), Duration::from_millis(8800)]);
        assert_eq!(events.iter().filter(|e| e.retry_after.is_some()).count(), 3);
        assert_eq!(events.last().unwrap().number, 4);
        assert!(events.last().unwrap().error.is_none());
    }

    #[test]
    fn exhaustion_is_finite_and_permanent_errors_do_not_retry() {
        for message in [
            "curl: (28) timeout",
            "curl: (28) Operation too slow. Less than 1 bytes/sec transferred the last 30 seconds",
            "curl: (22) The requested URL returned error: 503",
            "curl: (22) The requested URL returned error: 429",
            "wget: server returned error: HTTP/1.1 503 Service Unavailable",
        ] {
            let mut calls = 0;
            let mut sleeps = 0;
            let result: Result<(), _> = run(
                || {
                    calls += 1;
                    Err(error(message))
                },
                |_| {},
                &mut FixedJitter::new(vec![0.0]),
                &mut |_| sleeps += 1,
            );
            assert!(result.unwrap_err().message.contains("after 4 attempts"));
            assert_eq!(calls, 4);
            assert_eq!(sleeps, 3);
        }
        for message in [
            "curl: (60) certificate verify failed",
            "curl: (23) disk full",
            "download exceeds 20 bytes",
            "curl: (22) The requested URL returned error: 404",
            "wget: server returned error: HTTP/1.1 404 Not Found",
            "wget: Permission denied",
        ] {
            let mut calls = 0;
            let result: Result<(), _> = run(
                || {
                    calls += 1;
                    Err(error(message))
                },
                |_| {},
                &mut FixedJitter::new(vec![0.0]),
                &mut |_| panic!("permanent error slept"),
            );
            assert!(result.is_err());
            assert_eq!(calls, 1);
        }
    }
}
