//! Bounded TCP connect probes: no extra tool, DNS resolution or application data.
use serde_json::{json, Value};
use std::{
    io,
    net::{SocketAddr, TcpStream},
    time::{Duration, Instant},
};

const ATTEMPTS: usize = 4;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const INTERVAL: Duration = Duration::from_secs(1);

pub fn probe(address: SocketAddr) -> Value {
    run(address, |address, timeout| TcpStream::connect_timeout(&address, timeout).map(drop), std::thread::sleep)
}

fn run(address: SocketAddr, mut connect: impl FnMut(SocketAddr, Duration) -> io::Result<()>, mut sleep: impl FnMut(Duration)) -> Value {
    let started = Instant::now();
    let mut samples = Vec::with_capacity(ATTEMPTS);
    let mut latencies = Vec::with_capacity(ATTEMPTS);
    let mut stdout = String::new();
    for sequence in 1..=ATTEMPTS {
        let start = Instant::now();
        let result = connect(address, CONNECT_TIMEOUT);
        let elapsed = start.elapsed().as_secs_f64() * 1000.0;
        match result {
            Ok(()) => {
                latencies.push(elapsed);
                stdout.push_str(&format!("Connected to {address}: seq={sequence} time={elapsed:.3} ms\n"));
                samples.push(json!({"sequence":sequence,"connected":true,"latencyMs":elapsed,"error":null,"timedOut":false}));
            }
            Err(error) => {
                let timed_out = matches!(error.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock);
                stdout.push_str(&format!("Failed to connect to {address}: seq={sequence} error={error}\n"));
                samples
                    .push(json!({"sequence":sequence,"connected":false,"latencyMs":null,"error":error.to_string(),"timedOut":timed_out}));
            }
        }
        if sequence < ATTEMPTS {
            sleep(INTERVAL);
        }
    }
    let received = latencies.len();
    let loss = (ATTEMPTS - received) as f64 * 100.0 / ATTEMPTS as f64;
    let min = latencies.iter().copied().reduce(f64::min);
    let max = latencies.iter().copied().reduce(f64::max);
    let avg = (received > 0).then(|| latencies.iter().sum::<f64>() / received as f64);
    stdout.push_str(&format!("{ATTEMPTS} probes sent, {received} connected, {loss:.0}% loss\n"));
    if let (Some(min), Some(avg), Some(max)) = (min, avg, max) {
        stdout.push_str(&format!("connect min/avg/max = {min:.3}/{avg:.3}/{max:.3} ms\n"));
    }
    json!({"command":"tcping","arguments":[address.ip().to_string(),address.port().to_string()],
        "stdout":stdout,"stderr":"","exitCode":if received > 0 {0} else {1},"timedOut":false,"truncated":false,
        "target":address.ip().to_string(),"port":address.port(),"durationMs":started.elapsed().as_secs_f64()*1000.0,
        "samples":samples,"statistics":{"sent":ATTEMPTS,"received":received,"lossPercent":loss,"minMs":min,"avgMs":avg,"maxMs":max}})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connects_to_a_real_local_listener_and_reports_latency_without_external_tools() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let result = run(address, |address, timeout| TcpStream::connect_timeout(&address, timeout).map(drop), |_| {});
        assert_eq!(result["exitCode"], 0);
        assert_eq!(result["port"], address.port());
        assert_eq!(result["statistics"]["received"], 4);
        assert_eq!(result["statistics"]["lossPercent"], 0.0);
        assert!(result["statistics"]["minMs"].as_f64().unwrap() >= 0.0);
        assert!(result["samples"].as_array().unwrap().iter().all(|s| s["connected"] == true));
    }

    #[test]
    fn partial_success_excludes_failures_from_latency_and_returns_success() {
        let mut attempts = 0;
        let result = run(
            "10.0.0.1:443".parse().unwrap(),
            |_, _| {
                attempts += 1;
                if attempts == 4 {
                    Ok(())
                } else {
                    Err(io::Error::from(io::ErrorKind::ConnectionRefused))
                }
            },
            |_| {},
        );
        assert_eq!(result["exitCode"], 0);
        assert_eq!(result["statistics"]["received"], 1);
        assert_eq!(result["statistics"]["lossPercent"], 75.0);
        assert_eq!(result["statistics"]["minMs"], result["statistics"]["avgMs"]);
        assert_eq!(result["statistics"]["maxMs"], result["samples"][3]["latencyMs"]);
        assert_eq!(result["samples"][0]["latencyMs"], Value::Null);
    }

    #[test]
    fn failures_are_diagnostics_with_bounded_attempts_and_null_latency_without_replies() {
        let address = "10.0.0.1:443".parse().unwrap();
        let mut attempts = 0;
        let mut pauses = 0;
        let result = run(
            address,
            |got, timeout| {
                assert_eq!(got, address);
                assert_eq!(timeout, Duration::from_secs(2));
                attempts += 1;
                Err(io::Error::from(if attempts == 1 { io::ErrorKind::ConnectionRefused } else { io::ErrorKind::TimedOut }))
            },
            |duration| {
                assert_eq!(duration, Duration::from_secs(1));
                pauses += 1;
            },
        );
        assert_eq!((attempts, pauses), (4, 3));
        assert_eq!(result["exitCode"], 1);
        assert_eq!(result["statistics"]["lossPercent"], 100.0);
        assert_eq!(result["statistics"]["avgMs"], Value::Null);
        assert_eq!(result["samples"][0]["timedOut"], false);
        assert_eq!(result["samples"][1]["timedOut"], true);
        assert_eq!(result["timedOut"], false, "individual connect timeouts do not abort the bounded query");
    }
}
