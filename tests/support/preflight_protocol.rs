//! Read-only tool replies for the loopback SSH protocol fixture.

pub fn reply(command: &str) -> Option<(u32, &'static [u8])> {
    if command.starts_with("'sh' '-c' 'for tool do command -v") {
        return Some((0, b""));
    }
    if command.starts_with("'sh' '-c' 'path=$1;") {
        return Some((0, b"/\n"));
    }
    match command {
        "'tar' '--help'" => Some((0, b"--extract --gzip --directory --no-same-owner\n")),
        "'ln' '--help'" => Some((0, b"--symbolic\n")),
        "'mv' '--help'" => Some((0, b"--no-clobber --no-target-directory\n")),
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
