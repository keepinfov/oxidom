use std::io::{self, Write};

use anyhow::Result;
use oxidom_core::diagnostics::{Diagnostics, Event, FailureKind, Phase};
use oxidom_core::logbook::{LogSource, Severity};

/// Record returned startup failures before the CLI prints its original error.
/// Pre-exec failures, argument parsing, panic payloads and abrupt termination
/// are deliberately not copied into this structured journal.
pub(super) fn run(options: crate::daemon::DaemonOptions) -> Result<()> {
    let mut diagnostics = DaemonLog::new();
    diagnostics.phase(Phase::Starting);
    let result = crate::daemon::run(options, &mut diagnostics);
    if let Err(error) = &result {
        diagnostics.problem(error, true);
    }
    result
}

pub(super) struct DaemonLog {
    sink: Option<Diagnostics>,
    phase: Phase,
    warned: bool,
}

impl DaemonLog {
    fn new() -> Self {
        let sink = oxidom_core::paths::daemon_log_file()
            .ok()
            .and_then(|path| Diagnostics::open(path).ok());
        let mut diagnostics = Self {
            sink,
            phase: Phase::Starting,
            warned: false,
        };
        if diagnostics.sink.is_none() {
            diagnostics.unavailable();
        }
        diagnostics
    }

    pub(super) fn phase(&mut self, phase: Phase) {
        self.phase = phase;
        self.record(Event::Phase { phase });
    }

    pub(super) fn problem(&mut self, error: &anyhow::Error, fatal: bool) {
        let (kind, os_code) = classify(error);
        self.record(Event::Problem {
            phase: self.phase,
            kind,
            os_code,
            fatal,
        });
    }

    pub(super) fn catalog_load_warnings(&mut self, count: usize) {
        if count != 0 {
            self.record(Event::CatalogLoadWarnings {
                count: u32::try_from(count).unwrap_or(u32::MAX),
            });
        }
    }

    fn record(&mut self, event: Event) {
        if let Some(sink) = &self.sink
            && sink.write(event).is_err()
        {
            self.unavailable();
        }
    }

    fn unavailable(&mut self) {
        if self.warned {
            return;
        }
        self.warned = true;
        const WARNING: &str =
            "persistent daemon diagnostics unavailable; original stderr remains enabled";
        // Neither path re-enters the sink or depends on RUST_LOG. A broken
        // fallback stream must not replace the daemon's original outcome.
        oxidom_core::logbook::global().push(
            LogSource::Oxidom,
            Severity::Warn,
            None,
            "oxidom::diagnostics",
            WARNING.to_string(),
        );
        let _ = writeln!(io::stderr().lock(), "oxidom: {WARNING}");
    }
}

fn classify(error: &anyhow::Error) -> (FailureKind, Option<i32>) {
    for cause in error.chain() {
        if matches!(
            cause.downcast_ref::<std::fs::TryLockError>(),
            Some(std::fs::TryLockError::WouldBlock)
        ) {
            return (FailureKind::StoreBusy, None);
        }
        if let Some(error) = cause.downcast_ref::<io::Error>() {
            return classify_io(error);
        }
        if let Some(error) = cause.downcast_ref::<nix::errno::Errno>() {
            return classify_io(&io::Error::from_raw_os_error(*error as i32));
        }
        if let Some(error) = cause.downcast_ref::<zbus::Error>() {
            return match error {
                zbus::Error::InputOutput(error) => classify_io(error),
                _ => (FailureKind::Bus, None),
            };
        }
    }
    (FailureKind::Other, None)
}

fn classify_io(error: &io::Error) -> (FailureKind, Option<i32>) {
    let kind = match error.kind() {
        io::ErrorKind::PermissionDenied => FailureKind::PermissionDenied,
        io::ErrorKind::NotFound => FailureKind::NotFound,
        io::ErrorKind::AlreadyExists => FailureKind::AlreadyExists,
        io::ErrorKind::InvalidInput => FailureKind::InvalidInput,
        io::ErrorKind::InvalidData => FailureKind::InvalidData,
        io::ErrorKind::StorageFull => FailureKind::StorageFull,
        io::ErrorKind::ReadOnlyFilesystem => FailureKind::ReadOnlyFilesystem,
        io::ErrorKind::WriteZero => FailureKind::WriteZero,
        io::ErrorKind::Interrupted => FailureKind::Interrupted,
        _ => FailureKind::OtherIo,
    };
    (kind, error.raw_os_error())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_categories_never_copy_error_text_or_context() {
        let error = anyhow::anyhow!("password = invented-opaque-value")
            .context("https://provider.invalid/invented-subscription-token");
        let (kind, os_code) = classify(&error);
        let record = serde_json::to_string(&Event::Problem {
            phase: Phase::Store,
            kind,
            os_code,
            fatal: true,
        })
        .unwrap();
        assert_eq!(
            record,
            r#"{"event":"problem","phase":"store","kind":"other","os_code":null,"fatal":true}"#
        );
        let io_error = anyhow::Error::new(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "uuid=11111111-1111-4111-8111-111111111111",
        ))
        .context("another invented private context");
        assert_eq!(classify(&io_error), (FailureKind::PermissionDenied, None));
    }

    #[test]
    fn typed_store_and_bus_errors_keep_safe_causes_without_formatting() {
        let busy = anyhow::Error::new(std::fs::TryLockError::WouldBlock)
            .context("invented store location");
        assert_eq!(classify(&busy), (FailureKind::StoreBusy, None));
        let bus = anyhow::Error::new(zbus::Error::Address("invented private address".into()));
        assert_eq!(classify(&bus), (FailureKind::Bus, None));
        let full = anyhow::Error::new(nix::errno::Errno::ENOSPC);
        assert_eq!(
            classify(&full),
            (FailureKind::StorageFull, Some(nix::libc::ENOSPC))
        );
        let readonly = anyhow::Error::new(io::Error::from_raw_os_error(nix::libc::EROFS));
        assert_eq!(
            classify(&readonly),
            (FailureKind::ReadOnlyFilesystem, Some(nix::libc::EROFS))
        );
    }
}
