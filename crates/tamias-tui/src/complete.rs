//! Command completion for the `:` ex-style prompt.
//!
//! [`complete`] is pure and self-contained: it takes the current
//! command-line buffer (the text after `:`, or with the `:` still
//! attached — a leading colon is stripped defensively) and returns
//! the candidates that fit the token under the cursor. Static
//! candidates come from the [`VERBS`] table + fixed arg vocab;
//! [`complete_with`] additionally accepts caller-supplied dynamic
//! candidates (installed labels, registry names, unit names) which
//! are prefix-filtered and offered only at arg positions the static
//! grammar leaves free — see [`dynamic_ok`].
//!
//! The verb table mirrors `app.rs`'s `parse_command` exactly: same
//! aliases, same fixed-vocabulary positions (`:service` verbs,
//! `:sync --from-lock`, `:clean all|--all`, `:logs` flags). If the
//! grammar grows, grow [`VERBS`] in the same change.

/// One completion candidate for the `:` prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// Text to insert in place of the token being completed — for
    /// verbs that take arguments this includes a trailing space so
    /// the next arg position is already open.
    pub insert: String,
    /// Short human description shown beside the candidate.
    pub hint: String,
}

/// What a verb's argument positions look like — drives which tokens
/// get static candidates. Mirrors the `rest` handling in
/// `parse_command`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArgKind {
    /// The verb takes no arguments (`:q`, `:help`, `:services`, …).
    None,
    /// Free-text argument(s) — nothing static to offer (`:install`,
    /// `:remove`, `:run`, …).
    Free,
    /// `:service <verb> <unit>` — token 2 is a fixed vocabulary,
    /// token 3 (the unit) is free text.
    Service,
    /// `:sync [--from-lock]` — token 2 is the only flag.
    Sync,
    /// `:clean [all|--all]` / `:cache` — token 2 is the only arg.
    Clean,
    /// `:logs <name> [-f|-F|--follow]` — token 2 is a free-text name,
    /// token 3 the follow flags.
    Logs,
    /// `:prune [--keep N]` — token 2 is the only flag.
    Prune,
}

/// One row of the verb table: an alias, its arg shape, and the hint
/// shown in completion. Order is the display order.
struct VerbSpec {
    /// The canonical/alias spelling `parse_command` accepts.
    alias: &'static str,
    /// Argument shape.
    kind: ArgKind,
    /// Completion hint.
    hint: &'static str,
}

