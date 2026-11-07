#![allow(clippy::unwrap_used)]
//! End-to-end: install a local fixture, list, info, verify, remove —
//! driving the real binary through the fs source + cache + store.

use std::path::Path;
use std::process::{Command, Output};

fn tamias(home: &Path, cache: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tamias"))
        .args(args)
        .env("TAMIAS_HOME", home)
        .env("TAMIAS_CACHE", cache)
        .output()
        .unwrap()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).to_string()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).to_string()
}

#[test]
fn install_list_info_verify_remove() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let cache = tmp.path().join("cache");
    let pkg = tmp.path().join("pkg");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(
        pkg.join("tamias.yaml"),
        "name: widget\nversion: 1.2.3\ntype: files\n",
    )
    .unwrap();
    std::fs::write(pkg.join("data.txt"), "hello").unwrap();

    let o = tamias(&home, &cache, &["install", pkg.to_str().unwrap()]);
    assert!(o.status.success(), "install: {}", stderr(&o));
    assert!(stdout(&o).contains("widget@1.2.3"), "{}", stdout(&o));

    let o = tamias(&home, &cache, &["list"]);
    assert!(o.status.success());
    assert!(stdout(&o).contains("widget@1.2.3"));

    let o = tamias(&home, &cache, &["info", "widget"]);
    assert!(o.status.success(), "info: {}", stderr(&o));
    assert!(stdout(&o).contains("version: 1.2.3"));

    let o = tamias(&home, &cache, &["verify", "widget"]);
    assert!(o.status.success(), "verify: {}", stderr(&o));
    assert!(stdout(&o).contains("ok"));

    // tamper → verify fails
    let entry = home.join("store/widget@1.2.3/data.txt");
    std::fs::write(&entry, "tampered").unwrap();
    let o = tamias(&home, &cache, &["verify", "widget"]);
    assert!(!o.status.success(), "expected verify to fail after tamper");

    let o = tamias(&home, &cache, &["remove", "widget"]);
    assert!(o.status.success(), "remove: {}", stderr(&o));
    let o = tamias(&home, &cache, &["list"]);
    assert!(stdout(&o).contains("no packages installed"));
}

#[test]
fn peregrin_bundle_mirrors_into_peregrin_store() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let cache = tmp.path().join("cache");
    let peregrin = tmp.path().join("peregrin");
    let pkg = tmp.path().join("pkg");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(
        pkg.join("tamias.yaml"),
        "name: ci-gate\nversion: 0.2.0\ntype: peregrin-bundle\n",
    )
    .unwrap();
    std::fs::write(pkg.join("main.yaml"), "name: ci-gate\n").unwrap();
    // A stub `peregrin` provider on PATH — the content type shells
    // `peregrin provision install …`; it records the call.
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let provider = bin.join("peregrin");
    std::fs::write(
        &provider,
        "#!/bin/sh\necho \"$@\" > \"$PROV_MARKER\"\nexit 0\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&provider, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let marker = tmp.path().join("prov.marker");
    let path_env = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let o = Command::new(env!("CARGO_BIN_EXE_tamias"))
        .args(["install", pkg.to_str().unwrap()])
        .env("TAMIAS_HOME", &home)
        .env("TAMIAS_CACHE", &cache)
        .env("PEREGRIN_HOME", &peregrin)
        .env("PATH", path_env)
        .env("PROV_MARKER", marker.to_str().unwrap())
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(
        peregrin.join("store/ci-gate@0.2.0/main.yaml").exists(),
        "peregrin mirror missing"
    );
    let invoked = std::fs::read_to_string(&marker).unwrap();
    assert!(
        invoked.starts_with("provision install"),
        "provider not invoked: {invoked:?}"
    );
}

#[test]
fn add_writes_manifest_and_lock() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let cache = tmp.path().join("cache");
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    let pkg = tmp.path().join("pkg");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(
        pkg.join("tamias.yaml"),
        "name: widget\nversion: 1.0.0\ntype: files\n",
    )
    .unwrap();
    std::fs::write(pkg.join("a.txt"), "a").unwrap();
    let o = Command::new(env!("CARGO_BIN_EXE_tamias"))
        .args(["add", pkg.to_str().unwrap()])
        .current_dir(&proj)
        .env("TAMIAS_HOME", &home)
        .env("TAMIAS_CACHE", &cache)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let m = std::fs::read_to_string(proj.join("tamias.yaml")).unwrap();
    assert!(m.contains("deps:"), "manifest: {m}");
    let l = std::fs::read_to_string(proj.join("tamias.lock")).unwrap();
    assert!(l.contains("widget"), "lock: {l}");
    assert!(l.contains("sha256:"), "lock digest: {l}");
}

