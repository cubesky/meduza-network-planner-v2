//! Test double for native tools and daemons (not shipped). Appends its argv to `$MNP_FAKE_TOOL_LOG`.
//! `daemon` keeps running, `crash` exits 3 immediately, anything else exits 0.

use std::io::Write;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let log = std::env::var("MNP_FAKE_TOOL_LOG").ok().or_else(|| {
        // Supervised processes run with a cleared environment; the log path may be passed as last arg.
        args.iter().find_map(|a| a.strip_prefix("--log=").map(str::to_string))
    });
    if let Some(path) = log {
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "{}", args.iter().filter(|a| !a.starts_with("--log=")).cloned().collect::<Vec<_>>().join(" "));
        }
    }
    match args.first().map(String::as_str) {
        Some("wait-for-peer")
            if !args.iter().any(|a| a.strip_prefix("--ready-file=").is_some_and(|p| std::path::Path::new(p).exists())) =>
        {
            std::process::exit(1)
        }
        Some("daemon" | "wait-for-peer") => loop {
            std::thread::sleep(std::time::Duration::from_secs(3600));
        },
        Some("crash") => std::process::exit(3),
        _ => {}
    }
}