/// The `:` verb table — every alias `parse_command` matches, with
/// the same no-arg/free/fixed-arg classification.
const VERBS: &[VerbSpec] = &[
    VerbSpec {
        alias: "q",
        kind: ArgKind::None,
        hint: "quit the TUI",
    },
    VerbSpec {
        alias: "quit",
        kind: ArgKind::None,
        hint: "quit the TUI",
    },
    VerbSpec {
        alias: "q!",
        kind: ArgKind::None,
        hint: "quit the TUI",
    },
    VerbSpec {
        alias: "r",
        kind: ArgKind::None,
        hint: "reload the store listing",
    },
    VerbSpec {
        alias: "reload",
        kind: ArgKind::None,
        hint: "reload the store listing",
    },
    VerbSpec {
        alias: "help",
        kind: ArgKind::None,
        hint: "keybind overlay",
    },
    VerbSpec {
        alias: "?",
        kind: ArgKind::None,
        hint: "keybind overlay",
    },
    VerbSpec {
        alias: "log",
        kind: ArgKind::None,
        hint: "op-log sidebar",
    },
    VerbSpec {
        alias: "install",
        kind: ArgKind::Free,
        hint: "install a ref — usage: :install <ref>",
    },
    VerbSpec {
        alias: "i",
        kind: ArgKind::Free,
        hint: "install a ref — usage: :install <ref>",
    },
    VerbSpec {
        alias: "remove",
        kind: ArgKind::Free,
        hint: "remove a package — usage: :remove <name>",
    },
    VerbSpec {
        alias: "uninstall",
        kind: ArgKind::Free,
        hint: "full provider teardown — usage: :uninstall <name>",
    },
    VerbSpec {
        alias: "verify",
        kind: ArgKind::Free,
        hint: "verify a package — usage: :verify <name>",
    },
    VerbSpec {
        alias: "update",
        kind: ArgKind::Free,
        hint: "update [name] — bare updates everything",
    },
    VerbSpec {
        alias: "u",
        kind: ArgKind::Free,
        hint: "update [name] — bare updates everything",
    },
    VerbSpec {
        alias: "pin",
        kind: ArgKind::Free,
        hint: "pin a package — usage: :pin <name> <ref>",
    },
    VerbSpec {
        alias: "p",
        kind: ArgKind::Free,
        hint: "pin a package — usage: :pin <name> <ref>",
    },
    VerbSpec {
        alias: "search",
        kind: ArgKind::Free,
        hint: "search the registry — usage: :search <q>",
    },
    VerbSpec {
        alias: "registry",
        kind: ArgKind::Free,
        hint: "set the search/install target — usage: :registry [name]",
    },
    VerbSpec {
        alias: "clean",
        kind: ArgKind::Clean,
        hint: "clean the fetch cache — usage: :clean [all|--all]",
    },
    VerbSpec {
        alias: "cache",
        kind: ArgKind::Clean,
        hint: "clean the fetch cache — usage: :clean [all|--all]",
    },
    VerbSpec {
        alias: "doctor",
        kind: ArgKind::None,
        hint: "environment diagnostics",
    },
    VerbSpec {
        alias: "d",
        kind: ArgKind::None,
        hint: "environment diagnostics",
    },
    VerbSpec {
        alias: "services",
        kind: ArgKind::None,
        hint: "refresh provider-service states",
    },
    VerbSpec {
        alias: "service",
        kind: ArgKind::Service,
        hint: "control a unit — usage: :service <verb> <unit>",
    },
    VerbSpec {
        alias: "run",
        kind: ArgKind::Free,
        hint: "run a provider service — usage: :run <pkg> [service]",
    },
    VerbSpec {
        alias: "logs",
        kind: ArgKind::Logs,
        hint: "tail a journal — usage: :logs <pkg|unit> [-f]",
    },
    VerbSpec {
        alias: "deploy",
        kind: ArgKind::Free,
        hint: "deploy a package — usage: :deploy <name>",
    },
    VerbSpec {
        alias: "pack",
        kind: ArgKind::Free,
        hint: "pack a bundle — usage: :pack <dir> [out]",
    },
    VerbSpec {
        alias: "publish",
        kind: ArgKind::Free,
        hint: "publish a bundle — usage: :publish <dir> [registry]",
    },
    VerbSpec {
        alias: "registries",
        kind: ArgKind::None,
        hint: "list configured registries",
    },
    VerbSpec {
        alias: "registry-list",
        kind: ArgKind::None,
        hint: "list configured registries",
    },
    VerbSpec {
        alias: "manifest",
        kind: ArgKind::None,
        hint: "(re)load the project manifest",
    },
    VerbSpec {
        alias: "sync",
        kind: ArgKind::Sync,
        hint: "reconcile project deps — usage: :sync [--from-lock]",
    },
    VerbSpec {
        alias: "s",
        kind: ArgKind::Sync,
        hint: "reconcile project deps — usage: :sync [--from-lock]",
    },
    VerbSpec {
        alias: "prune",
        kind: ArgKind::Prune,
        hint: "drop superseded store versions — usage: :prune [--keep N]",
    },
];

/// `:service` token-2 vocabulary — must match the whitelist in
/// `parse_command`.
const SERVICE_ACTIONS: &[&str] = &["start", "stop", "restart", "enable", "disable"];

/// `:sync` token-2 vocabulary.
const SYNC_FLAGS: &[&str] = &["--from-lock"];

/// `:clean`/`:cache` token-2 vocabulary.
const CLEAN_ARGS: &[&str] = &["all", "--all"];

