//! Fuzz the tar/gzip unpacker: arbitrary bytes must error, never panic.
#![no_main]

use std::path::Path;

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(dest) = tempfile::tempdir() {
        let _ = tamias_core::untar(data, dest.path());
        let _ = tamias_core::untar_gz(data, dest.path());
    }
    let _ = Path::new("/nonexistent");
});
