//! Property tests for the path ref kind.

use proptest::prelude::*;
use tamias_source::RefKind;
use tamias_source_fs::{FsRefKind, FsSpec};

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    /// Any path-shaped ref parses to an absolute path (never panics).
    #[test]
    fn absolute_paths_parse(p in "[a-zA-Z0-9._/-]{1,24}") {
        let s = format!("/{p}");
        prop_assert!(FsRefKind.claims(&s));
        let spec = FsRefKind.parse(&s).unwrap();
        let f = spec.downcast::<FsSpec>().unwrap();
        prop_assert!(f.path.is_absolute());
        prop_assert_eq!(f.path.to_string_lossy().to_string(), s);
    }

    /// Claims are path-shaped only — a bare name is not a path.
    #[test]
    fn bare_names_are_not_paths(name in "[a-z]{1,10}") {
        prop_assert!(!FsRefKind.claims(&name));
    }
}