/// `:logs` token-3 vocabulary — the flags `parse_command` scans for
/// anywhere after the name.
const LOGS_FLAGS: &[&str] = &["-f", "-F", "--follow"];

/// `:prune` token-2 vocabulary — `parse_command` also accepts
/// `--keep=N` and a separate value token.
const PRUNE_FLAGS: &[&str] = &["--keep"];

/// Look up a verb's arg shape by its (lowercased) first token.
fn arg_kind(verb: &str) -> Option<ArgKind> {
    VERBS
        .iter()
        .find(|v| v.alias.eq_ignore_ascii_case(verb))
        .map(|v| v.kind)
}

/// Build candidates for a fixed-vocabulary arg position: prefix-
/// filtered, case-insensitive, canonical lowercase inserts.
/// `trailing_space` appends `" "` to each insert (used where more
/// args follow, e.g. `:service <verb> ` → unit position).
fn arg_candidates(
    words: &[&str],
    prefix: &str,
    trailing_space: bool,
    hint: &str,
) -> Vec<Candidate> {
    let prefix = prefix.to_lowercase();
    words
        .iter()
        .filter(|w| w.to_lowercase().starts_with(&prefix))
        .map(|w| Candidate {
            insert: if trailing_space {
                format!("{w} ")
            } else {
                (*w).to_string()
            },
            hint: hint.to_string(),
        })
        .collect()
}

/// Complete the `:` command line.
///
/// `line` is the buffer content — the leading `:` may be present or
/// absent; surrounding whitespace is ignored for tokenizing but a
/// trailing space means "completing a fresh token".
///
/// Returns candidates for the token under completion:
///
/// - Verb position (empty buffer, or the first token): every alias
///   whose lowercase form starts with the typed prefix, in table
///   order. Verbs that take arguments insert with a trailing space;
///   no-arg verbs insert bare.
/// - `:service` token 2 → `start|stop|restart|enable|disable`
///   (trailing space — the unit follows).
/// - `:sync`/`:s` token 2 → `--from-lock`.
/// - `:clean`/`:cache` token 2 → `all`, `--all`.
/// - `:logs` token 3 → `-f`, `-F`, `--follow` (token 2, the name,
///   is free text).
/// - Anything else — unknown verbs, free-text arg positions, args
///   past the fixed vocabulary — yields an empty vec.
pub fn complete(line: &str) -> Vec<Candidate> {
    complete_with(line, &[])
}

/// [`complete`] plus caller-supplied `dynamic` candidates.
///
/// Static candidates always win: dynamic entries are consulted only
/// when the position under completion yields no static candidates
/// *and* [`dynamic_ok`] says the verb's arg shape has a free slot
/// there — `Free` verbs at any arg position, `:service` at the unit
/// position (≥2), `:logs` at the name position (1). The dynamic list
/// is prefix-filtered case-insensitively against the token being
/// completed (matching on `insert` minus its trailing space); the
/// caller owns ordering, dedupe, and the size cap.
pub fn complete_with(line: &str, dynamic: &[Candidate]) -> Vec<Candidate> {
    let body = line.trim_start();
    let body = match body.strip_prefix(':') {
        Some(rest) => rest.trim_start(),
        None => body,
    };
    let tokens: Vec<&str> = body.split_whitespace().collect();
    // A trailing whitespace char means the cursor sits on a fresh,
    // empty token *after* the last word.
    let fresh = body.chars().last().is_none_or(|c| c.is_whitespace());

    if tokens.is_empty() {
        // Empty/whitespace-only buffer — every verb, no prefix.
        return verb_candidates("");
    }

    // Position of the token being completed: `fresh` means one past
    // the last token.
    let (position, prefix) = if fresh {
        (tokens.len(), "")
    } else {
        (tokens.len() - 1, tokens[tokens.len() - 1])
    };

    if position == 0 {
        return verb_candidates(prefix);
    }

    let Some(kind) = arg_kind(tokens[0]) else {
        return Vec::new();
    };
    // The verb sits at token 0, so the token index *is* the 1-based
    // arg index (`:service <verb>` completes at position 1, etc.).
    let static_out = match (kind, position) {
        (ArgKind::Service, 1) => arg_candidates(
            SERVICE_ACTIONS,
            prefix,
            true,
            "unit action — :service <verb> <unit>",
        ),
        (ArgKind::Sync, 1) => arg_candidates(
            SYNC_FLAGS,
            prefix,
            false,
            "install tamias.lock entries verbatim",
        ),
        (ArgKind::Clean, 1) => arg_candidates(
            CLEAN_ARGS,
            prefix,
            false,
            "clear everything, not just stale entries",
        ),
        (ArgKind::Logs, 2) => arg_candidates(LOGS_FLAGS, prefix, false, "follow the journal"),
        (ArgKind::Prune, 1) => arg_candidates(
            PRUNE_FLAGS,
            prefix,
            true,
            "versions kept per package — :prune --keep <N>",
        ),
        // Free-text positions, positions past the fixed vocabulary,
        // and no-arg verbs with stray args all yield nothing static.
        _ => Vec::new(),
    };
    if !static_out.is_empty() || !dynamic_ok(kind, position) {
        return static_out;
    }
    let prefix = prefix.to_lowercase();
    dynamic
        .iter()
        .filter(|c| c.insert.trim_end().to_lowercase().starts_with(&prefix))
        .cloned()
        .collect()
}

