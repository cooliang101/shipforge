//! Explicit, one-shot clipboard requests through the terminal-owned output writer.
//!
//! OSC 52 requires an ANSI terminal and terminal/multiplexer permission. There is
//! no acknowledgement: `Sent` never means that the host clipboard was updated.
//! Non-ANSI Windows consoles are unsupported; this module does not invoke `WinAPI`,
//! external clipboard commands, read the clipboard, or retry a partial write.

use std::io::Write;

use crossterm::{Command, clipboard::CopyToClipboard};

pub(super) const MAX_CLIPBOARD_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ClipboardTransport {
    /// The caller has selected the terminal's ANSI output transport, not proven OSC support.
    Ansi,
    Unsupported,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ClipboardOutcome {
    /// One request was written and flushed. Ask the user to paste to verify it.
    Sent,
    Unsupported,
    Rejected,
    /// Output may be partial. Never automatically resend it or claim success.
    Failed,
}

/// Writes exactly one bounded request when explicitly requested by the user.
///
/// The caller must provide already-redacted text. Validation never silently
/// changes or truncates it. Line feeds and tabs are allowed; other terminal
/// controls and invisible directional formatting are rejected before any I/O.
pub(super) fn request_copy(
    output: &mut impl Write,
    redacted_text: &str,
    transport: ClipboardTransport,
) -> ClipboardOutcome {
    if redacted_text.len() > MAX_CLIPBOARD_BYTES || redacted_text.chars().any(unsafe_character) {
        return ClipboardOutcome::Rejected;
    }
    if transport == ClipboardTransport::Unsupported {
        return ClipboardOutcome::Unsupported;
    }
    // Build the whole request before using the shared buffered terminal writer.
    // Do not use `execute!`: its Windows fallback can bypass this injected writer.
    let mut request = String::new();
    if CopyToClipboard::to_clipboard_from(redacted_text)
        .write_ansi(&mut request)
        .is_err()
    {
        return ClipboardOutcome::Failed;
    }
    if output
        .write_all(request.as_bytes())
        .and_then(|()| output.flush())
        .is_err()
    {
        ClipboardOutcome::Failed
    } else {
        ClipboardOutcome::Sent
    }
}

fn unsafe_character(value: char) -> bool {
    (value.is_control() && !matches!(value, '\n' | '\t'))
        || matches!(value, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}')
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;

    #[derive(Default)]
    struct Writer {
        bytes: Vec<u8>,
        writes: usize,
        flushes: usize,
        fail_write: bool,
        fail_flush: bool,
    }

    impl Write for Writer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.writes += 1;
            if self.fail_write {
                return Err(io::Error::other("private diagnostic must not escape"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            if self.fail_flush {
                Err(io::Error::other("private diagnostic must not escape"))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn sends_one_request_and_flush_without_claiming_acknowledgement() {
        let mut output = Writer::default();
        assert_eq!(
            request_copy(&mut output, "foo", ClipboardTransport::Ansi),
            ClipboardOutcome::Sent
        );
        assert_eq!(output.bytes, b"\x1b]52;c;Zm9v\x1b\\");
        assert_eq!((output.writes, output.flushes), (1, 1));
    }

    #[test]
    fn preserves_unicode_newlines_tabs_and_redacted_markers() {
        let content = "日志\n\t[REDACTED] 'quoted'";
        let mut expected = String::new();
        CopyToClipboard::to_clipboard_from(content)
            .write_ansi(&mut expected)
            .unwrap();
        let mut output = Vec::new();
        assert_eq!(
            request_copy(&mut output, content, ClipboardTransport::Ansi),
            ClipboardOutcome::Sent
        );
        assert_eq!(output, expected.as_bytes());
        assert!(
            !output
                .windows("日志".len())
                .any(|part| part == "日志".as_bytes())
        );
    }

    #[test]
    fn rejects_controls_directional_text_and_excess_without_output() {
        let oversized = "x".repeat(MAX_CLIPBOARD_BYTES + 1);
        for content in [
            "before\x1b[31mafter",
            "before\0after",
            "before\rafter",
            "before\u{85}after",
            "before\u{202e}after",
            oversized.as_str(),
        ] {
            let mut output = Writer::default();
            assert_eq!(
                request_copy(&mut output, content, ClipboardTransport::Ansi),
                ClipboardOutcome::Rejected
            );
            assert_eq!((output.writes, output.flushes), (0, 0));
        }
        let mut output = Vec::new();
        assert_eq!(
            request_copy(
                &mut output,
                &"x".repeat(MAX_CLIPBOARD_BYTES),
                ClipboardTransport::Ansi
            ),
            ClipboardOutcome::Sent
        );
    }

    #[test]
    fn unsupported_transport_never_writes_even_valid_content() {
        let mut output = Writer::default();
        assert_eq!(
            request_copy(&mut output, "valid", ClipboardTransport::Unsupported),
            ClipboardOutcome::Unsupported
        );
        assert_eq!((output.writes, output.flushes), (0, 0));
    }

    #[test]
    fn write_or_flush_failure_is_static_and_is_not_retried() {
        for (fail_write, fail_flush) in [(true, false), (false, true)] {
            let mut output = Writer {
                fail_write,
                fail_flush,
                ..Writer::default()
            };
            let outcome = request_copy(&mut output, "foo", ClipboardTransport::Ansi);
            assert_eq!(outcome, ClipboardOutcome::Failed);
            assert_eq!(output.writes, 1);
            assert_eq!(output.flushes, usize::from(!fail_write));
            assert!(!format!("{outcome:?}").contains("private"));
        }
    }

    #[test]
    fn partial_output_is_failed_without_resending_or_flushing() {
        #[derive(Default)]
        struct PartialWriter {
            bytes: Vec<u8>,
            calls: usize,
        }

        impl Write for PartialWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.calls += 1;
                if self.calls == 1 {
                    self.bytes.extend_from_slice(&bytes[..2]);
                    Ok(2)
                } else {
                    Err(io::Error::other("output unavailable"))
                }
            }

            fn flush(&mut self) -> io::Result<()> {
                panic!("a failed partial request must not flush or retry");
            }
        }

        let mut output = PartialWriter::default();
        assert_eq!(
            request_copy(&mut output, "foo", ClipboardTransport::Ansi),
            ClipboardOutcome::Failed
        );
        assert_eq!(output.calls, 2);
        assert_eq!(output.bytes, b"\x1b]");
    }
}
