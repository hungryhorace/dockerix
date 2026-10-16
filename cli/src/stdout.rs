//! Printing to stdout when the reader may go away.
//!
//! Rust ignores SIGPIPE, so `println!` to a closed pipe panics:
//! `dockerix status | head -1` used to end in "failed printing to stdout:
//! Broken pipe". `outln!` and `out!` print instead, and a reader that has
//! gone ends the program quietly, with the status a SIGPIPE death gives
//! (128 + 13), as `head` expects of a C program. SIGPIPE itself stays
//! ignored, so a `docker` child that dies while we write to it is an error
//! we report, not a silent death.

use std::fmt;
use std::io::{self, ErrorKind, Write};
use std::process::exit;

/// The exit status of a process killed by SIGPIPE.
pub const BROKEN_PIPE_EXIT: i32 = 128 + 13;

/// `println!` that ends quietly when the reader has gone.
macro_rules! outln {
    () => {
        $crate::stdout::print(format_args!("\n"))
    };
    ($($arg:tt)*) => {
        $crate::stdout::print(format_args!("{}\n", format_args!($($arg)*)))
    };
}

/// `print!` that ends quietly when the reader has gone.
macro_rules! out {
    ($($arg:tt)*) => {
        $crate::stdout::print(format_args!($($arg)*))
    };
}

/// Write `args` to stdout and flush it, so a closed pipe shows at once
/// rather than at exit. A reader gone ends the program (`gone`).
pub fn print(args: fmt::Arguments) {
    let mut stdout = io::stdout().lock();
    if let Err(e) = stdout.write_fmt(args).and_then(|()| stdout.flush()) {
        gone(&e);
    }
}

/// Write raw bytes to stdout and flush them; a reader gone ends the program
/// quietly (`gone`). For binary output, such as the tar `dockerix get … -`
/// streams.
pub fn write_bytes(data: &[u8]) {
    let mut stdout = io::stdout().lock();
    if let Err(e) = stdout.write_all(data).and_then(|()| stdout.flush()) {
        gone(&e);
    }
}

/// End the program over a failed stdout write: quietly with
/// `BROKEN_PIPE_EXIT` when the reader has gone, else with a message.
pub fn gone(e: &io::Error) -> ! {
    if e.kind() == ErrorKind::BrokenPipe {
        exit(BROKEN_PIPE_EXIT);
    }
    eprintln!("dockerix: writing to stdout: {e}");
    exit(crate::host::FAILURE);
}