/// Whether `dynamic` candidates may surface at `(kind, position)` —
/// the arg slots the static grammar leaves free:
///
/// - `Free` verbs: every arg position (`:remove <name>`, `:pin`'s
///   name *and* ref slots, …).
/// - `Service`: position ≥ 2 — the unit name (position 1 is the
///   fixed action vocabulary).
/// - `Logs`: position 1 — the package/unit name (position 2 is the
///   flag vocabulary).
/// - `Sync`/`Clean`/`None`: never — their slots are fixed or stray.
fn dynamic_ok(kind: ArgKind, position: usize) -> bool {
    match kind {
        ArgKind::Free => true,
        ArgKind::Service => position >= 2,
        ArgKind::Logs => position == 1,
        ArgKind::Sync | ArgKind::Clean | ArgKind::Prune | ArgKind::None => false,
    }
}

/// Verb-position candidates: every alias matching `prefix`
/// (case-insensitive), canonical lowercase, trailing space when the
/// verb takes arguments.
fn verb_candidates(prefix: &str) -> Vec<Candidate> {
    let prefix = prefix.to_lowercase();
    VERBS
        .iter()
        .filter(|v| v.alias.to_lowercase().starts_with(&prefix))
        .map(|v| Candidate {
            insert: match v.kind {
                ArgKind::None => v.alias.to_string(),
                _ => format!("{} ", v.alias),
            },
            hint: v.hint.to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// The set of `insert` strings — order-insensitive assertions.
    fn inserts(cands: &[Candidate]) -> Vec<String> {
        cands.iter().map(|c| c.insert.clone()).collect()
    }

    #[test]
    fn empty_buffer_lists_all_verbs() {
        for line in ["", "   ", ":", ":  "] {
            let cands = complete(line);
            let got = inserts(&cands);
            // Spot-check coverage across the grammar's shapes.
            for expected in [
                "q",
                "quit",
                "q!",
                "r",
                "reload",
                "help",
                "?",
                "install ",
                "i ",
                "remove ",
                "uninstall ",
                "verify ",
                "update ",
                "u ",
                "pin ",
                "p ",
                "search ",
                "registry ",
                "clean ",
                "cache ",
                "doctor",
                "d",
                "services",
                "service ",
                "run ",
                "logs ",
                "deploy ",
                "pack ",
                "publish ",
                "registries",
                "registry-list",
                "manifest",
                "sync ",
                "s ",
                "prune ",
            ] {
                assert!(
                    got.iter().any(|i| i == expected),
                    "missing {expected:?} for line {line:?}"
                );
            }
            assert_eq!(got.len(), VERBS.len(), "alias count drifted for {line:?}");
        }
    }

    #[test]
    fn verb_prefix_filters() {
        let got = inserts(&complete("se"));
        // Table order: `services` is declared before `service`.
        assert_eq!(got, vec!["search ", "services", "service "]);

        let got = inserts(&complete("ser"));
        assert_eq!(got, vec!["services", "service "]);

        // `r` prefix also catches `registry`, `registries`,
        // `registry-list`, `reload`, `run`, `remove`… — check a few.
        let got = inserts(&complete("reg"));
        assert_eq!(got, vec!["registry ", "registries", "registry-list"]);
    }

    #[test]
    fn verb_matching_is_case_insensitive_canonical_lowercase() {
        let got = inserts(&complete("INS"));
        assert_eq!(got, vec!["install "]);
        let got = inserts(&complete("Ser"));
        assert_eq!(got, vec!["services", "service "]);
        // No-arg verbs insert bare (no trailing space).
        let got = inserts(&complete("HE"));
        assert_eq!(got, vec!["help"]);
    }

    #[test]
    fn service_verb_position() {
        let got = inserts(&complete("service "));
        assert_eq!(
            got,
            vec!["start ", "stop ", "restart ", "enable ", "disable "]
        );
        // Prefix filters the five actions.
        let got = inserts(&complete("service st"));
        assert_eq!(got, vec!["start ", "stop "]);
        let got = inserts(&complete("service RE"));
        assert_eq!(got, vec!["restart "]);
        // Once the action is complete, the unit is free text.
        assert!(complete("service start ").is_empty());
        assert!(complete("service start my-unit ").is_empty());
        // An invalid action still occupies the fixed-vocab slot —
        // prefix filtering yields nothing for it.
        assert!(complete("service bogus ").is_empty());
    }

    #[test]
    fn sync_flag_position() {
        for line in ["sync ", "s ", "sync  "] {
            let got = inserts(&complete(line));
            assert_eq!(got, vec!["--from-lock"], "line {line:?}");
        }
        // Flag already typed in full still completes to itself
        // (harmless re-insert); a trailing space moves past it.
        let got = inserts(&complete("sync --from-lock"));
        assert_eq!(got, vec!["--from-lock"]);
        assert!(complete("sync --from-lock ").is_empty());
        // Prefix-filtered.
        let got = inserts(&complete("sync --f"));
        assert_eq!(got, vec!["--from-lock"]);
    }

    #[test]
    fn clean_arg_position() {
        for verb in ["clean", "cache"] {
            let got = inserts(&complete(&format!("{verb} ")));
            assert_eq!(got, vec!["all", "--all"], "verb {verb}");
        }
        let got = inserts(&complete("clean a"));
        assert_eq!(got, vec!["all"]);
        assert!(complete("clean all ").is_empty());
    }

    #[test]
    fn logs_positions() {
        // Token 2 is the free-text name — nothing offered.
        assert!(complete("logs ").is_empty());
        // Token 3 is the flag position.
        let got = inserts(&complete("logs my-pkg "));
        assert_eq!(got, vec!["-f", "-F", "--follow"]);
        // Mid-token flag filtering; case-insensitive.
        let got = inserts(&complete("logs my-pkg -f"));
        assert_eq!(got, vec!["-f", "-F"]);
        let got = inserts(&complete("logs my-pkg --"));
        assert_eq!(got, vec!["--follow"]);
        // Past the flags there's nothing.
        assert!(complete("logs my-pkg -f ").is_empty());
    }

    #[test]
    fn free_text_verbs_yield_nothing_at_arg_position() {
        for line in [
            "install ",
            "i git:x#main ",
            "remove ",
            "verify w ",
            "update ",
            "pin ",
            "pin w ",
            "search ",
            "registry ",
            "run ",
            "run @s/w ",
            "deploy ",
            "pack ",
            "publish ",
        ] {
            assert!(
                complete(line).is_empty(),
                "expected no candidates for {line:?}"
            );
        }
    }

    #[test]
    fn no_arg_verbs_yield_nothing_after() {
        for line in [
            "q ",
            "quit extra",
            "help x",
            "? y",
            "doctor z",
            "d w",
            "services v",
            "registries r",
            "registry-list x",
            "manifest m",
            "r e",
        ] {
            assert!(
                complete(line).is_empty(),
                "expected no candidates for {line:?}"
            );
        }
    }

    #[test]
    fn unknown_verb_yields_nothing() {
        assert!(complete("bogus").is_empty());
        assert!(complete("bogus ").is_empty());
        assert!(complete("bogus arg").is_empty());
        // Near-miss at verb position filters to empty too.
        assert!(complete("zz").is_empty());
    }

    #[test]
    fn leading_colon_and_whitespace_are_tolerated() {
        assert_eq!(
            inserts(&complete(":se")),
            vec!["search ", "services", "service "]
        );
        assert_eq!(inserts(&complete("  :service st")), vec!["start ", "stop "]);
        // Trailing-space detection survives the colon strip.
        assert_eq!(inserts(&complete(":sync ")), vec!["--from-lock"]);
    }

    #[test]
    fn candidates_carry_hints() {
        let cands = complete("service ");
        assert!(cands.iter().all(|c| !c.hint.is_empty()));
        let cands = complete("");
        assert!(cands.iter().all(|c| !c.hint.is_empty()));
    }

    /// A dynamic candidate factory for `complete_with` tests.
    fn dyn_cands(inserts: &[&str]) -> Vec<Candidate> {
        inserts
            .iter()
            .map(|i| Candidate {
                insert: format!("{i} "),
                hint: "dynamic".to_string(),
            })
            .collect()
    }

    #[test]
    fn dynamic_candidates_fill_free_arg_positions() {
        let dyns = dyn_cands(&["widget@1", "gadget@2"]);
        // `:remove <name>` — Free arg position.
        let got = inserts(&complete_with("remove wid", &dyns));
        assert_eq!(got, vec!["widget@1 "]);
        // Fresh token after the space — both candidates, unfiltered.
        let got = inserts(&complete_with("remove ", &dyns));
        assert_eq!(got, vec!["widget@1 ", "gadget@2 "]);
        // `:pin`'s second arg is Free too — the caller decides what
        // to offer there; the gate doesn't discriminate.
        let got = inserts(&complete_with("pin w ", &dyns));
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn dynamic_candidates_respect_fixed_vocab_positions() {
        let dyns = dyn_cands(&["widget@1"]);
        // `:service` position 1 is the action vocabulary — static
        // wins, dynamic never leaks in even when it prefix-matches.
        let got = inserts(&complete_with("service st", &dyns));
        assert_eq!(got, vec!["start ", "stop "]);
        // …but position ≥2 (the unit) is free for dynamic.
        let got = inserts(&complete_with("service start wi", &dyns));
        assert_eq!(got, vec!["widget@1 "]);
        // `:sync`/`:clean` slots stay static-only.
        assert!(complete_with("sync --f", &dyns)
            .iter()
            .all(|c| c.insert == "--from-lock"));
        assert_eq!(inserts(&complete_with("sync ", &dyns)), vec!["--from-lock"]);
        // Verb position ignores dynamic entirely.
        let got = inserts(&complete_with("rem", &dyns));
        assert_eq!(got, vec!["remove "]);
        // Unknown verbs and no-arg verbs' stray args yield nothing.
        assert!(complete_with("bogus ", &dyns).is_empty());
        assert!(complete_with("q x", &dyns).is_empty());
        // `:logs` name position (1) takes dynamic; the flag position
        // (2) stays static.
        let got = inserts(&complete_with("logs wi", &dyns));
        assert_eq!(got, vec!["widget@1 "]);
        let got = inserts(&complete_with("logs widget -", &dyns));
        assert_eq!(got, vec!["-f", "-F", "--follow"]);
    }

    #[test]
    fn dynamic_prefix_matching_is_case_insensitive() {
        let dyns = dyn_cands(&["Widget@1"]);
        let got = inserts(&complete_with("remove WID", &dyns));
        assert_eq!(got, vec!["Widget@1 "]);
    }
}
