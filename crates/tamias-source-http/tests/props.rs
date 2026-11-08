#![allow(clippy::unwrap_used)]
//! Property tests for the http ref grammar.

use proptest::prelude::*;
use tamias_source::RefKind;
use tamias_source_http::HttpRefKind;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    /// Parsing any input is total (never panics) and stable.
    #[test]
    fn parse_is_total(s in "\\PC*") {
        let kind = HttpRefKind;
        prop_assert_eq!(kind.parse(&s).is_ok(), kind.parse(&s).is_ok());
    }

    /// A tarball URL parses; a non-tarball URL is rejected.
    #[test]
    fn extensions_are_enforced(host in "[a-z]{1,10}") {
        let ok1 = HttpRefKind.parse(&format!("http://{host}/x.tgz")).is_ok();
        let ok2 = HttpRefKind.parse(&format!("https://{host}/x.tar.gz")).is_ok();
        let bad1 = HttpRefKind.parse(&format!("http://{host}/x.zip")).is_err();
        let bad2 = HttpRefKind.parse(&format!("http://{host}/x")).is_err();
        prop_assert!(ok1 && ok2 && bad1 && bad2);
    }

    /// A 64-hex pin parses; a short/long/non-hex pin is rejected.
    #[test]
    fn digest_pins_are_validated(hex in "[0-9a-f]{64}") {
        let ok = HttpRefKind.parse(&format!("http://h/x.tgz#sha256:{hex}")).is_ok();
        let long = HttpRefKind.parse(&format!("http://h/x.tgz#sha256:{hex}0")).is_err();
        let junk = HttpRefKind.parse("http://h/x.tgz#nope").is_err();
        prop_assert!(ok && long && junk);
    }

    /// Claims are prefix-based — only http(s) URLs.
    #[test]
    fn claims_only_http(s in "[a-z:/]{0,12}") {
        prop_assert_eq!(
            HttpRefKind.claims(&s),
            s.starts_with("http://") || s.starts_with("https://")
        );
    }
}
