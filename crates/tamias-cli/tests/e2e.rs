#![allow(clippy::unwrap_used)]
//! End-to-end: install a local fixture, list, info, verify, remove —
//! driving the real binary through the fs source + cache + store.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Point `XDG_CONFIG_HOME` at a per-process tempdir so spawned `tamias`
/// binaries never read the developer's real `~/.config/tamias/` — the
/// user-level config layer (registries, publish/search defaults) would
/// otherwise leak ambient state into every test. Called from
/// [`tamias_bin`], which every spawn goes through; the env mutation is
/// process-wide so later spawns inherit it.
fn isolate_user_config() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let dir = std::env::temp_dir().join(format!("tamias-e2e-xdg-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::env::set_var("XDG_CONFIG_HOME", dir);
    });
}

/// Path to the `tamias` binary under test. `env!("CARGO_BIN_EXE_…")`
/// is baked at compile time — a stale or shared `target/` (repo moved,
/// `CARGO_TARGET_DIR` reuse across checkouts) can leave it pointing at
/// a path that no longer exists. Fall back to the bin next to this
/// test executable: `target/<profile>/deps/<test>` → `target/<profile>/`.
fn tamias_bin() -> PathBuf {
    isolate_user_config();
    let baked = PathBuf::from(env!("CARGO_BIN_EXE_tamias"));
    if baked.exists() {
        return baked;
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .and_then(|deps| deps.parent().map(Path::to_path_buf))
        .map(|profile| profile.join("tamias"))
        .filter(|p| p.exists())
        .unwrap_or(baked)
}

fn tamias(home: &Path, cache: &Path, args: &[&str]) -> Output {
    Command::new(tamias_bin())
        .args(args)
        .env("TAMIAS_HOME", home)
        .env("TAMIAS_CACHE", cache)
        // per-test consent file — driver-consent records must never
        // leak into the real ~/.config/tamias across test runs
        .env("TAMIAS_CONSENT_FILE", cache.join("consented.json"))
        .env_remove("TAMIAS_ALLOW_DRIVERS")
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
fn list_and_info_surface_recorded_trust() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let cache = tmp.path().join("cache");
    let pkg = tmp.path().join("pkg");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(
        pkg.join("tamias.yaml"),
        "name: widget\nversion: 1.0.0\ntype: files\n",
    )
    .unwrap();
    std::fs::write(pkg.join("a.txt"), "a").unwrap();

    let o = tamias(&home, &cache, &["install", pkg.to_str().unwrap()]);
    assert!(o.status.success(), "install: {}", stderr(&o));

    // fs-path installs record the `unpinned` trust tier at install.
    let o = tamias(&home, &cache, &["list"]);
    assert!(o.status.success());
    let text = stdout(&o);
    assert!(text.contains("widget@1.0.0"), "{text}");
    assert!(text.contains("unpinned"), "trust badge missing: {text}");

    // --json carries trust + signer (null when unrecorded).
    let o = tamias(&home, &cache, &["list", "--json"]);
    assert!(o.status.success());
    let list: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    let pkg = &list["packages"][0];
    assert_eq!(pkg["trust"], "unpinned");
    assert!(pkg["signer"].is_null());

    let o = tamias(&home, &cache, &["info", "widget"]);
    assert!(o.status.success(), "info: {}", stderr(&o));
    let text = stdout(&o);
    assert!(text.contains("trust:   unpinned"), "{text}");
    assert!(text.contains("signer:  -"), "{text}");

    let o = tamias(&home, &cache, &["info", "widget", "--json"]);
    assert!(o.status.success());
    let info: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(info["trust"], "unpinned");
    assert!(info["signer"].is_null());
}

#[test]
fn peregrin_bundle_uses_the_peregrin_driver() {
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
    // A stub `tamias-driver-peregrin` — found by capability; installs into
    // PEREGRIN_HOME.
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let driver = bin.join("tamias-driver-peregrin");
    std::fs::write(
        &driver,
        "#!/bin/sh\ncase \"$1\" in\n  capabilities) echo '{\"types\":[\"peregrin-bundle\"]}' ;;\n  install) mkdir -p \"$PEREGRIN_HOME/store/ci-gate@0.2.0\"; echo x > \"$PEREGRIN_HOME/store/ci-gate@0.2.0/main.yaml\" ;;\nesac\nexit 0\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&driver, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path_env = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let o = Command::new(tamias_bin())
        .args(["install", pkg.to_str().unwrap()])
        .env("TAMIAS_HOME", &home)
        .env("TAMIAS_CACHE", &cache)
        .env("PEREGRIN_HOME", &peregrin)
        .env("PATH", path_env)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(
        peregrin.join("store/ci-gate@0.2.0/main.yaml").exists(),
        "driver mirror missing"
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
    let o = Command::new(tamias_bin())
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

#[cfg(not(coverage))]
#[test]
fn serve_ndjson_roundtrip() {
    use std::io::Write;
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let cache = tmp.path().join("cache");
    let mut child = Command::new(tamias_bin())
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

    let o = Command::new(tamias_bin())
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

    let o = Command::new(tamias_bin())
        .args(["search", "widget", "--registry", reg.to_str().unwrap()])
        .env("TAMIAS_HOME", &home)
        .env("TAMIAS_CACHE", &cache)
        .output()
        .unwrap();
    assert!(stdout(&o).contains("widget@2.0.0"), "{}", stdout(&o));

    // --json carries signed + publisher (unverified index metadata).
    let o = Command::new(tamias_bin())
        .args([
            "search",
            "widget",
            "--registry",
            reg.to_str().unwrap(),
            "--json",
        ])
        .env("TAMIAS_HOME", &home)
        .env("TAMIAS_CACHE", &cache)
        .output()
        .unwrap();
    assert!(o.status.success(), "search --json: {}", stderr(&o));
    let results: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    let hit = &results["results"][0];
    assert_eq!(hit["signed"], false);
    assert!(hit["publisher"].is_null());

    // install the published .tgz via an fs ref, then verify it landed.
    let tgz = reg.join("widget/2.0.0.tgz");
    let o = tamias(&home, &cache, &["install", tgz.to_str().unwrap()]);
    assert!(o.status.success(), "install tgz: {}", stderr(&o));
    let o = tamias(&home, &cache, &["list"]);
    assert!(stdout(&o).contains("widget@2.0.0"));
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
    let mut server = Command::new(tamias_bin())
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
    // wait for the serve subprocess to bind — a fixed sleep raced it
    for _ in 0..100 {
        if std::net::TcpStream::connect(addr).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let url = format!("http://{addr}");
    let o = Command::new(tamias_bin())
        .args(["publish", pkg.to_str().unwrap(), "--registry", &url])
        .env("TAMIAS_HOME", &home)
        .env("TAMIAS_CACHE", &cache)
        .output()
        .unwrap();
    assert!(o.status.success(), "publish http: {}", stderr(&o));
    let o = Command::new(tamias_bin())
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
fn deploy_and_uninstall_use_the_driver() {
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
    let driver = bin.join("tamias-driver-peregrin");
    std::fs::write(
        &driver,
        "#!/bin/sh\ncase \"$1\" in\n  capabilities) echo '{\"types\":[\"peregrin-bundle\"]}' ;;\n  *) echo \"$1\" >> \"$DRV_LOG\" ;;\nesac\nexit 0\n",
    )
    .unwrap();
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
    let run = |args: &[&str]| {
        Command::new(tamias_bin())
            .args(args)
            .env("TAMIAS_HOME", &home)
            .env("TAMIAS_CACHE", &cache)
            .env("PEREGRIN_HOME", tmp.path().join("peregrin"))
            .env("PATH", &path_env)
            .env("DRV_LOG", log.to_str().unwrap())
            .output()
            .unwrap()
    };
    assert!(run(&["install", pkg.to_str().unwrap()]).status.success());
    assert!(run(&["deploy", "gate"]).status.success());
    assert!(run(&["uninstall", "gate"]).status.success());
    let calls = std::fs::read_to_string(&log).unwrap();
    assert!(calls.contains("install"), "{calls}");
    assert!(calls.contains("deploy"), "{calls}");
    assert!(calls.contains("uninstall"), "{calls}");
}

#[test]
fn driver_config_schema_fetches_and_surfaces_unsupported() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let cache = tmp.path().join("cache");
    let pkg = tmp.path().join("pkg");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(
        pkg.join("tamias.yaml"),
        "name: cs\nversion: 1.0.0\ntype: csx\n",
    )
    .unwrap();
    std::fs::write(pkg.join("a.txt"), "a").unwrap();
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    // `config-schema` answers JSON; everything else is a no-op success.
    let driver = bin.join("tamias-driver-csx");
    std::fs::write(
        &driver,
        "#!/bin/sh\nif [ \"$1\" = config-schema ]; then\n  echo '{\"$schema\":\"https://json-schema.org/draft/2020-12/schema\",\"type\":\"object\",\"properties\":{\"name\":{\"type\":\"string\"}}}'\n  exit 0\nfi\nexit 0\n",
    )
    .unwrap();
    // A driver without the verb: non-zero + clear stderr.
    let bad = bin.join("tamias-driver-csbad");
    std::fs::write(
        &bad,
        "#!/bin/sh\nif [ \"$1\" = config-schema ]; then\n  echo 'config-schema: unsupported' >&2\n  exit 1\nfi\nexit 0\n",
    )
    .unwrap();
    for d in [&driver, &bad] {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    let badpkg = tmp.path().join("badpkg");
    std::fs::create_dir_all(&badpkg).unwrap();
    std::fs::write(
        badpkg.join("tamias.yaml"),
        "name: cb\nversion: 1.0.0\ntype: csbad\n",
    )
    .unwrap();
    std::fs::write(badpkg.join("a.txt"), "a").unwrap();
    let path_env = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let run = |args: &[&str]| {
        Command::new(tamias_bin())
            .args(args)
            .env("TAMIAS_HOME", &home)
            .env("TAMIAS_CACHE", &cache)
            .env("PATH", &path_env)
            .output()
            .unwrap()
    };
    assert!(run(&["install", pkg.to_str().unwrap()]).status.success());
    assert!(run(&["install", badpkg.to_str().unwrap()]).status.success());
    // Fetch: stdout is the driver's schema JSON, verbatim.
    let o = run(&["driver", "config-schema", "cs"]);
    assert!(o.status.success(), "{}", stderr(&o));
    let schema: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(schema["type"], "object");
    // Unsupported verb → non-zero, driver stderr surfaced.
    let o = run(&["driver", "config-schema", "cb"]);
    assert!(!o.status.success());
    assert!(stderr(&o).contains("unsupported"), "{}", stderr(&o));
    // Unknown package → error.
    let o = run(&["driver", "config-schema", "ghost"]);
    assert!(!o.status.success());
    // Installed package with no driver → notice on stdout, success.
    let plain = tmp.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    std::fs::write(plain.join("tamias.yaml"), "name: nd\nversion: 1.0.0\n").unwrap();
    std::fs::write(plain.join("a.txt"), "a").unwrap();
    assert!(run(&["install", plain.to_str().unwrap()]).status.success());
    let o = run(&["driver", "config-schema", "nd"]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(stdout(&o).contains("no driver"), "{}", stdout(&o));
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
    let o = Command::new(tamias_bin())
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
fn drivers_lists_capabilities() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    // a driver installed under <tamias-home>/drivers answers capabilities
    let dd = home.join("drivers");
    std::fs::create_dir_all(&dd).unwrap();
    let driver = dd.join("tamias-driver-fx");
    std::fs::write(
        &driver,
        "#!/bin/sh\nif [ \"$1\" = capabilities ]; then echo '{\"types\":[\"fx\"],\"verbs\":[\"install\",\"deploy\",\"uninstall\"]}'; fi\nexit 0\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&driver, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let cache = tmp.path().join("cache");
    let o = tamias(&home, &cache, &["drivers", "--json"]);
    assert!(o.status.success(), "drivers: {}", stderr(&o));
    let value: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    let drivers = value["drivers"].as_array().unwrap();
    let fx = drivers
        .iter()
        .find(|d| d["name"] == "tamias-driver-fx")
        .expect("fx driver listed");
    assert_eq!(fx["types"], serde_json::json!(["fx"]));
    assert_eq!(fx["location"], "installed");
    assert!(fx["path"].as_str().unwrap().contains("drivers"));
    assert_eq!(
        fx["capabilities"]["verbs"],
        serde_json::json!(["install", "deploy", "uninstall"])
    );
    // text mode too
    let o = tamias(&home, &cache, &["drivers"]);
    assert!(o.status.success(), "drivers text: {}", stderr(&o));
    assert!(stdout(&o).contains("tamias-driver-fx"), "{}", stdout(&o));
}

#[test]
fn provider_list_shows_provider_packages() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let cache = tmp.path().join("cache");
    let pkg = tmp.path().join("pkg");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(
        pkg.join("tamias.yaml"),
        "name: gate\nversion: 1.0.0\nprovider:\n  name: gateprov\n  services:\n    - name: web\n      exec: \"/bin/true\"\n  ingress:\n    - public_url: https://ci.example.com\n      path: /hook\n  verbs: [run, logs]\n",
    )
    .unwrap();
    std::fs::write(pkg.join("a.txt"), "a").unwrap();
    let plain = tmp.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    std::fs::write(plain.join("tamias.yaml"), "name: w\nversion: 1.0.0\n").unwrap();
    std::fs::write(plain.join("a.txt"), "a").unwrap();
    assert!(tamias(&home, &cache, &["install", pkg.to_str().unwrap()])
        .status
        .success());
    assert!(tamias(&home, &cache, &["install", plain.to_str().unwrap()])
        .status
        .success());
    // default: only the provider package
    let o = tamias(&home, &cache, &["provider", "list"]);
    assert!(o.status.success(), "provider list: {}", stderr(&o));
    let out = stdout(&o);
    assert!(out.contains("gate@1.0.0"), "{out}");
    assert!(out.contains("gateprov-web.service"), "{out}");
    assert!(out.contains("https://ci.example.com"), "{out}");
    assert!(!out.contains("w@1.0.0"), "{out}");
    // --all adds the plain package
    let o = tamias(&home, &cache, &["provider", "list", "--all"]);
    assert!(stdout(&o).contains("w@1.0.0"), "{}", stdout(&o));
    // --json parses and carries the provider block
    let o = tamias(&home, &cache, &["provider", "list", "--json"]);
    assert!(o.status.success(), "provider list --json: {}", stderr(&o));
    let value: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    let providers = value["providers"].as_array().unwrap();
    assert_eq!(providers.len(), 1);
    assert_eq!(providers[0]["provider"], "gateprov");
    assert_eq!(providers[0]["services"][0]["unit"], "gateprov-web.service");
    assert_eq!(
        providers[0]["ingress"][0]["public_url"],
        "https://ci.example.com"
    );
    assert_eq!(providers[0]["verbs"], serde_json::json!(["run", "logs"]));
}

#[test]
fn run_execs_provider_service_foreground() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let cache = tmp.path().join("cache");
    let pkg = tmp.path().join("pkg");
    std::fs::create_dir_all(&pkg).unwrap();
    // single service → no service arg needed; exec prints via printenv
    std::fs::write(
        pkg.join("tamias.yaml"),
        "name: solo\nversion: 1.0.0\nprovider:\n  services:\n    - name: main\n      exec: \"printenv RUN_MARK\"\n      env: {RUN_MARK: ok}\n",
    )
    .unwrap();
    std::fs::write(pkg.join("a.txt"), "a").unwrap();
    assert!(tamias(&home, &cache, &["install", pkg.to_str().unwrap()])
        .status
        .success());
    // the service env reaches the child; exit 0
    let o = tamias(&home, &cache, &["run", "solo"]);
    assert!(o.status.success(), "run: {}", stderr(&o));
    assert!(stdout(&o).contains("ok"), "{}", stdout(&o));
    // a package without provider: → error
    let plain = tmp.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    std::fs::write(plain.join("tamias.yaml"), "name: w\nversion: 1.0.0\n").unwrap();
    std::fs::write(plain.join("a.txt"), "a").unwrap();
    assert!(tamias(&home, &cache, &["install", plain.to_str().unwrap()])
        .status
        .success());
    let o = tamias(&home, &cache, &["run", "w"]);
    assert!(!o.status.success(), "run on plain package must fail");
    let o = tamias(&home, &cache, &["run", "ghost"]);
    assert!(!o.status.success());
}

#[test]
fn driver_exec_runs_provider_verbs() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let cache = tmp.path().join("cache");
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    // A driver that echoes its argv — capabilities declares the ops
    // verbs `driver exec` will dispatch.
    let driver = bin.join("tamias-driver-fx");
    std::fs::write(
        &driver,
        "#!/bin/sh\nif [ \"$1\" = capabilities ]; then echo '{\"types\":[\"fx\"],\"verbs\":[\"health\",\"hooks\"]}'; exit 0; fi\nprintf '%s\\n' \"$@\"\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&driver, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path_env = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let run = |args: &[&str]| {
        Command::new(tamias_bin())
            .args(args)
            .env("TAMIAS_HOME", &home)
            .env("TAMIAS_CACHE", &cache)
            .env("PATH", &path_env)
            .output()
            .unwrap()
    };
    let pkg = tmp.path().join("pkg");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(
        pkg.join("tamias.yaml"),
        "name: ops-pkg\nversion: 1.0.0\ntype: fx\n",
    )
    .unwrap();
    std::fs::write(pkg.join("a.txt"), "a").unwrap();
    assert!(run(&["install", pkg.to_str().unwrap()]).status.success());
    // verb + trailing args pass through; driver stdout streams back
    let o = run(&[
        "driver",
        "exec",
        "ops-pkg",
        "hooks",
        "register",
        "--dry-run",
    ]);
    assert!(o.status.success(), "driver exec: {}", stderr(&o));
    let out = stdout(&o);
    assert!(out.contains("hooks"), "{out}");
    assert!(out.contains("--dir"), "{out}");
    assert!(out.contains("register"), "{out}");
    assert!(out.contains("--dry-run"), "{out}");
    // undeclared verb warns on stderr but still dispatches
    let o = run(&["driver", "exec", "ops-pkg", "bogus"]);
    assert!(o.status.success(), "driver exec bogus: {}", stderr(&o));
    assert!(
        stderr(&o).contains("does not declare verb"),
        "{}",
        stderr(&o)
    );
    // unknown package → error
    assert!(!run(&["driver", "exec", "ghost", "health"]).status.success());
}

#[test]
fn bare_tamias_prints_help() {
    let o = Command::new(tamias_bin()).output().unwrap();
    assert!(o.status.success(), "bare tamias: {}", stderr(&o));
    let out = stdout(&o);
    assert!(out.contains("Usage:"), "{out}");
    assert!(out.contains("Commands:"), "{out}");
    // `tamias version` still prints the version
    let tmp = tempfile::tempdir().unwrap();
    let o = tamias(&tmp.path().join("h"), &tmp.path().join("c"), &["version"]);
    assert!(o.status.success());
    assert!(stdout(&o).starts_with("tamias "), "{}", stdout(&o));
}

#[test]
fn manifest_lists_every_verb() {
    let tmp = tempfile::tempdir().unwrap();
    let o = tamias(&tmp.path().join("h"), &tmp.path().join("c"), &["manifest"]);
    assert!(o.status.success(), "manifest: {}", stderr(&o));
    let value: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    let verbs: Vec<&str> = value["verbs"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    for want in [
        "install",
        "add",
        "remove",
        "list",
        "info",
        "verify",
        "update",
        "pin",
        "cache",
        "version",
        "manifest",
        "schema",
        "gen-cli-docs",
        "completions",
        "doctor",
        "config",
        "serve",
        "publish",
        "search",
        "bundle",
        "registry",
        "deploy-registry",
        "deploy",
        "uninstall",
        "service",
        "drivers",
        "driver",
        "provider",
        "run",
        "logs",
    ] {
        assert!(verbs.contains(&want), "manifest missing verb {want}");
    }
}

#[test]
fn json_flags_emit_parseable_json() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let cache = tmp.path().join("cache");
    let reg = tmp.path().join("registry");
    let pkg = tmp.path().join("pkg");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(
        pkg.join("tamias.yaml"),
        "name: widget\nversion: 1.0.0\ntype: files\n",
    )
    .unwrap();
    std::fs::write(pkg.join("a.txt"), "a").unwrap();
    let o = tamias(&home, &cache, &["install", pkg.to_str().unwrap()]);
    assert!(o.status.success(), "install: {}", stderr(&o));
    // publish so `search` has an entry
    let o = Command::new(tamias_bin())
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

    let cases: Vec<Vec<String>> = vec![
        vec!["list".into(), "--json".into()],
        vec!["info".into(), "widget".into(), "--json".into()],
        vec!["verify".into(), "widget".into(), "--json".into()],
        vec!["update".into(), "widget".into(), "--json".into()],
        vec!["cache".into(), "clean".into(), "--json".into()],
        vec![
            "search".into(),
            "widget".into(),
            "--registry".into(),
            reg.to_str().unwrap().into(),
            "--json".into(),
        ],
        vec!["doctor".into(), "--json".into()],
    ];
    for args in cases {
        let o = tamias(
            &home,
            &cache,
            &args.iter().map(String::as_str).collect::<Vec<_>>(),
        );
        assert!(o.status.success(), "{args:?}: {}", stderr(&o));
        let text = stdout(&o);
        let parsed: serde_json::Value = serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("{args:?} not JSON: {e}\n{text}"));
        assert!(!parsed.is_null(), "{args:?}");
    }
    // --all conflicts with --days
    let o = tamias(&home, &cache, &["cache", "clean", "--all", "--days", "3"]);
    assert!(!o.status.success(), "--all --days must conflict");
}

#[test]
fn update_and_pin_refresh_the_lock() {
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
    let run = |args: &[&str]| {
        Command::new(tamias_bin())
            .args(args)
            .current_dir(&proj)
            .env("TAMIAS_HOME", &home)
            .env("TAMIAS_CACHE", &cache)
            .output()
            .unwrap()
    };
    let o = run(&["add", pkg.to_str().unwrap()]);
    assert!(o.status.success(), "add: {}", stderr(&o));
    let lock_path = proj.join("tamias.lock");
    let text = std::fs::read_to_string(&lock_path).unwrap();
    assert!(text.contains("widget"), "lock: {text}");
    // pin retargets the recorded ref → the lock entry follows
    let o = run(&["pin", "widget", "./elsewhere"]);
    assert!(o.status.success(), "pin: {}", stderr(&o));
    let text = std::fs::read_to_string(&lock_path).unwrap();
    assert!(text.contains("./elsewhere"), "lock: {text}");
    // update re-resolves → digest in the lock is refreshed, not stale
    let o = run(&["update", "widget"]);
    // the pinned ref ./elsewhere doesn't exist → update fails; the lock
    // must be left untouched rather than rewritten with garbage.
    assert!(!o.status.success(), "update of bogus ref should fail");
    let text = std::fs::read_to_string(&lock_path).unwrap();
    assert!(
        text.contains("./elsewhere"),
        "lock after failed update: {text}"
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
    // no consent → refused before install; nothing registered or run
    let o = tamias(&home, &cache, &["install", pkg.to_str().unwrap()]);
    assert!(!o.status.success(), "expected refusal: {}", stdout(&o));
    assert!(
        stderr(&o).contains("--allow-drivers"),
        "error should name the consent flag: {}",
        stderr(&o)
    );
    assert!(!home.join("drivers/tamias-driver-doodad").exists());
    let o = tamias(&home, &cache, &["list"]);
    assert!(stdout(&o).contains("no packages installed"));

    // --allow-drivers → registered
    let o = tamias(
        &home,
        &cache,
        &["install", pkg.to_str().unwrap(), "--allow-drivers"],
    );
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(
        home.join("drivers/tamias-driver-doodad").is_file(),
        "driver not registered: {}",
        stdout(&o)
    );

    // TAMIAS_ALLOW_DRIVERS=1 is equivalent consent
    let other = tmp.path().join("pkg2");
    std::fs::create_dir_all(other.join("drivers")).unwrap();
    std::fs::write(
        other.join("tamias.yaml"),
        "name: doodad2\nversion: 1.0.0\ntype: files\n",
    )
    .unwrap();
    std::fs::write(
        other.join("drivers/tamias-driver-doodad2"),
        "#!/bin/sh\nexit 0\n",
    )
    .unwrap();
    let o = Command::new(tamias_bin())
        .args(["install", other.to_str().unwrap()])
        .env("TAMIAS_HOME", &home)
        .env("TAMIAS_CACHE", &cache)
        .env("TAMIAS_CONSENT_FILE", cache.join("consented.json"))
        .env("TAMIAS_ALLOW_DRIVERS", "1")
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(home.join("drivers/tamias-driver-doodad2").is_file());
}

#[test]
fn install_unknown_type_fails_without_entry() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let cache = tmp.path().join("cache");
    let pkg = tmp.path().join("pkg");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(
        pkg.join("tamias.yaml"),
        "name: mystery\nversion: 1.0.0\ntype: zzz-e2e-no-driver\n",
    )
    .unwrap();
    std::fs::write(pkg.join("a.txt"), "a").unwrap();
    let o = tamias(&home, &cache, &["install", pkg.to_str().unwrap()]);
    assert!(!o.status.success(), "expected failure: {}", stdout(&o));
    let err = stderr(&o);
    assert!(err.contains("zzz-e2e-no-driver"), "{err}");
    assert!(err.contains("tamias-driver-zzz-e2e-no-driver"), "{err}");
    let o = tamias(&home, &cache, &["list"]);
    assert!(
        stdout(&o).contains("no packages installed"),
        "{}",
        stdout(&o)
    );
}

#[test]
fn uninstall_driver_failure_keeps_entry() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let cache = tmp.path().join("cache");
    let pkg = tmp.path().join("pkg");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(
        pkg.join("tamias.yaml"),
        "name: leak\nversion: 1.0.0\ntype: e2e-flaky\n",
    )
    .unwrap();
    std::fs::write(pkg.join("a.txt"), "a").unwrap();
    // driver installs fine but its uninstall verb fails
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let driver = bin.join("tamias-driver-e2e-flaky");
    std::fs::write(
        &driver,
        "#!/bin/sh\ncase \"$1\" in\n  uninstall) exit 1;;\n  *) exit 0;;\nesac\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&driver, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path_env = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let run = |args: &[&str]| {
        Command::new(tamias_bin())
            .args(args)
            .env("TAMIAS_HOME", &home)
            .env("TAMIAS_CACHE", &cache)
            .env("PATH", &path_env)
            .output()
            .unwrap()
    };
    assert!(run(&["install", pkg.to_str().unwrap()]).status.success());
    let o = run(&["uninstall", "leak"]);
    assert!(!o.status.success(), "uninstall should fail: {}", stdout(&o));
    let o = run(&["list"]);
    assert!(
        stdout(&o).contains("leak"),
        "entry must be kept: {}",
        stdout(&o)
    );
}

#[test]
fn kuda_and_mimus_have_their_own_drivers() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let cache = tmp.path().join("cache");
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    for (driver, types) in [("kuda", "kuda-domain"), ("mimus", "mimus-rules")] {
        let path = bin.join(format!("tamias-driver-{driver}"));
        let body = format!(
            "#!/bin/sh\ncase \"$1\" in\n  capabilities) echo '{{\"types\":[\"{types}\"]}}' ;;\n  install) mkdir -p \"$HOME_TARGET\"; echo x > \"$HOME_TARGET/marker\" ;;\nesac\nexit 0\n"
        );
        std::fs::write(&path, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    let path_env = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    for (name, kind, target) in [
        ("kd", "kuda-domain", "kuda"),
        ("mr", "mimus-rules", "mimus"),
    ] {
        let pkg = tmp.path().join(name);
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(
            pkg.join("tamias.yaml"),
            format!("name: {name}\nversion: 1.0.0\ntype: {kind}\n"),
        )
        .unwrap();
        std::fs::write(pkg.join("x.toml"), "x").unwrap();
        let target = tmp.path().join(target);
        let o = Command::new(tamias_bin())
            .args(["install", pkg.to_str().unwrap()])
            .env("TAMIAS_HOME", &home)
            .env("TAMIAS_CACHE", &cache)
            .env("PATH", &path_env)
            .env("HOME_TARGET", &target)
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", stderr(&o));
        assert!(
            target.join("marker").is_file(),
            "driver {kind} not dispatched"
        );
    }
}

#[test]
fn publish_sign_and_signed_cf_install() {
    use std::time::Duration;
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let cache = tmp.path().join("cache");
    let reg = tmp.path().join("registry");
    let pkg = tmp.path().join("pkg");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(
        pkg.join("tamias.yaml"),
        "name: sigpkg\nversion: 4.0.0\ntype: files\n",
    )
    .unwrap();
    std::fs::write(pkg.join("a.txt"), "a").unwrap();

    // A real ed25519 key + allowed_signers line (principal = pkg name).
    let key = tmp.path().join("sigkey");
    let out = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&key)
        .output()
        .unwrap();
    assert!(out.status.success(), "ssh-keygen: {}", stderr(&out));
    let pubout = Command::new("ssh-keygen")
        .args(["-y", "-f"])
        .arg(&key)
        .output()
        .unwrap();
    assert!(pubout.status.success());
    let pubkey = String::from_utf8_lossy(&pubout.stdout).trim().to_string();
    let signers = tmp.path().join("allowed_signers");
    std::fs::write(&signers, format!("sigpkg {pubkey}\n")).unwrap();

    // publish --sign --key → the index entry carries the signature.
    let o = Command::new(tamias_bin())
        .args([
            "publish",
            pkg.to_str().unwrap(),
            "--registry",
            reg.to_str().unwrap(),
            "--sign",
            "--key",
            key.to_str().unwrap(),
        ])
        .env("TAMIAS_HOME", &home)
        .env("TAMIAS_CACHE", &cache)
        .output()
        .unwrap();
    assert!(o.status.success(), "publish --sign: {}", stderr(&o));
    let index: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(reg.join("index.json")).unwrap()).unwrap();
    assert!(
        index[0]["signature"]
            .as_str()
            .unwrap()
            .contains("SSH SIGNATURE"),
        "index entry unsigned: {index}"
    );

    // serve the registry, install via cf: with allowed_signers → the
    // signature verifies and lands in the store record.
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap().to_string();
    drop(l);
    let served = reg.clone();
    let mut server = Command::new(tamias_bin())
        .args([
            "registry",
            "serve",
            "--addr",
            &addr,
            "--dir",
            served.to_str().unwrap(),
        ])
        .env("TAMIAS_HOME", &home)
        .env("TAMIAS_CACHE", &cache)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    for _ in 0..100 {
        if std::net::TcpStream::connect(&addr).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let reference = format!("cf:http://{addr}/sigpkg@4.0.0");
    let o = Command::new(tamias_bin())
        .args(["install", &reference])
        .env("TAMIAS_HOME", &home)
        .env("TAMIAS_CACHE", &cache)
        .env("TAMIAS_ALLOWED_SIGNERS", &signers)
        .output()
        .unwrap();
    assert!(o.status.success(), "install cf signed: {}", stderr(&o));

    // tamias verify reports the signature status.
    let o = Command::new(tamias_bin())
        .args(["verify", "sigpkg", "--json"])
        .env("TAMIAS_HOME", &home)
        .env("TAMIAS_CACHE", &cache)
        .env("TAMIAS_ALLOWED_SIGNERS", &signers)
        .output()
        .unwrap();
    assert!(o.status.success(), "verify: {}", stderr(&o));
    assert!(
        stdout(&o).contains("\"signature\":\"verified\""),
        "verify json: {}",
        stdout(&o)
    );

    // a wrong allowed_signers rejects the install outright.
    let other = tmp.path().join("otherkey");
    let out = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&other)
        .output()
        .unwrap();
    assert!(out.status.success());
    let pubout = Command::new("ssh-keygen")
        .args(["-y", "-f"])
        .arg(&other)
        .output()
        .unwrap();
    let pubkey = String::from_utf8_lossy(&pubout.stdout).trim().to_string();
    let bad_signers = tmp.path().join("bad_signers");
    std::fs::write(&bad_signers, format!("sigpkg {pubkey}\n")).unwrap();
    let home2 = tmp.path().join("home2");
    let o = Command::new(tamias_bin())
        .args(["install", &reference])
        .env("TAMIAS_HOME", &home2)
        .env("TAMIAS_CACHE", &cache)
        .env("TAMIAS_ALLOWED_SIGNERS", &bad_signers)
        .output()
        .unwrap();
    assert!(!o.status.success(), "bad signer must fail");
    assert!(
        stderr(&o).contains("signature verification failed"),
        "{}",
        stderr(&o)
    );

    server.kill().ok();
    let _ = server.wait();
}

#[test]
fn install_plan_is_a_dry_run() {
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

    let o = tamias(&home, &cache, &["install", pkg.to_str().unwrap(), "--plan"]);
    assert!(o.status.success(), "plan: {}", stderr(&o));
    let out = stdout(&o);
    assert!(out.contains("widget@1.2.3"), "{out}");
    assert!(out.contains("unpinned"), "{out}");
    assert!(out.contains("includes:"), "{out}");
    assert!(
        !out.contains("installed "),
        "plan must not claim an install: {out}"
    );

    // the dry-run committed nothing — and did not print the install line
    let o = tamias(&home, &cache, &["list"]);
    assert!(stdout(&o).contains("no packages installed"));

    // --json emits the serialized plan; the second fetch is a cache hit
    let o = tamias(
        &home,
        &cache,
        &["install", pkg.to_str().unwrap(), "--plan", "--json"],
    );
    assert!(o.status.success(), "plan --json: {}", stderr(&o));
    let plan: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(plan["manifest"]["name"], "widget");
    assert_eq!(plan["manifest"]["version"], "1.2.3");
    assert_eq!(plan["trust"], "unpinned");
    assert_eq!(plan["cache_hit"], true);
    assert!(plan["problems"].as_array().unwrap().is_empty());
    let o = tamias(&home, &cache, &["list"]);
    assert!(stdout(&o).contains("no packages installed"));
}

#[test]
fn bin_links_payload_into_bin_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let cache = tmp.path().join("cache");
    let bindir = tmp.path().join("userbin");
    let pkg = tmp.path().join("pkg");
    std::fs::create_dir_all(pkg.join("bin")).unwrap();
    std::fs::write(
        pkg.join("tamias.yaml"),
        "name: widget\nversion: 1.0.0\ntype: files\n",
    )
    .unwrap();
    std::fs::write(pkg.join("bin/widget"), "#!/bin/sh\necho hi\n").unwrap();
    std::fs::write(pkg.join("bin/.hidden"), "h").unwrap();

    // `tamias()` doesn't set TAMIAS_BIN_DIR — drive the binary directly
    // so the link lands in the temp dir, never the real ~/.local/bin.
    let run = |args: &[&str]| {
        Command::new(tamias_bin())
            .args(args)
            .env("TAMIAS_HOME", &home)
            .env("TAMIAS_CACHE", &cache)
            .env("TAMIAS_BIN_DIR", &bindir)
            .env("TAMIAS_CONSENT_FILE", cache.join("consented.json"))
            .env_remove("TAMIAS_ALLOW_DRIVERS")
            .output()
            .unwrap()
    };

    let o = run(&["install", pkg.to_str().unwrap()]);
    assert!(o.status.success(), "install: {}", stderr(&o));

    // no bin/ payload → clean error naming the package
    let o = run(&["bin", "ghost"]);
    assert!(!o.status.success());

    let o = run(&["bin", "widget"]);
    assert!(o.status.success(), "bin: {}", stderr(&o));
    let link = bindir.join("widget");
    #[cfg(unix)]
    {
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        // the link resolves through the stable alias under TAMIAS_HOME
        let target = std::fs::read_link(&link).unwrap();
        assert!(
            target.starts_with(home.join("bin-alias/widget/bin")),
            "{target:?}"
        );
    }
    #[cfg(not(unix))]
    assert!(link.is_file());
    assert!(!bindir.join(".hidden").exists());

    // idempotent, then --unlink removes link + alias
    let o = run(&["bin", "widget"]);
    assert!(o.status.success(), "re-link: {}", stderr(&o));
    let o = run(&["bin", "widget", "--unlink"]);
    assert!(o.status.success(), "unlink: {}", stderr(&o));
    assert!(!link.exists() && !link.is_symlink());
    assert!(std::fs::symlink_metadata(home.join("bin-alias/widget")).is_err());
}

#[test]
fn self_upgrade_rejects_unmanaged_binary() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let cache = tmp.path().join("cache");
    // The test binary lives in target/, not in the temp TAMIAS_HOME
    // store — every self-upgrade shape must fail with the same clean
    // "not installed via tamias" error (detection runs before --check
    // or any fetch).
    for args in [
        &["self-upgrade"][..],
        &["self-upgrade", "--check"][..],
        &["self-upgrade", "--json"][..],
    ] {
        let o = tamias(&home, &cache, args);
        assert!(!o.status.success(), "{args:?}: {}", stdout(&o));
        assert!(
            stderr(&o).contains("not installed via tamias"),
            "{args:?}: {}",
            stderr(&o)
        );
    }
}

