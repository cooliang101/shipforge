//! Read-only tool replies for the loopback SSH protocol fixture.

use shipforge::telemetry::{CommandArgument, CommandSpec};

// Intentionally exact, independent fixture copy. Unknown scripts are not accepted.
const RETENTION_CHECK: &str = r#"set -eu
export LC_ALL=C
if (false | true); then exit 1; fi
exec 3< /
test -d /proc/self/fd/3
test "$(stat -L -c '%d:%i:%s:%y:%z:%h:%f' -- /proc/self/fd/3)" = "$(stat -c '%d:%i:%s:%y:%z:%h:%f' -- /)"
test -r /proc/self/mountinfo
test -n "$(head -c 1 -- /proc/self/mountinfo)"
test "$(find -P /proc/self/status -maxdepth 0 -xdev -printf '%D:%y:%s')" = "$(stat -c '%d:f:%s' -- /proc/self/status)"
test "$(printf 12 | head -c 1)" = 1
test "$(printf '%s' 'a b\c' | sed -e 's/\\/\\134/g' -e 's/ /\\040/g')" = 'a\040b\134c'
printf '1:f:2\n1:d:3\n' | awk -F: '{ if (NF!=3 || $1!=1 || ($2!="f" && $2!="d") || $3!~/^[0-9]+$/) exit 1; total+=$3; } END { if (NR!=2 || total!=5) exit 1; }'
printf 'shipforge-retention-tools-v1\n'
"#;

fn retention_probe_command() -> String {
    CommandSpec::structured(
        "timeout",
        [
            "--signal=TERM",
            "--kill-after=1s",
            "5s",
            "bash",
            "-o",
            "pipefail",
            "-c",
            RETENTION_CHECK,
            "shipforge-preflight-retention",
        ]
        .map(CommandArgument::plain),
    )
    .unwrap()
    .render_posix()
    .unwrap()
}

pub fn reply(command: &str) -> Option<(u32, &'static [u8])> {
    if command.starts_with("'sh' '-c' 'for tool do command -v") {
        return Some((0, b""));
    }
    if command.starts_with("'sh' '-c' 'path=$1;") {
        return Some((0, b"/\n"));
    }
    if command == retention_probe_command() {
        return Some((0, b"shipforge-retention-tools-v1\n"));
    }
    match command {
        "'tar' '--help'" => Some((0, b"--extract --gzip --directory --no-same-owner\n")),
        "'ln' '--help'" => Some((0, b"--symbolic\n")),
        "'mv' '--help'" => Some((0, b"--no-clobber --no-target-directory\n")),
        "'timeout' '--help'" => Some((0, b"--kill-after --signal\n")),
        "'dd' '--help'" => Some((0, b"oflag= conv= status= append notrunc none\n")),
        "'rm' '--help'" => Some((0, b"--recursive --one-file-system --preserve-root[=all]\n")),
        "'sh' '-c' 'exec 3< /proc/self/status; test -f /proc/self/fd/3 && test -r /proc/self/fd/3 && head -c 1 /proc/self/fd/3 >/dev/null' 'shipforge-preflight'" => {
            Some((0, b""))
        }
        "'curl' '--version'" => Some((0, b"curl 8\nProtocols: http https\n")),
        _ if command.starts_with("'stat' '--file-system' '--format=%a:%S:%c:%d' '--'") => {
            Some((0, b"1048576:4096:131072:65536\n"))
        }
        _ if command.starts_with("'systemctl' 'show' '--property=LoadState' '--'") => {
            Some((0, b"LoadState=loaded\n"))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retention_probe_accepts_only_the_exact_script_and_argument_vector() {
        let command = retention_probe_command();
        assert_eq!(
            reply(&command),
            Some((0, b"shipforge-retention-tools-v1\n".as_slice()))
        );
        assert_eq!(
            reply(&command.replace("/proc/self/mountinfo", "/unapproved-path")),
            None
        );
        assert_eq!(reply(&format!("{command} 'unexpected'")), None);
        assert_eq!(reply("'rm' '--recursive' '--help'"), None);
    }
}
