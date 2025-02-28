use std::env;

#[cfg(windows)]
use ansi_term;
#[cfg(not(windows))]
use atty;

pub fn init_tracing_logger() {
    tracing_subscriber::fmt()
        .with_ansi(detect_ansi_support())
        .init();
}

fn detect_ansi_support() -> bool {
    #[cfg(windows)]
    {
        use ansi_term::enable_ansi_support;
        enable_ansi_support().is_ok()
    }
    #[cfg(not(windows))]
    {
        // Detect whether our log output (which goes to stdout) is going to a terminal.
        // For example, instead of the terminal, it might be getting piped into another file, which
        // probably ought to be plain text.
        let is_terminal = atty::is(atty::Stream::Stdout);
        if !is_terminal {
            return false;
        }

        // Return whether terminal defined in TERM supports ANSI
        env::var("TERM").map(|term| term != "dumb").unwrap_or(false)
    }
}