#[test]
fn serve_ndjson_roundtrip() {
    use std::io::Write;
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let cache = tmp.path().join("cache");
    let mut child = Command::new(env!("CARGO_BIN_EXE_tamias"))
        .arg("serve")
        .env("TAMIAS_HOME", &home)
        .env("TAMIAS_CACHE", &cache)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"{\"op\":\"list\"}\n")
        .unwrap();
    drop(child.stdin.take());
    let out = child.wait_with_output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("\"ok\":true"), "serve out: {text}");
}

#[test]
fn publish_search_and_install_from_registry_tgz() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let cache = tmp.path().join("cache");
    let reg = tmp.path().join("registry");
    let pkg = tmp.path().join("pkg");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(
        pkg.join("tamias.yaml"),
        "name: widget\nversion: 2.0.0\ntype: files\n",
    )
    .unwrap();
    std::fs::write(pkg.join("a.txt"), "a").unwrap();

    let o = Command::new(env!("CARGO_BIN_EXE_tamias"))
        .args([
            "publish",
            pkg.to_str().unwrap(),
            "--registry",
            reg.to_str().unwrap(),
        ])
        .env("TAMIAS_HOME", &home)
        .env("TAMIAS_CACHE", &cache)
        .output()
        .unwrap();
    assert!(o.status.success(), "publish: {}", stderr(&o));
    assert!(reg.join("widget/2.0.0.tgz").is_file());

    let o = Command::new(env!("CARGO_BIN_EXE_tamias"))
        .args(["search", "widget", "--registry", reg.to_str().unwrap()])
        .env("TAMIAS_HOME", &home)
        .env("TAMIAS_CACHE", &cache)
        .output()
        .unwrap();
    assert!(stdout(&o).contains("widget@2.0.0"), "{}", stdout(&o));

    // install the published .tgz via an fs ref, then verify it landed.
    let tgz = reg.join("widget/2.0.0.tgz");
    let o = tamias(&home, &cache, &["install", tgz.to_str().unwrap()]);
    assert!(o.status.success(), "install tgz: {}", stderr(&o));
    let o = tamias(&home, &cache, &["list"]);
    assert!(stdout(&o).contains("widget@2.0.0"));
}

#[test]
fn myco_config_type_installs_a_pack() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let cache = tmp.path().join("cache");
    let myco = tmp.path().join("myco");
    let pkg = tmp.path().join("pkg");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(
        pkg.join("tamias.yaml"),
        "name: pm\nversion: 1.0.0\ntype: myco-config\n",
    )
    .unwrap();
    std::fs::write(
        pkg.join("kinds.toml"),
        "[[kinds]]\nname = \"spike\"\nprefix = \"S\"\nstorage = \"dir\"\n",
    )
    .unwrap();
    let o = Command::new(env!("CARGO_BIN_EXE_tamias"))
        .args(["install", pkg.to_str().unwrap()])
        .env("TAMIAS_HOME", &home)
        .env("TAMIAS_CACHE", &cache)
        .env("MYCO_HOME", &myco)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(
        myco.join("packs/pm/kinds.toml").is_file(),
        "myco pack missing"
    );
}

