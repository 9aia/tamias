//! PTY integration tests — `tamias_tui::run()` needs a real
//! terminal (raw mode, alternate screen, crossterm events), so these
//! spawn the `tamias-tui` binary under a pseudo-terminal and drive
//! it with keystrokes. They cover the event loop, the `/` filter,
//! `:q`, and the terminal-setup error arms.

#![allow(clippy::unwrap_used)]

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use tamias_core::StoreRecord;
use tamias_store_local::Store;

/// A temp home: `store/` holding one installed package, plus
/// throwaway XDG/cache/registry dirs so the user's real dirs can't
/// leak in.
struct Fixture {
    _dir: tempfile::TempDir,
    _data: tempfile::TempDir,
    _cache: tempfile::TempDir,
    cwd: PathBuf,
    home: PathBuf,
    data: PathBuf,
    cache: PathBuf,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let data = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();

    // one installed entry — payload + record through the real store
    let store = Store::new(home.join("store"));
    let payload = dir.path().join("pkg");
    std::fs::create_dir_all(&payload).unwrap();
    std::fs::write(
        payload.join("tamias.yaml"),
        "name: widget\nversion: 1.2.3\ntype: files\n",
    )
    .unwrap();
    std::fs::write(payload.join("a.txt"), "a").unwrap();
    store
        .install(
            &payload,
            StoreRecord {
                name: "widget".into(),
                scope: None,
                version: Some("1.2.3".into()),
                kind: Some("files".into()),
                source_ref: "./pkg".into(),
                resolved: "sha256:0123456789abcdef".into(),
                digest: String::new(), // install() fills it
                installed_at: 0,
            },
        )
        .unwrap();

    Fixture {
        cwd: dir.path().to_path_buf(),
        home,
        data: data.path().to_path_buf(),
        cache: cache.path().to_path_buf(),
        _dir: dir,
        _data: data,
        _cache: cache,
    }
}

fn command(f: &Fixture) -> CommandBuilder {
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_tamias-tui"));
    cmd.cwd(&f.cwd);
    cmd.env("TAMIAS_HOME", &f.home);
    cmd.env("TAMIAS_CACHE", f.cache.join("fetch"));
    cmd.env("TAMIAS_REGISTRY", f.data.join("registry"));
    cmd.env("XDG_DATA_HOME", &f.data);
    cmd.env("XDG_CACHE_HOME", &f.cache);
    cmd.env_remove("TAMIAS_CONFIG");
    cmd
}

/// Spawn under a PTY; returns (master, child). The slave is closed
/// in the parent — the child holds its own copies.
fn spawn_pty(cmd: CommandBuilder) -> (Box<dyn MasterPty + Send>, Box<dyn Child + Send + Sync>) {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let child = pair.slave.spawn_command(cmd).unwrap();
    drop(pair.slave);
    (pair.master, child)
}

/// Wait for exit, bounded — a hung TUI fails the test instead of
/// hanging the suite.
fn wait_exit(child: &mut Box<dyn Child + Send + Sync>) -> portable_pty::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(Instant::now() < deadline, "tamias-tui did not exit");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A pty session with a drainer thread: output accumulates in a
/// shared buffer and Device Status Report queries (`ESC[6n`) get an
/// automatic cursor position reply — a real terminal answers these,
/// a bare pty doesn't.
struct Drained {
    input: Arc<Mutex<Box<dyn Write + Send>>>,
    output: Arc<Mutex<Vec<u8>>>,
}

fn drain(master: &mut Box<dyn MasterPty + Send>) -> Drained {
    let mut reader = master.try_clone_reader().unwrap();
    let writer = master.take_writer().unwrap();
    let d = Drained {
        input: Arc::new(Mutex::new(writer)),
        output: Arc::new(Mutex::new(Vec::new())),
    };
    let (input, output) = (d.input.clone(), d.output.clone());
    std::thread::spawn(move || {
        let mut buf = [0u8; 512];
        let mut tail: Vec<u8> = Vec::new();
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    output.lock().unwrap().extend_from_slice(&buf[..n]);
                    tail.extend_from_slice(&buf[..n]);
                    if tail.len() > 16 {
                        tail.drain(..tail.len() - 16);
                    }
                    if tail.ends_with(b"\x1b[6n") {
                        let _ = input.lock().unwrap().write_all(b"\x1b[1;1R");
                        tail.clear();
                    }
                }
            }
        }
    });
    d
}

