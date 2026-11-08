//! Fuzz every ref-grammar kind: parsing arbitrary bytes must never panic.
#![no_main]

use libfuzzer_sys::fuzz_target;
use tamias_source::RefKind;

fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else {
        return;
    };
    let _ = tamias_source_git::GitRefKind.parse(s);
    let _ = tamias_source_http::HttpRefKind.parse(s);
    let _ = tamias_source_cf::CfRefKind.parse(s);
    let _ = tamias_source_fs::FsRefKind.parse(s);
    let _ = tamias_registry::parse_registry_ref(s);
});
