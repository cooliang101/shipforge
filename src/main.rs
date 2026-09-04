use std::io;

fn main() -> io::Result<()> {
    // Panic payloads may contain captured process output or credentials. The
    // TUI's worker boundary supplies a safe diagnostic instead of this payload.
    std::panic::set_hook(Box::new(|_| {
        eprintln!(
            "ShipForge encountered an unexpected error; inspect deployment history and remote state before retrying."
        );
    }));
    shipforge::tui::run()
}
