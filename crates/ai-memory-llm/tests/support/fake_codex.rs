use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

fn main() {
    let codex_home = PathBuf::from(std::env::var_os("CODEX_HOME").expect("CODEX_HOME"));
    writeln!(
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(codex_home.join("fake-invocations"))
            .unwrap(),
        "started"
    )
    .unwrap();
    let mode = fs::read_to_string(codex_home.join("fake-mode"))
        .unwrap_or_else(|_| "success".into());
    if mode.trim() == "sleep" {
        thread::sleep(Duration::from_secs(10));
        return;
    }
    if mode.trim() == "exit" {
        return;
    }
    if mode.trim() == "stderr" {
        io::stderr().write_all(&vec![b'x'; 70 * 1024]).unwrap();
        io::stderr().flush().unwrap();
        thread::sleep(Duration::from_secs(10));
        return;
    }
    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();
    let mut stdout = io::stdout();
    let initialize = lines.next().expect("initialize line").expect("initialize");
    assert!(initialize.contains("\"method\":\"initialize\""));
    match mode.trim() {
        "invalid" => writeln!(stdout, "not-json").unwrap(),
        "wrong-id" => writeln!(stdout, "{{\"jsonrpc\":\"2.0\",\"id\":99,\"result\":{{}}}}").unwrap(),
        "oversized" => writeln!(stdout, "{}", "x".repeat(257 * 1024)).unwrap(),
        _ => {
            writeln!(stdout, "{{\"jsonrpc\":\"2.0\",\"method\":\"notice\"}}").unwrap();
            writeln!(stdout, "{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{}}}}").unwrap();
        }
    }
    stdout.flush().unwrap();
    if !matches!(mode.trim(), "success" | "no-change") {
        thread::sleep(Duration::from_secs(10));
        return;
    }
    let initialized = lines.next().expect("initialized line").expect("initialized");
    assert!(initialized.contains("\"method\":\"initialized\""));
    let account_read = lines.next().expect("account/read line").expect("account/read");
    assert!(account_read.contains("\"method\":\"account/read\""));
    assert!(account_read.contains("\"refreshToken\":true"));
    if mode.trim() == "success" {
        let auth_path = codex_home.join("auth.json");
        let auth = fs::read_to_string(&auth_path).unwrap();
        fs::write(auth_path, auth.replace("old-token", "new-token")).unwrap();
    }
    writeln!(stdout, "{{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{{}}}}").unwrap();
    stdout.flush().unwrap();
    thread::sleep(Duration::from_secs(10));
}
