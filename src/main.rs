use std::{
    io,
    panic::{AssertUnwindSafe, catch_unwind},
    process::ExitCode,
};

const RUNTIME_FAILURE: &str = "ShipForge could not continue or could not fully restore the terminal. Terminal restoration was attempted; reopen the terminal if input or display is abnormal, then inspect deployment history and remote state before retrying.";
const PANIC_FAILURE: &str = "ShipForge encountered an unexpected error. Terminal restoration was attempted; inspect deployment history and remote state before retrying.";

fn main() -> ExitCode {
    // A hook runs before catch_unwind. Writing here would bypass the TUI's
    // buffered output while raw/alternate-screen mode is still active, and a
    // failed stderr write could turn a recoverable panic into an abort.
    std::panic::set_hook(Box::new(|_| {}));
    let outcome = catch_unwind(AssertUnwindSafe(shipforge::tui::run));
    finish_main(&outcome, &mut io::stderr().lock())
}

fn finish_main(
    outcome: &std::thread::Result<io::Result<()>>,
    diagnostic: &mut impl io::Write,
) -> ExitCode {
    match outcome {
        Ok(Ok(())) => ExitCode::SUCCESS,
        Ok(Err(_)) => {
            let _ = writeln!(diagnostic, "{RUNTIME_FAILURE}");
            ExitCode::FAILURE
        }
        Err(_) => {
            // The panic payload is deliberately never formatted or inspected.
            let _ = writeln!(diagnostic, "{PANIC_FAILURE}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panic_diagnostic_is_fixed_and_never_contains_the_payload() {
        let outcome: std::thread::Result<io::Result<()>> = Err(Box::new("private-panic-payload"));
        let mut output = Vec::new();
        let _ = finish_main(&outcome, &mut output);
        let output = String::from_utf8(output).unwrap();
        assert_eq!(output.trim_end(), PANIC_FAILURE);
        assert!(!output.contains("private-panic-payload"));
    }

    #[test]
    fn diagnostic_write_failure_is_not_retried_or_panicked() {
        struct Broken;

        impl io::Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("controlled failure"))
            }

            fn flush(&mut self) -> io::Result<()> {
                Err(io::Error::other("controlled failure"))
            }
        }

        let status = std::panic::catch_unwind(AssertUnwindSafe(|| {
            finish_main(&Ok(Err(io::Error::other("private error"))), &mut Broken)
        }));
        assert!(status.is_ok());
    }
}