/// Full self-upgrade: the payload's `bin/tamias` is a copy of the real
/// binary, so running the store copy makes `current_exe` resolve inside
/// the store entry — `self-upgrade` finds its record and re-installs
/// the recorded ref. Unix-only: the update swaps the entry dir out from
/// under the running exe, which Windows can't do to a locked image.
#[cfg(unix)]
#[test]
fn self_upgrade_updates_the_owning_package() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let cache = tmp.path().join("cache");
    let pkg = tmp.path().join("pkg");
    std::fs::create_dir_all(pkg.join("bin")).unwrap();
    std::fs::copy(tamias_bin(), pkg.join("bin/tamias")).unwrap();
    let manifest = pkg.join("tamias.yaml");
    std::fs::write(&manifest, "name: selfbin\nversion: 1.0.0\n").unwrap();

    let o = tamias(&home, &cache, &["install", pkg.to_str().unwrap()]);
    assert!(o.status.success(), "install: {}", stderr(&o));

    // Bump the payload — the recorded source_ref is the pkg dir, so
    // self-upgrade's re-install picks up 1.0.1.
    std::fs::write(&manifest, "name: selfbin\nversion: 1.0.1\n").unwrap();

    // --check reports the pending update without touching the store.
    let exe = home.join("store/selfbin@1.0.0/bin/tamias");
    assert!(exe.exists(), "store exe missing: {}", exe.display());
    let run_exe = |args: &[&str]| {
        Command::new(&exe)
            .args(args)
            .env("TAMIAS_HOME", &home)
            .env("TAMIAS_CACHE", &cache)
            .env("TAMIAS_CONSENT_FILE", cache.join("consented.json"))
            .env_remove("TAMIAS_ALLOW_DRIVERS")
            .output()
            .unwrap()
    };
    let o = run_exe(&["self-upgrade", "--check"]);
    assert!(o.status.success(), "check: {}", stderr(&o));
    assert!(stdout(&o).contains("selfbin@1.0.0"), "{}", stdout(&o));
    assert!(stdout(&o).contains("update available"), "{}", stdout(&o));

    let o = run_exe(&["self-upgrade", "--json"]);
    assert!(o.status.success(), "self-upgrade: {}", stderr(&o));
    let v: serde_json::Value = serde_json::from_str(stdout(&o).trim()).unwrap();
    assert_eq!(v["package"], "selfbin");
    assert_eq!(v["from"], "selfbin@1.0.0");
    assert_eq!(v["to"], "selfbin@1.0.1");
    assert_eq!(v["restart"], true);

    // The store now carries both labels — `update` semantics keep
    // coexisting versions (same as `tamias update <name>`); the old
    // entry is `tamias remove`'s job, not self-upgrade's.
    let o = tamias(&home, &cache, &["list"]);
    assert!(stdout(&o).contains("selfbin@1.0.1"), "{}", stdout(&o));
    assert!(stdout(&o).contains("selfbin@1.0.0"), "{}", stdout(&o));
}

