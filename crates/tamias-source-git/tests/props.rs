//! Property tests for the `git:` grammar.

use proptest::prelude::*;
use tamias_source::RefKind;
use tamias_source_git::{GitRefKind, GitSpec};

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    /// Parsing any `git:` body is total + deterministic (never panics).
    #[test]
    fn parse_is_total(body in "\\PC*") {
        let kind = GitRefKind;
        let s = format!("git:{body}");
        let a = kind.parse(&s).is_ok();
        prop_assert_eq!(a, kind.parse(&s).is_ok());
    }

    /// An accepted shorthand always yields a real clone URL.
    #[test]
    fn accepted_refs_have_a_clone_url(org in "[a-z]{1,8}", repo in "[a-z]{1,8}") {
        let want = format!("{org}/{repo}");
        let spec = GitRefKind.parse(&format!("git:{want}")).unwrap();
        let g = spec.downcast::<GitSpec>().unwrap();
        prop_assert!(g.url.starts_with("https://"));
        prop_assert!(g.url.ends_with(&want));
    }

    /// A full sha rev is accepted and preserved.
    #[test]
    fn full_sha_revs_round_trip(sha in "[0-9a-f]{40}") {
        let want = sha.clone();
        let spec = GitRefKind.parse(&format!("git:o/r#{sha}")).unwrap();
        let g = spec.downcast::<GitSpec>().unwrap();
        prop_assert_eq!(g.rev.as_deref(), Some(want.as_str()));
    }

    /// In-repo paths never escape (`..` is rejected).
    #[test]
    fn traversal_is_rejected(sub in "[a-z]{1,6}") {
        let bad = format!("git:o/r//{sub}/../x");
        prop_assert!(GitRefKind.parse(&bad).is_err());
    }
}
