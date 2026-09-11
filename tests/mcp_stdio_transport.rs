mod support;
use std::io::{BufRead, BufReader};
use std::process::Stdio;
use std::time::{Duration, Instant};
use support::mcp::{SharedStdout, Watchdog};

#[test]
fn watchdog_converts_silent_server_into_timeout() {
    use std::process::Command;
    use std::sync::Arc;
    use std::sync::Mutex;

    let mut silent = Command::new("sleep")
        .arg("30")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn sleep");
    let stdout = Arc::new(Mutex::new(SharedStdout(BufReader::new(
        silent.stdout.take().expect("stdout"),
    ))));
    let deadline = Instant::now() + Duration::from_secs(2);
    let wd = Watchdog::start(Some(silent.id()), deadline, Duration::from_secs(2));
    let start = Instant::now();
    let mut line = String::new();
    let n = {
        let mut out = stdout.lock().unwrap();
        out.0.read_line(&mut line).expect("read")
    };
    // sleep never writes: the read must unblock via watchdog SIGKILL -> EOF.
    assert_eq!(n, 0, "expected EOF after watchdog kill, got {n} bytes");
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "watchdog must kill quickly"
    );
    // progress() must NOT have disarmed the watchdog earlier: with a
    // sliding deadline, calling it mid-read cannot extend life past the
    // deadline when no further line arrives. The kill above happened while
    // the watchdog was armed the whole time.
    wd.progress();
    wd.stop();
    let _ = silent.kill();
    let _ = silent.wait();
}
