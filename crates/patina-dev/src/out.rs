//! Standard output that tolerates a closed reader (`patina-dev status | head`).
//!
//! Rust ignores SIGPIPE, so a write to a closed pipe fails with EPIPE and
//! `println!` panics. These helpers end the process quietly instead.

use std::io::{self, Write};

/// Writes `args` (plus a newline when `newline`) to stdout and flushes.
/// Exits the process with status 0 when stdout's reader went away; panics
/// on any other write error, like `println!`.
pub fn write_stdout(args: std::fmt::Arguments<'_>, newline: bool) {
    let mut out = io::stdout().lock();
    let r = out
        .write_fmt(args)
        .and_then(|()| {
            if newline {
                out.write_all(b"\n")
            } else {
                Ok(())
            }
        })
        .and_then(|()| out.flush());
    if let Err(e) = r {
        exit_if_broken_pipe(&e);
        panic!("failed printing to stdout: {e}");
    }
}

/// Ends the process quietly (status 0) when `e` says stdout's reader is gone.
pub fn exit_if_broken_pipe(e: &io::Error) {
    if e.kind() == io::ErrorKind::BrokenPipe {
        std::process::exit(0);
    }
}

/// `println!` that exits quietly when stdout is a closed pipe.
#[macro_export]
macro_rules! outln {
    () => {
        $crate::out::write_stdout(format_args!(""), true)
    };
    ($($arg:tt)*) => {
        $crate::out::write_stdout(format_args!($($arg)*), true)
    };
}

/// `print!` (flushed) that exits quietly when stdout is a closed pipe.
#[macro_export]
macro_rules! out {
    ($($arg:tt)*) => {
        $crate::out::write_stdout(format_args!($($arg)*), false)
    };
}
