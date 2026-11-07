//! tamias-tui — interactive vault browser (`tamias ui`).
//!
//! A thin, read-only front-end over `tamias-ops`: env tabs come from
//! `[envs]` routing, the ref list and detail pane come from
//! `list_records` — metadata only. The posture is *browse the
//! surface, never spill*: the single way a value renders is `r`
//! then `r` again — a deliberate two-step that goes through
//! `Ops::get_in`, emitting the same `get` materialization audit
//! event the CLI does. Keys: `j/k`/arrows move, `tab`/`1-9`
//! switch env, `/` filters, `?` help, `R` reload, `q` quits.
//!
//! A vault that won't open renders as an error line in the pane —
//! the browser never panics on backend failure, and a non-tty
//! stdout declines up front instead of drawing into a pipe.

mod app;
mod render;

use std::io::{self, IsTerminal};
use std::time::Duration;

use crossterm::event::{self, Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

/// Restore the terminal on any exit path.
struct Guard;

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

/// Panic hook — restore the terminal *before* the default hook
/// prints, or the panic is erased by the alt-screen exit and the
/// shell is left in raw mode.
fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        prev(info);
    }));
}

/// Launch the browser on the loaded [`tamias_ops::Ops`]. The caller
/// (`tamias ui`) emits the op-scoped audit event on success — the
/// TUI itself only emits via `get_in` reveals.
pub fn run(ops: &tamias_ops::Ops) -> io::Result<()> {
    if !io::stdout().is_terminal() {
        return Err(io::Error::other(
            "tamias ui needs a terminal — stdout is not a tty",
        ));
    }
    let mut model = app::Model::open(ops);
    enable_raw_mode()?;
    // Guard first — raw mode is on from here, so every fallible
    // step below must restore on `?`.
    let _guard = Guard;
    install_panic_hook();
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    loop {
        terminal.draw(|f| render::draw(f, &mut model))?;
        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(k) = event::read()? {
                if matches!(k.kind, KeyEventKind::Press | KeyEventKind::Repeat) && model.key(k) {
                    break;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::backend::TestBackend;
    use tamias_core::{Env, Ref, Secret};
    use tamias_vault::Vault;

    /// An Ops over a real temp age vault with secrets stored —
    /// exercises the real load path, not a stub.
    fn ops_with_secrets(envs: &[(&str, &[&str])]) -> (tempfile::TempDir, tamias_ops::Ops) {
        let home = tempfile::tempdir().unwrap();
        let id_path = home.path().join("identity.txt");
        let vault_path = home.path().join("vault.age");
        let (identity, _) =
            tamias_identity::ensure(&tamias_identity_file::FileIdentity::new(&id_path)).unwrap();
        let vault = tamias_vault_age::AgeVault::open(&vault_path, identity, &[]).unwrap();
        for (env, refs) in envs {
            for r in *refs {
                Vault::put(
                    &vault,
                    &Env::new(env).unwrap(),
                    &Ref::new(r).unwrap(),
                    Secret::new("fake-tui-value"),
                )
                .unwrap();
            }
        }
        // Ops fields are private — point it at a temp tamias.toml so
        // Ops::load resolves like the CLI does.
        let cfg_path = home.path().join("tamias.toml");
        std::fs::write(
            &cfg_path,
            format!(
                "[vaults.local]\nkind = \"age\"\npath = {:?}\nidentity = {:?}\n",
                vault_path, id_path
            ),
        )
        .unwrap();
        let ops = tamias_ops::Ops::load(tamias_ops::LoadOpts {
            config_path: Some(cfg_path.to_string_lossy().into_owned()),
            env: None,
            actor: Some("tui-test".into()),
        })
        .unwrap();
        (home, ops)
    }

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    /// Buffer → text rows for substring assertions.
    fn screen(term: &Terminal<TestBackend>) -> String {
        let buf = term.backend().buffer();
        let w = buf.area.width as usize;
        buf.content()
            .iter()
            .map(|c| c.symbol())
            .collect::<Vec<_>>()
            .chunks(w)
            .map(|row| row.concat())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn renders_refs_and_never_values() {
        let (_h, ops) =
            ops_with_secrets(&[("dev", &["tamias://billing/stripe", "tamias://db/pg"])]);
        let mut m = app::Model::open(&ops);
        let backend = TestBackend::new(100, 24);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| render::draw(f, &mut m)).unwrap();
        let s = screen(&term);
        assert!(s.contains("tamias://billing/stripe"), "{s}");
        assert!(s.contains("tamias://db/pg"), "{s}");
        assert!(s.contains("sealed"), "{s}");
        assert!(!s.contains("fake-tui-value"), "{s}");
    }

    #[test]
    fn env_tabs_and_filter_and_help() {
        let (_h, ops) = ops_with_secrets(&[
            ("dev", &["tamias://billing/stripe"]),
            ("prod", &["tamias://prodonly/key"]),
        ]);
        let mut m = app::Model::open(&ops);
        let mut term = Terminal::new(TestBackend::new(100, 24)).unwrap();
        // prod tab via digit key — tabs are dev(1) staging(2) prod(3)
        m.key(key('3'));
        term.draw(|f| render::draw(f, &mut m)).unwrap();
        let s = screen(&term);
        assert!(s.contains("tamias://prodonly/key"), "{s}");
        assert!(!s.contains("stripe"), "{s}");
        // filter narrows
        m.key(key('/'));
        for c in "nomatch".chars() {
            m.key(key(c));
        }
        m.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        term.draw(|f| render::draw(f, &mut m)).unwrap();
        let s = screen(&term);
        assert!(s.contains("nothing matches"), "{s}");
        // help overlay
        m.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        m.key(key('?'));
        term.draw(|f| render::draw(f, &mut m)).unwrap();
        let s = screen(&term);
        assert!(s.contains("keys"), "{s}");
        // q quits
        m.key(key('?'));
        assert!(m.key(key('q')));
    }

    #[test]
    fn broken_vault_renders_error_not_panic() {
        let home = tempfile::tempdir().unwrap();
        let cfg_path = home.path().join("tamias.toml");
        // Points at a vault path that can't open — missing identity.
        std::fs::write(
            &cfg_path,
            format!(
                "[vaults.local]\nkind = \"age\"\npath = {:?}\nidentity = {:?}\n",
                home.path().join("vault.age"),
                home.path().join("no-identity.txt"),
            ),
        )
        .unwrap();
        let ops = tamias_ops::Ops::load(tamias_ops::LoadOpts {
            config_path: Some(cfg_path.to_string_lossy().into_owned()),
            env: None,
            actor: Some("tui-test".into()),
        })
        .unwrap();
        let mut m = app::Model::open(&ops);
        let mut term = Terminal::new(TestBackend::new(100, 24)).unwrap();
        term.draw(|f| render::draw(f, &mut m)).unwrap();
        let s = screen(&term);
        assert!(s.contains("vault not readable"), "{s}");
    }

    #[test]
    fn rotated_entries_show_their_rotated_at_stamp() {
        let (_h, ops) = ops_with_secrets(&[("dev", &["tamias://x/y"])]);
        ops.rotate(&Ref::new("tamias://x/y").unwrap(), Secret::new("fake-2"))
            .unwrap();
        let m = app::Model::open(&ops);
        let e = m.current().unwrap();
        assert!(e.rotated_at.is_some());
    }

    #[test]
    fn custom_envs_open_sorted() {
        let h = tempfile::tempdir().unwrap();
        let ops = ops_on_config(&h, "[envs]\nzeta = \"local\"\nalpha = \"local\"\n", None);
        let m = app::Model::open(&ops);
        let names: Vec<&str> = m.tabs.iter().map(|t| t.env.as_str()).collect();
        let z = names.iter().position(|n| *n == "zeta").unwrap();
        let a = names.iter().position(|n| *n == "alpha").unwrap();
        assert!(a < z, "custom envs must sort: {names:?}");
    }

    #[test]
    fn the_panic_hook_restores_then_delegates() {
        install_panic_hook();
        let _ = std::panic::catch_unwind(|| panic!("deliberate — hook body ran"));
        // put the default hook back — the installed one would
        // spray alt-screen escapes on any later panic here
        let _ = std::panic::take_hook();
    }

    #[test]
    fn non_tty_declines() {
        // Test stdout in this harness is a pipe — run() must refuse.
        let (_h, ops) = ops_with_secrets(&[("dev", &["tamias://x/y"])]);
        assert!(run(&ops).is_err());
    }

    fn code(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    /// An Ops loaded from a raw config body — for env routing the
    /// canned helpers don't express.
    fn ops_on_config(home: &tempfile::TempDir, body: &str, env: Option<&str>) -> tamias_ops::Ops {
        let cfg_path = home.path().join("tamias.toml");
        std::fs::write(&cfg_path, body).unwrap();
        tamias_ops::Ops::load(tamias_ops::LoadOpts {
            config_path: Some(cfg_path.to_string_lossy().into_owned()),
            env: env.map(str::to_string),
            actor: Some("tui-test".into()),
        })
        .unwrap()
    }

    #[test]
    fn every_key_in_the_map_behaves() {
        let (_h, ops) = ops_with_secrets(&[
            (
                "dev",
                &["tamias://a/one", "tamias://b/two", "tamias://c/three"],
            ),
            ("prod", &["tamias://p/one"]),
        ]);
        let mut m = app::Model::open(&ops);
        assert_eq!(m.tabs.len(), 3);
        assert_eq!(m.active, 0);

        // unbound keys are ignored and never quit
        assert!(!m.key(key('z')));
        assert!(!m.key(code(KeyCode::F(5))));
        // ctrl-c quits like q
        assert!(m.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)));

        // j/k + arrows move and wrap in the filtered view
        m.key(key('j'));
        assert_eq!(m.selected, 1);
        m.key(code(KeyCode::Down));
        assert_eq!(m.selected, 2);
        m.key(key('j')); // wraps to 0
        assert_eq!(m.selected, 0);
        m.key(key('k')); // wraps to the bottom
        assert_eq!(m.selected, 2);
        m.key(code(KeyCode::Up));
        assert_eq!(m.selected, 1);

        // g/G and Home/End jump the ends
        m.key(key('G'));
        assert_eq!(m.selected, 2);
        m.key(code(KeyCode::Home));
        assert_eq!(m.selected, 0);
        m.key(code(KeyCode::End));
        assert_eq!(m.selected, 2);
        m.key(key('g'));
        assert_eq!(m.selected, 0);

        // digit tabs: current is a no-op, valid switches, out-of-range ignored
        m.key(key('1'));
        assert_eq!(m.active, 0);
        m.key(key('9'));
        assert_eq!(m.active, 0);
        m.key(key('3'));
        assert_eq!(m.active, 2); // prod
        assert_eq!(
            std::mem::discriminant(&m.listing),
            std::mem::discriminant(&app::Listing::Ready(vec![]))
        );

        // Tab/BackTab cycle
        m.key(code(KeyCode::Tab));
        assert_eq!(m.active, 0); // wraps prod → dev
        m.key(code(KeyCode::BackTab));
        assert_eq!(m.active, 2);

        // R reloads and says so
        m.key(key('R'));
        assert_eq!(m.status.as_deref(), Some("reloaded"));

        // 'r' on a tab with secrets arms the two-step reveal
        m.key(key('r'));
        assert!(m.status.as_deref().unwrap().contains("press r again"));

        // Esc clears filter, pending reveal and status
        m.filter = "abc".into();
        m.key(code(KeyCode::Esc));
        assert!(m.filter.is_empty());
        assert!(m.status.is_none());

        // '?' opens help; any key closes it
        m.key(key('?'));
        assert!(m.help);
        m.key(key('x'));
        assert!(!m.help);

        // q and ctrl-c quit
        assert!(m.key(key('q')));
        let mut m = app::Model::open(&ops);
        assert!(m.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)));
    }

    #[test]
    fn filter_editing_keys_all_do_something() {
        let (_h, ops) = ops_with_secrets(&[("dev", &["tamias://a/one", "tamias://b/two"])]);
        let mut m = app::Model::open(&ops);
        m.key(key('/'));
        assert!(m.filtering);
        // chars append
        m.key(key('a'));
        m.key(key('/')); // '/' inside filter is a char, not a toggle
        assert_eq!(m.filter, "a/");
        // Backspace edits; Enter keeps the filter; Esc drops it
        m.key(code(KeyCode::Backspace));
        assert_eq!(m.filter, "a");
        m.key(code(KeyCode::Delete)); // unbound-in-filter → ignored
        m.key(code(KeyCode::Enter));
        assert!(!m.filtering);
        assert_eq!(m.filter, "a");
        // reopen and Esc out
        m.key(key('/'));
        m.key(key('x'));
        m.key(code(KeyCode::Esc));
        assert!(!m.filtering);
        assert!(m.filter.is_empty());
    }

    #[test]
    fn reveal_two_step_shows_then_masks() {
        let (_h, ops) = ops_with_secrets(&[("dev", &["tamias://a/one"])]);
        let mut m = app::Model::open(&ops);
        let mut term = Terminal::new(TestBackend::new(100, 24)).unwrap();
        // first 'r' arms — still sealed
        m.key(key('r'));
        term.draw(|f| render::draw(f, &mut m)).unwrap();
        assert!(!screen(&term).contains("fake-tui-value"));
        // second 'r' → audited get_in → the pane can show it
        m.key(key('r'));
        assert!(m.revealed.is_some());
        term.draw(|f| render::draw(f, &mut m)).unwrap();
        let s = screen(&term);
        assert!(s.contains("fake-tui-value"), "{s}");
        // moving the cursor masks it again — never a stale value
        m.key(key('j'));
        assert!(m.revealed.is_none());
        term.draw(|f| render::draw(f, &mut m)).unwrap();
        assert!(!screen(&term).contains("fake-tui-value"));
    }

    #[test]
    fn reveal_reports_missing_and_failing_vaults() {
        // a cf vault whose list claims a ref the get can't produce
        let id_dir = tempfile::tempdir().unwrap();
        let id_path = id_dir.path().join("id.txt");
        tamias_identity::ensure(&tamias_identity_file::FileIdentity::new(&id_path)).unwrap();
        let home = tempfile::tempdir().unwrap();
        // list_records must see the ref first — the stateful wire
        // mock gives a real put; the failure lands only at reveal.
        let rref = tamias_core::Ref::new("tamias://x/y").unwrap();
        for (mode, want) in [("rm", "not stored"), ("kill", "reveal failed")] {
            let mock = tamias_testkit::mocks::vault_wire("fake-tok");
            let ops = ops_on_config(
                &home,
                &format!(
                    "[vaults.cloud]\nkind = \"cf\"\nurl = \"{}\"\ntoken_env = \"TUI_TOK\"\nidentity = {:?}\n\n[envs]\ndev = \"cloud\"\n",
                    mock.url(),
                    id_path
                ),
                None,
            );
            std::env::set_var("TUI_TOK", "fake-tok");
            ops.put(&rref, tamias_core::Secret::new("fake-tui-value"))
                .unwrap();
            let mut m = app::Model::open(&ops);
            if mode == "rm" {
                ops.rm(&rref).unwrap(); // listed, gone by reveal
            } else {
                drop(mock); // reveal's get hits a dead backend
            }
            m.key(key('r'));
            m.key(key('r'));
            let st = m.status.clone().unwrap_or_default();
            assert!(st.contains(want), "want {want:?} in {st:?}");
            assert!(m.revealed.is_none());
        }
    }

    #[test]
    fn reveal_on_a_broken_listing_says_nothing_to_reveal() {
        let home = tempfile::tempdir().unwrap();
        // vault that can't open → Failed listing → no current entry
        let ops = ops_on_config(
            &home,
            &format!(
                "[vaults.local]\nkind = \"age\"\npath = {:?}\nidentity = {:?}\n",
                home.path().join("vault.age"),
                home.path().join("no-id.txt")
            ),
            None,
        );
        let mut m = app::Model::open(&ops);
        assert_eq!(
            std::mem::discriminant(&m.listing),
            std::mem::discriminant(&app::Listing::Failed(String::new()))
        );
        assert!(m.current().is_none());
        m.key(key('r'));
        assert_eq!(m.status.as_deref(), Some("nothing to reveal"));
        // j on an empty view is a no-op, not a panic
        m.key(key('j'));
        assert_eq!(m.selected, 0);
    }

    #[test]
    fn custom_envs_and_unlisted_selection_get_tabs() {
        let home = tempfile::tempdir().unwrap();
        // ci is configured; the selected env zz is neither known
        // nor configured — it still gets a tab appended
        let ops = ops_on_config(
            &home,
            "[vaults.local]\nkind = \"age\"\n\n[envs]\nci = \"local\"\n",
            Some("zz"),
        );
        let m = app::Model::open(&ops);
        let names: Vec<&str> = m.tabs.iter().map(|t| t.env.as_str()).collect();
        assert_eq!(names, ["dev", "staging", "prod", "ci", "zz"]);
        assert_eq!(m.tabs[m.active].env.as_str(), "zz");
        // configured vaults surface kind + location for the pane
        let dev = &m.tabs[0];
        assert_eq!(dev.kind, "age");
        assert_eq!(dev.vault, "local");
    }

    #[test]
    fn detail_pane_shows_default_paths_when_unconfigured() {
        // an unconfigured `local` reports no configured location —
        // the detail pane says "default paths", honestly
        let home = tempfile::tempdir().unwrap();
        let cfg_home = tempfile::tempdir().unwrap();
        temp_env::with_vars(
            [
                ("HOME", Some(home.path().to_str().unwrap())),
                (
                    "XDG_CONFIG_HOME",
                    Some(home.path().join(".config").to_str().unwrap()),
                ),
                (
                    "XDG_DATA_HOME",
                    Some(home.path().join(".local/share").to_str().unwrap()),
                ),
                ("TAMIAS_IDENTITY_FILE", None::<&str>),
            ],
            || {
                // seed a secret at the *default* vault location
                let src = tamias_identity_file::FileIdentity::default();
                let v_path = tamias_vault_age::AgeVault::default_vault_path();
                let (identity, _) = tamias_identity::ensure(&src).unwrap();
                tamias_vault_age::AgeVault::open(&v_path, identity, &[])
                    .unwrap()
                    .put(
                        &Env::dev(),
                        &Ref::new("tamias://x/y").unwrap(),
                        Secret::new("fake"),
                    )
                    .unwrap();
                // config with NO [vaults.local] — defaults only
                let ops = ops_on_config(&cfg_home, "[envs]\ndev = \"local\"\n", None);
                let mut m = app::Model::open(&ops);
                let mut term = Terminal::new(TestBackend::new(100, 24)).unwrap();
                term.draw(|f| render::draw(f, &mut m)).unwrap();
                let s = screen(&term);
                assert!(s.contains("default paths"), "{s}");
            },
        );
    }

    #[test]
    fn empty_and_tiny_and_narrow_frames_render() {
        let (_h, ops) = ops_with_secrets(&[]);
        let mut m = app::Model::open(&ops);
        // full frame on an empty env
        let mut term = Terminal::new(TestBackend::new(100, 24)).unwrap();
        term.draw(|f| render::draw(f, &mut m)).unwrap();
        assert!(screen(&term).contains("no secrets stored"));
        // a 2-row terminal drops tabs and footer entirely
        let mut term = Terminal::new(TestBackend::new(100, 2)).unwrap();
        term.draw(|f| render::draw(f, &mut m)).unwrap();
        // 4 rows keeps tabs but drops the footer
        let mut term = Terminal::new(TestBackend::new(100, 4)).unwrap();
        term.draw(|f| render::draw(f, &mut m)).unwrap();
        // under ~72 columns the detail pane folds away
        let (_h, ops) = ops_with_secrets(&[("dev", &["tamias://x/y"])]);
        let mut m = app::Model::open(&ops);
        let mut term = Terminal::new(TestBackend::new(60, 24)).unwrap();
        term.draw(|f| render::draw(f, &mut m)).unwrap();
        assert!(screen(&term).contains("tamias://x/y"));
        // footer: status line and the live filter line
        m.status = Some("fake-status".into());
        term.draw(|f| render::draw(f, &mut m)).unwrap();
        assert!(screen(&term).contains("fake-status"));
        m.status = None;
        m.key(key('/'));
        m.key(key('x'));
        term.draw(|f| render::draw(f, &mut m)).unwrap();
        assert!(screen(&term).contains("/x"));
    }
}
