//! Bounded sensitive-value and PEM filtering across arbitrary output chunks.

use super::Redactor;

struct Pattern {
    bytes: Vec<u8>,
    end_key: Option<Vec<u8>>,
}

pub(crate) struct StreamingRedactor {
    patterns: Vec<Pattern>,
    pending: Vec<u8>,
    private_key_end: Option<Vec<u8>>,
}

impl StreamingRedactor {
    pub(crate) fn new(redactor: &Redactor) -> Self {
        let mut patterns = redactor
            .values()
            .iter()
            .map(|value| Pattern {
                bytes: value.as_bytes().to_vec(),
                end_key: None,
            })
            .collect::<Vec<_>>();
        for label in [
            "PRIVATE KEY",
            "ENCRYPTED PRIVATE KEY",
            "RSA PRIVATE KEY",
            "EC PRIVATE KEY",
            "DSA PRIVATE KEY",
            "OPENSSH PRIVATE KEY",
            "PGP PRIVATE KEY BLOCK",
        ] {
            patterns.push(Pattern {
                bytes: format!("-----BEGIN {label}-----").into_bytes(),
                end_key: Some(format!("-----END {label}-----").into_bytes()),
            });
        }
        patterns.sort_by_key(|pattern| std::cmp::Reverse(pattern.bytes.len()));
        Self {
            patterns,
            pending: Vec::new(),
            private_key_end: None,
        }
    }

    pub(crate) fn push(&mut self, bytes: &[u8], eof: bool) -> Vec<u8> {
        let mut output = Vec::new();
        for byte in bytes {
            if let Some(end) = &self.private_key_end {
                self.pending.push(*byte);
                while !end.starts_with(&self.pending) {
                    self.pending.remove(0);
                }
                if *end == self.pending {
                    self.pending.clear();
                    self.private_key_end = None;
                }
            } else {
                self.pending.push(*byte);
                self.drain(false, &mut output);
            }
        }
        if eof {
            if self.private_key_end.is_none() {
                self.drain(true, &mut output);
            }
            self.pending.clear();
            // Do not reset an unterminated PEM block at an error/EOF boundary:
            // subsequent command output could still contain its private body.
        }
        output
    }

    fn drain(&mut self, eof: bool, output: &mut Vec<u8>) {
        while !self.pending.is_empty() {
            let could_extend = self.patterns.iter().any(|pattern| {
                pattern.bytes.len() > self.pending.len() && pattern.bytes.starts_with(&self.pending)
            });
            if could_extend && !eof {
                return;
            }
            if let Some(pattern) = self
                .patterns
                .iter()
                .find(|pattern| self.pending.starts_with(&pattern.bytes))
            {
                let matched = pattern.bytes.len();
                if let Some(end) = &pattern.end_key {
                    self.private_key_end = Some(end.clone());
                    self.pending.clear();
                    output.extend_from_slice(b"[REDACTED PRIVATE KEY]");
                    return;
                }
                self.pending.drain(..matched);
                output.extend_from_slice(b"[REDACTED]");
            } else if could_extend && eof {
                // Never release an unfinished sensitive-value prefix on error.
                self.pending.clear();
                output.extend_from_slice(b"[REDACTED]");
            } else {
                output.push(self.pending.remove(0));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_multiline_and_overlapping_values_across_every_chunk_boundary() {
        for chunk_size in 1..30 {
            let redactor = Redactor::new([
                "token".into(),
                "token-value".into(),
                "part-a\npart-b".into(),
            ]);
            let mut stream = StreamingRedactor::new(&redactor);
            let mut output = Vec::new();
            for chunk in b"before token-value part-a\npart-b after".chunks(chunk_size) {
                output.extend(stream.push(chunk, false));
            }
            output.extend(stream.push(&[], true));
            assert_eq!(
                String::from_utf8(output).unwrap(),
                "before [REDACTED] [REDACTED] after"
            );
        }
    }

    #[test]
    fn private_key_markers_are_found_after_long_prefixes_and_across_chunks() {
        let input = format!(
            "{}-----BEGIN OPENSSH PRIVATE KEY-----\nprivate-body\n-----END OPENSSH PRIVATE KEY-----\nnext",
            "x".repeat(20_000)
        );
        let mut stream = StreamingRedactor::new(&Redactor::default());
        let mut output = Vec::new();
        for chunk in input.as_bytes().chunks(7) {
            output.extend(stream.push(chunk, false));
            assert!(stream.pending.len() <= 64);
        }
        output.extend(stream.push(&[], true));
        assert_eq!(
            String::from_utf8(output).unwrap(),
            format!("{}[REDACTED PRIVATE KEY]\nnext", "x".repeat(20_000))
        );
    }

    #[test]
    fn eof_never_releases_partial_secrets_or_unterminated_private_keys() {
        let mut stream = StreamingRedactor::new(&Redactor::new(["sensitive-token".into()]));
        assert_eq!(stream.push(b"sensitive-", true), b"[REDACTED]");
        assert_eq!(
            stream.push(b"-----BEGIN PRIVATE KEY-----\nbody", true),
            b"[REDACTED PRIVATE KEY]"
        );
        assert!(stream.push(b"remaining-body\n", true).is_empty());
    }
}