#[test]
fn registry_http_publish_and_search() {
    use std::time::Duration;
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let cache = tmp.path().join("cache");
    let reg = tmp.path().join("registry");
    let pkg = tmp.path().join("pkg");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(
        pkg.join("tamias.yaml"),
        "name: widget\nversion: 3.0.0\ntype: files\n",
    )
    .unwrap();
    std::fs::write(pkg.join("a.txt"), "a").unwrap();
    let addr = "127.0.0.1:8799";
    let mut server = Command::new(env!("CARGO_BIN_EXE_tamias"))
        .args([
            "registry",
            "serve",
            "--addr",
            addr,
            "--dir",
            reg.to_str().unwrap(),
        ])
        .env("TAMIAS_HOME", &home)
        .env("TAMIAS_CACHE", &cache)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(700));
    let url = format!("http://{addr}");
    let o = Command::new(env!("CARGO_BIN_EXE_tamias"))
        .args(["publish", pkg.to_str().unwrap(), "--registry", &url])
        .env("TAMIAS_HOME", &home)
        .env("TAMIAS_CACHE", &cache)
        .output()
        .unwrap();
    assert!(o.status.success(), "publish http: {}", stderr(&o));
    let o = Command::new(env!("CARGO_BIN_EXE_tamias"))
        .args(["search", "widget", "--registry", &url])
        .env("TAMIAS_HOME", &home)
        .env("TAMIAS_CACHE", &cache)
        .output()
        .unwrap();
    assert!(stdout(&o).contains("widget@3.0.0"), "{}", stdout(&o));
    server.kill().ok();
    let _ = server.wait();
}

#[test]
fn deploy_and_uninstall_use_the_provider() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let cache = tmp.path().join("cache");
    let pkg = tmp.path().join("pkg");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(
        pkg.join("tamias.yaml"),
        "name: gate\nversion: 0.1.0\ntype: peregrin-bundle\n",
    )
    .unwrap();
    std::fs::write(pkg.join("main.yaml"), "name: gate\n").unwrap();
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let provider = bin.join("peregrin");
    std::fs::write(
        &provider,
        "#!/bin/sh\necho \"$1 $2\" >> \"$PROV_LOG\"\nexit 0\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&provider, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let log = tmp.path().join("prov.log");
    let path_env = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_tamias"))
            .args(args)
            .env("TAMIAS_HOME", &home)
            .env("TAMIAS_CACHE", &cache)
            .env("PEREGRIN_HOME", tmp.path().join("peregrin"))
            .env("PATH", &path_env)
            .env("PROV_LOG", log.to_str().unwrap())
            .output()
            .unwrap()
    };
    assert!(run(&["install", pkg.to_str().unwrap()]).status.success());
    assert!(run(&["deploy", "gate"]).status.success());
    assert!(run(&["uninstall", "gate"]).status.success());
    let calls = std::fs::read_to_string(&log).unwrap();
    assert!(calls.contains("provision install"), "{calls}");
    assert!(calls.contains("provision deploy"), "{calls}");
    assert!(calls.contains("provision uninstall"), "{calls}");
}

#[test]
fn driver_discovery_dispatches() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let cache = tmp.path().join("cache");
    let pkg = tmp.path().join("pkg");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(
        pkg.join("tamias.yaml"),
        "name: fx\nversion: 1.0.0\ntype: fixture\n",
    )
    .unwrap();
    std::fs::write(pkg.join("a.txt"), "a").unwrap();
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let driver = bin.join("tamias-driver-fixture");
    std::fs::write(&driver, "#!/bin/sh\necho \"$1\" >> \"$DRV_LOG\"\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&driver, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let log = tmp.path().join("drv.log");
    let path_env = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let o = Command::new(env!("CARGO_BIN_EXE_tamias"))
        .args(["install", pkg.to_str().unwrap()])
        .env("TAMIAS_HOME", &home)
        .env("TAMIAS_CACHE", &cache)
        .env("PATH", &path_env)
        .env("DRV_LOG", log.to_str().unwrap())
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", stderr(&o));
    let invoked = std::fs::read_to_string(&log).unwrap();
    assert!(
        invoked.contains("install"),
        "driver not dispatched: {invoked:?}"
    );
}

#[test]
fn install_registers_bundled_driver() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let cache = tmp.path().join("cache");
    let pkg = tmp.path().join("pkg");
    std::fs::create_dir_all(pkg.join("drivers")).unwrap();
    std::fs::write(
        pkg.join("tamias.yaml"),
        "name: doodad\nversion: 1.0.0\ntype: files\n",
    )
    .unwrap();
    std::fs::write(
        pkg.join("drivers/tamias-driver-doodad"),
        "#!/bin/sh\nexit 0\n",
    )
    .unwrap();
    let o = tamias(&home, &cache, &["install", pkg.to_str().unwrap()]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(
        home.join("drivers/tamias-driver-doodad").is_file(),
        "driver not registered: {}",
        stdout(&o)
    );
}
