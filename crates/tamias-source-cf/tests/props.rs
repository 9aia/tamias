//! Property tests for the `cf:` grammar.

use proptest::prelude::*;
use tamias_source::RefKind;
use tamias_source_cf::{CfRefKind, CfSpec};

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    /// Parsing any `cf:` body is total (never panics).
    #[test]
    fn parse_is_total(body in "\\PC*") {
        let _ = CfRefKind.parse(&format!("cf:{body}"));
    }

    /// The account/bucket address derives the registry base URL.
    #[test]
    fn base_url_derivation(
        acct in "[a-z]{1,8}", bucket in "[a-z]{1,8}",
        name in "[a-z]{1,8}", ver in "[0-9]{1,3}\\.[0-9]{1,3}\\.[0-9]{1,3}"
    ) {
        let ref_str = format!("cf:{acct}/{bucket}/{name}@{ver}");
        let want_base = format!("https://{acct}.workers.dev/{bucket}");
        let spec = CfRefKind.parse(&ref_str).unwrap();
        let c = spec.downcast::<CfSpec>().unwrap();
        prop_assert_eq!(&c.base_url, &want_base);
        prop_assert_eq!(&c.name, &name);
        prop_assert_eq!(&c.version, &ver);
    }

    /// Any malformed shape (no `@`, no `/`, empty parts) is rejected.
    #[test]
    fn malformed_shapes_are_rejected(name in "[a-z]{1,6}") {
        let no_ver = format!("cf:{name}");
        let no_path = format!("cf:{name}@1.0.0");
        let no_ver2 = format!("cf:a/b/{name}");
        let empty_ver = format!("cf:a/b/{name}@");
        prop_assert!(CfRefKind.parse(&no_ver).is_err());
        prop_assert!(CfRefKind.parse(&no_path).is_err());
        prop_assert!(CfRefKind.parse(&no_ver2).is_err());
        prop_assert!(CfRefKind.parse(&empty_ver).is_err());
    }
}
