//! Property tests for registry-ref parsing.

use proptest::prelude::*;
use tamias_registry::parse_registry_ref;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    /// Parsing any registry ref is total (never panics).
    #[test]
    fn parse_never_panics(s in "\\PC*") {
        let v = parse_registry_ref(&s);
        let _ = format!("{v:?}");
    }

    /// An http(s) base is recognized as the http arm.
    #[test]
    fn http_bases_are_http(host in "[a-z]{1,10}") {
        let s = format!("https://{host}");
        prop_assert_eq!(
            format!("{:?}", parse_registry_ref(&s)).contains("Http"),
            true
        );
    }
}