#[test]
fn install_via_named_registry_ref() {
    use std::time::Duration;
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let cache = tmp.path().join("cache");
    // Isolated user-config dir — the real ~/.config/tamias must never
    // leak registries into the ref resolution under test.
    let xdg = tmp.path().join("xdg");
    // The cwd carrying the project `tamias.yml` with `registries:`.
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let reg = tmp.path().join("registry");

    // Two payloads: unscoped `demo` + scoped `@acme/scoped`.
    let pkg = tmp.path().join("pkg");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(
        pkg.join("tamias.yaml"),
        "name: demo\nversion: 1.0.0\ntype: files\n",
    )
    .unwrap();
    std::fs::write(pkg.join("a.txt"), "a").unwrap();
    let spkg = tmp.path().join("spkg");
    std::fs::create_dir_all(&spkg).unwrap();
    std::fs::write(
        spkg.join("tamias.yaml"),
        "name: scoped\nscope: acme\nversion: 2.0.0\ntype: files\n",
    )
    .unwrap();
    std::fs::write(spkg.join("b.txt"), "b").unwrap();

    // Serve the registry on an ephemeral port, publish both packages.
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap().to_string();
    drop(l);
    let mut server = Command::new(tamias_bin())
        .args([
            "registry",
            "serve",
            "--addr",
            &addr,
            "--dir",
            reg.to_str().unwrap(),
        ])
        .env("TAMIAS_HOME", &home)
        .env("TAMIAS_CACHE", &cache)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    for _ in 0..100 {
        if std::net::TcpStream::connect(&addr).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let url = format!("http://{addr}");
    for dir in [&pkg, &spkg] {
        let o = Command::new(tamias_bin())
            .args(["publish", dir.to_str().unwrap(), "--registry", &url])
            .env("TAMIAS_HOME", &home)
            .env("TAMIAS_CACHE", &cache)
            .output()
            .unwrap();
        assert!(o.status.success(), "publish: {}", stderr(&o));
    }

    // The project config names the registry twice: by slug (`localreg`)
    // and by stable id (`mirror`, matched before the slug).
    let reg_id = "018f3c2e-7b1a-7c4d-9e5f-2a6b8c0d1e2f";
    std::fs::write(
        work.join("tamias.yml"),
        format!(
            "registries:\n  localreg:\n    kind: http\n    url: {url}\n  \
             mirror:\n    id: {reg_id}\n    kind: http\n    url: {url}\n  \
             localdir:\n    kind: local\n    url: {}\n",
            reg.display()
        ),
    )
    .unwrap();

    // tamias in `work` — picks up ./tamias.yml + the isolated XDG dir.
    let tamias_at = |args: &[&str]| {
        Command::new(tamias_bin())
            .args(args)
            .current_dir(&work)
            .env("TAMIAS_HOME", &home)
            .env("TAMIAS_CACHE", &cache)
            .env("TAMIAS_CONSENT_FILE", cache.join("consented.json"))
            .env("XDG_CONFIG_HOME", &xdg)
            .env_remove("TAMIAS_ALLOW_DRIVERS")
            .env_remove("TAMIAS_REGISTRY")
            .env_remove("TAMIAS_REGISTRY_TOKEN")
            .env_remove("TAMIAS_CONFIG")
            .output()
            .unwrap()
    };

    // <slug>:<name>@<version>
    let o = tamias_at(&["install", "localreg:demo@1.0.0"]);
    assert!(o.status.success(), "install localreg: {}", stderr(&o));
    // <id>:@scope/<name>@<version> — id match + scoped package name
    let o = tamias_at(&["install", &format!("{reg_id}:@acme/scoped@2.0.0")]);
    assert!(o.status.success(), "install by id scoped: {}", stderr(&o));
    let o = tamias_at(&["list"]);
    assert!(stdout(&o).contains("demo@1.0.0"), "{}", stdout(&o));
    assert!(stdout(&o).contains("@acme/scoped@2.0.0"), "{}", stdout(&o));

    // provenance — the store record keeps the verbatim registry ref
    let o = tamias_at(&["info", "demo", "--json"]);
    assert!(o.status.success(), "info: {}", stderr(&o));
    let info: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(info["source_ref"], "localreg:demo@1.0.0");

    // unknown label → hard error (never an fs-path fallback)
    let o = tamias_at(&["install", "nope:demo@1.0.0"]);
    assert!(!o.status.success());
    assert!(
        stderr(&o).contains("unknown registry 'nope'"),
        "{}",
        stderr(&o)
    );
    // bare name — @<version> required
    let o = tamias_at(&["install", "localreg:demo"]);
    assert!(!o.status.success());
    // local-kind registry → clear error
    let o = tamias_at(&["install", "localdir:demo@1.0.0"]);
    assert!(!o.status.success());
    assert!(stderr(&o).contains("kind local"), "{}", stderr(&o));

    // registry get --json surfaces the id
    let o = tamias_at(&["registry", "get", "mirror", "--json"]);
    assert!(o.status.success(), "get: {}", stderr(&o));
    let got: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(got["id"], reg_id);

    server.kill().ok();
    let _ = server.wait();
}