impl Drained {
    fn send(&self, keys: &str) {
        self.input
            .lock()
            .unwrap()
            .write_all(keys.as_bytes())
            .unwrap();
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.output.lock().unwrap()).into_owned()
    }

    /// Block until `needle` appears in the output (bounded).
    fn wait_for(&self, needle: &[u8]) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if self
                .output
                .lock()
                .unwrap()
                .windows(needle.len())
                .any(|w| w == needle)
            {
                return;
            }
            assert!(Instant::now() < deadline, "never saw {needle:?}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

#[test]
fn draws_installed_and_quits_cleanly() {
    let f = fixture();
    let (mut master, mut child) = spawn_pty(command(&f));
    let d = drain(&mut master);
    // the fixture entry's label proves the store was read and drawn
    d.wait_for(b"widget@1.2.3");
    // SGR mouse events (press/release/wheel), a real resize
    // (SIGWINCH → Event::Resize), and a bracketed paste (→ the
    // loop's catch-all arm)
    master
        .resize(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    d.send("\x1b[<0;10;5M\x1b[<0;10;5m\x1b[<64;10;5M\x1b[<65;10;5M\x1b[I\x1b[O\x1b[200~pasted\x1b[201~");
    std::thread::sleep(Duration::from_millis(800)); // ≥3 poll cycles
    d.send("q");
    let status = wait_exit(&mut child);
    assert!(
        status.success(),
        "exit {:?}: {}",
        status.exit_code(),
        d.text()
    );
    assert!(d.text().contains("widget"), "no installed row drawn");
    // the terminal was restored — alt-screen exit + mouse-off appear
    // in the stream after the last frame
    let text = d.text();
    assert!(
        text.contains("\x1b[?1049l")
            || text.contains("\x1b[?1000l")
            || text.contains("\x1b[?1006l"),
        "no terminal restore seen"
    );
}

#[test]
fn filter_and_command_quit() {
    let f = fixture();
    let (mut master, mut child) = spawn_pty(command(&f));
    let d = drain(&mut master);
    d.wait_for(b"widget@1.2.3");
    // `/wid` filters live, `Enter` keeps it, `esc`-less; then `:` + `q`
    d.send("/wid");
    std::thread::sleep(Duration::from_millis(300));
    d.send("\r");
    std::thread::sleep(Duration::from_millis(200));
    d.send(":q\r");
    let status = wait_exit(&mut child);
    assert!(
        status.success(),
        "exit {:?}: {}",
        status.exit_code(),
        d.text()
    );
}

#[test]
fn detail_verify_and_registry_pane() {
    let f = fixture();
    let (mut master, mut child) = spawn_pty(command(&f));
    let d = drain(&mut master);
    d.wait_for(b"widget@1.2.3");
    // Enter opens the detail view — provenance fields render
    d.send("\r");
    d.wait_for(b"source:");
    d.wait_for(b"./pkg");
    // esc backs out to the table
    d.send("\x1b");
    std::thread::sleep(Duration::from_millis(300));
    // `2` → the registry pane's search hint (phase 3) — the hint's
    // head survives the 80-col truncation
    d.send("2");
    d.wait_for(b"registry search");
    // back to installed; `:verify` runs the op on a worker — the
    // result lands in the log/status stream
    d.send("1");
    std::thread::sleep(Duration::from_millis(200));
    d.send(":verify widget\r");
    d.wait_for(b"widget@1.2.3: ok");
    d.send("q");
    let status = wait_exit(&mut child);
    assert!(
        status.success(),
        "exit {:?}: {}",
        status.exit_code(),
        d.text()
    );
}

#[test]
fn unknown_command_echoes_and_r_quits() {
    let f = fixture();
    let (mut master, mut child) = spawn_pty(command(&f));
    let d = drain(&mut master);
    d.wait_for(b"widget@1.2.3");
    d.send(":bogus\r");
    d.wait_for(b"unknown command");
    d.send("q");
    let status = wait_exit(&mut child);
    assert!(status.success(), "exit {:?}", status.exit_code());
}

#[test]
fn mid_run_pty_death_surfaces_an_error() {
    let f = fixture();
    // no drain() here — its cloned fds would keep the master alive;
    // we need every master-side fd gone for the child to see EIO
    let (master, mut child) = spawn_pty(command(&f));
    std::thread::sleep(Duration::from_millis(800));
    drop(master);
    let status = wait_exit(&mut child);
    assert!(!status.success());
}

#[test]
fn closed_pty_surfaces_an_error() {
    let f = fixture();
    let (master, mut child) = spawn_pty(command(&f));
    // master dropped → the child's reads/writes hit EIO; run()
    // must return Err rather than spin
    drop(master);
    let status = wait_exit(&mut child);
    assert!(!status.success());
}

#[test]
fn no_tty_fails_fast() {
    let f = fixture();
    // piped stdio, no controlling terminal for the child — raw
    // mode or the size query must fail, not hang
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_tamias-tui"))
        .current_dir(&f.cwd)
        .env("TAMIAS_HOME", &f.home)
        .env("TAMIAS_CACHE", f.cache.join("fetch"))
        .env("TAMIAS_REGISTRY", f.data.join("registry"))
        .env("XDG_DATA_HOME", &f.data)
        .env("XDG_CACHE_HOME", &f.cache)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        assert!(Instant::now() < deadline, "tamias-tui hung without a tty");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(!status.success());
}
