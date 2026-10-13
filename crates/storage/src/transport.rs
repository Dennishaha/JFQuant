//! System curl transport with cooperative cancellation and child reaping.
use analytics_core::task::TaskContext;
use std::{
    io::{self, Read},
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};
/// Execute curl without a shell. Pipes are drained while the worker polls cancellation.
pub fn curl(command: &mut Command) -> io::Result<Output> {
    let context = TaskContext::current();
    context.check()?;
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    fn read(mut stream: impl Read) -> io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes)?;
        Ok(bytes)
    }
    let out = thread::spawn(move || read(stdout));
    let err = thread::spawn(move || read(stderr));
    let mut last = Instant::now();
    let mut cancelled = false;
    let status = loop {
        if context.check().is_err() {
            cancelled = true;
            let _ = child.kill();
            break child.wait();
        }
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => {}
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(e);
            }
        }
        if last.elapsed() >= Duration::from_secs(5) {
            let _ = context.report("正在传输数据…");
            last = Instant::now();
        }
        thread::sleep(Duration::from_millis(50));
    };
    let stdout = out
        .join()
        .map_err(|_| io::Error::other("stdout reader panicked"))??;
    let stderr = err
        .join()
        .map_err(|_| io::Error::other("stderr reader panicked"))??;
    if cancelled {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "task cancelled"));
    }
    Ok(Output {
        status: status?,
        stdout,
        stderr,
    })
}
/// Retrieve a bounded-time public JSON endpoint.
pub fn json(url: &str) -> io::Result<serde_json::Value> {
    let output = curl(Command::new("/usr/bin/curl").args([
        "--fail",
        "--silent",
        "--show-error",
        "--location",
        "--connect-timeout",
        "15",
        "--max-time",
        "60",
        "--retry",
        "2",
        "--user-agent",
        "JFQuant/0.1",
        url,
    ]))?;
    if !output.status.success() {
        return Err(io::Error::other(
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ));
    }
    serde_json::from_slice(&output.stdout).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::Write,
        net::TcpListener,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
    };
    #[test]
    fn local_http_errors_invalid_json_and_cancellation_are_reported() {
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = server.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = server.accept().unwrap();
            let mut b = [0; 4096];
            let _ = stream.read(&mut b);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\ninvalid",
                )
                .unwrap();
        });
        assert!(json(&format!("http://{address}/")).is_err());
        handle.join().unwrap();
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = server.local_addr().unwrap();
        let flag = Arc::new(AtomicBool::new(false));
        let cancel = flag.clone();
        let handle = thread::spawn(move || {
            let (mut stream, _) = server.accept().unwrap();
            let mut b = [0; 4096];
            let _ = stream.read(&mut b);
            cancel.store(true, Ordering::Relaxed);
            thread::sleep(Duration::from_millis(200));
        });
        let context = TaskContext::new(TaskContext::default().id, flag, |_| {});
        let start = Instant::now();
        let error = context
            .execute(|| json(&format!("http://{address}/")))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(start.elapsed() < Duration::from_secs(2));
        handle.join().unwrap();
        let mut command = Command::new("/usr/bin/curl");
        command.args([
            "--fail",
            "--silent",
            "--connect-timeout",
            "1",
            "--max-time",
            "1",
            "http://127.0.0.1:1",
        ]);
        assert!(!curl(&mut command).unwrap().status.success());
    }
}

#[cfg(test)]
mod deadline_tests {
    use super::*;
    use std::{io::Write, net::TcpListener};
    #[test]
    fn curl_http_failure_and_deadline_do_not_return_successful_payloads() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut b = [0; 4096];
            let _ = s.read(&mut b);
            s.write_all(
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .unwrap();
        });
        assert!(json(&format!("http://{address}/")).is_err());
        server.join().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut b = [0; 4096];
            let _ = s.read(&mut b);
            thread::sleep(Duration::from_millis(1300));
        });
        let output = curl(Command::new("/usr/bin/curl").args([
            "--silent",
            "--show-error",
            "--max-time",
            "1",
            &format!("http://{address}/"),
        ]))
        .unwrap();
        assert_eq!(output.status.code(), Some(28));
        server.join().unwrap();
    }
}
