//! tamias — compat CLI driver. The engine is the `tamias` lib
//! (src/lib.rs); this binary is just `tamias::run()` plus the
//! opt-in OTLP telemetry guard the lib deliberately doesn't own
//! (drivers re-exec `run_from` in-process and must not re-init
//! tracing per call).

fn main() -> std::process::ExitCode {
    // Opt-in OTLP export; with no endpoint this is the plain tracing setup.
    let _telemetry = tamias_telemetry::init();
    match tamias::run() {
        0 => std::process::ExitCode::SUCCESS,
        code => std::process::ExitCode::from(code.clamp(0, 255) as u8),
    }
}
