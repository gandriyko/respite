//! SIGINT stops `respite run` in-process: the request in flight is dropped, its History marker
//! goes, later requests don't run. Alone in its binary: it signals its own process and sets HOME.

use std::process::ExitCode;
use std::time::{Duration, Instant};

#[test]
fn signal_cancels_the_run() {
    let home = tempfile::tempdir().unwrap();
    // SAFETY: the only test in this binary; no other thread reads the environment yet.
    unsafe { std::env::set_var("HOME", home.path()) };

    // Accepts and never answers; counts connections.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut held = vec![];
        for s in listener.incoming() {
            held.push(s);
            tx.send(()).unwrap();
        }
    });
    let file = home.path().join("t.http");
    std::fs::write(&file, format!("GET http://127.0.0.1:{port}/one\n\n###\nGET http://127.0.0.1:{port}/two\n")).unwrap();

    let pid = std::process::id().to_string();
    let kill = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(1));
        std::process::Command::new("kill").args(["-INT", &pid]).status().unwrap();
    });
    let t = Instant::now();
    let args = ["respite", "run", file.to_str().unwrap(), "--output", "json", "--timeout", "30s"];
    let code = respite_cli::main_with(args.iter().map(|s| s.to_string()).collect());
    kill.join().unwrap();

    assert!(t.elapsed() < Duration::from_secs(5), "took {:?}", t.elapsed());
    assert_eq!(code, Some(ExitCode::from(130)));
    assert_eq!(rx.try_iter().count(), 1, "the second request ran");
    let running = home.path().join(".respite/history/running");
    assert!(std::fs::read_dir(&running).map_or(true, |d| d.count() == 0), "History marker left in {running:?}");
}
