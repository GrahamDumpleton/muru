//! Muru's own log lines and the times on them (section 11.8 of the
//! specification, ADR 0043).
//!
//! Every line gets a time when it is written; whether the time is shown
//! depends on where the line goes. A regular file always carries the full
//! form, RFC 3339 in UTC with milliseconds. Standard output and standard
//! error carry it only when `--timestamps` was given, since a terminal,
//! a container runtime and journald each have their own. The short form,
//! a clock in local time, is for a terminal only.

use std::fmt;
use std::fs::File;
use std::io::{self, IsTerminal as _, Write as _};
use std::os::fd::AsFd as _;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use jiff::Timestamp;
use jiff::tz::TimeZone;

/// The two streams a long-running process writes its lines to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

/// Whether `--timestamps` (or the configuration key) asked for the time
/// on standard output and standard error.
static TIMESTAMPS: AtomicBool = AtomicBool::new(false);

/// Put the full time in front of every line written to standard output
/// and standard error, as `--timestamps` does.
pub fn show_timestamps(on: bool) {
    TIMESTAMPS.store(on, Ordering::Relaxed);
}

/// Whether `--timestamps` is in force.
pub fn timestamps_shown() -> bool {
    TIMESTAMPS.load(Ordering::Relaxed)
}

/// Whether lines written to `stream` carry the full time in front: always
/// when the stream is a regular file, otherwise with `--timestamps`.
pub fn stamped(stream: Stream) -> bool {
    timestamps_shown() || is_file(route(stream))
}

/// Whether every line goes to standard output, as under `muructl serve`
/// (section 13.9), so lines stay in order and a log collector does not
/// take everything on standard error for an error.
static ONE_STREAM: AtomicBool = AtomicBool::new(false);

/// Send every line to standard output from now on.
pub fn one_stream(on: bool) {
    ONE_STREAM.store(on, Ordering::Relaxed);
}

/// The stream a line meant for `stream` is written to.
pub fn route(stream: Stream) -> Stream {
    if ONE_STREAM.load(Ordering::Relaxed) {
        Stream::Stdout
    } else {
        stream
    }
}

/// Whether lines are written as JSON objects, one per line, as `muructl
/// serve --log-format json` asks (section 13.9).
static JSON: AtomicBool = AtomicBool::new(false);

/// Write every line as a JSON object from now on.
pub fn json_lines(on: bool) {
    JSON.store(on, Ordering::Relaxed);
}

/// Whether lines are written as JSON objects.
pub fn json() -> bool {
    JSON.load(Ordering::Relaxed)
}

/// A string as a JSON string literal, quotes included.
pub fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');

    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if (control as u32) < 0x20 => {
                let _ = fmt::write(&mut out, format_args!("\\u{:04x}", control as u32));
            }
            other => out.push(other),
        }
    }

    out.push('"');
    out
}

/// Whether `stream` is a terminal, decided once.
pub fn is_terminal(stream: Stream) -> bool {
    static STDOUT: OnceLock<bool> = OnceLock::new();
    static STDERR: OnceLock<bool> = OnceLock::new();

    match stream {
        Stream::Stdout => *STDOUT.get_or_init(|| io::stdout().is_terminal()),
        Stream::Stderr => *STDERR.get_or_init(|| io::stderr().is_terminal()),
    }
}

/// The time a record's own line starts with, such as an access log line,
/// whose time is part of the record: the short clock on a terminal, and
/// the full form otherwise or with `--timestamps`.
pub fn record_time(stream: Stream, time: SystemTime) -> String {
    if timestamps_shown() || !is_terminal(route(stream)) {
        full(time)
    } else {
        clock(time)
    }
}

/// Whether `stream` is a regular file, decided once: the daemon and the
/// edge write to their log files through their standard streams, and a
/// reopen after rotation replaces the file with another file.
fn is_file(stream: Stream) -> bool {
    static STDOUT: OnceLock<bool> = OnceLock::new();
    static STDERR: OnceLock<bool> = OnceLock::new();

    let cell = match stream {
        Stream::Stdout => &STDOUT,
        Stream::Stderr => &STDERR,
    };

    *cell.get_or_init(|| {
        let descriptor = match stream {
            Stream::Stdout => io::stdout().as_fd().try_clone_to_owned(),
            Stream::Stderr => io::stderr().as_fd().try_clone_to_owned(),
        };

        descriptor
            .ok()
            .map(File::from)
            .and_then(|file| file.metadata().ok())
            .is_some_and(|metadata| metadata.is_file())
    })
}

/// The full form: RFC 3339 in UTC with milliseconds and a `Z`, for
/// example `2026-11-02T05:42:22.836Z`.
pub fn full(time: SystemTime) -> String {
    let Ok(timestamp) = Timestamp::try_from(time) else {
        return "1970-01-01T00:00:00.000Z".to_owned();
    };
    timestamp
        .to_zoned(TimeZone::UTC)
        .strftime("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

/// The short form: the local clock with milliseconds, for example
/// `16:42:22.836`, for a terminal only.
pub fn clock(time: SystemTime) -> String {
    static LOCAL: OnceLock<TimeZone> = OnceLock::new();

    let Ok(timestamp) = Timestamp::try_from(time) else {
        return "00:00:00.000".to_owned();
    };
    let zone = LOCAL.get_or_init(TimeZone::system).clone();
    timestamp
        .to_zoned(zone)
        .strftime("%H:%M:%S%.3f")
        .to_string()
}

/// Split the full time off the front of a line written by Muru, giving
/// the time, when the line starts with one, and the rest.
pub fn split_time(line: &str) -> (Option<&str>, &str) {
    // 2026-11-02T05:42:22.836Z and a space: 25 bytes.

    let bytes = line.as_bytes();
    let shaped = bytes.len() >= 25
        && bytes[..24].iter().enumerate().all(|(at, byte)| match at {
            4 | 7 => *byte == b'-',
            10 => *byte == b'T',
            13 | 16 => *byte == b':',
            19 => *byte == b'.',
            23 => *byte == b'Z',
            _ => byte.is_ascii_digit(),
        })
        && bytes[24] == b' ';

    if shaped {
        (Some(&line[..24]), &line[25..])
    } else {
        (None, line)
    }
}

/// Write one line to `stream`: the full time when the stream is stamped,
/// then `tag: ` and the message, in one write so lines from several
/// threads never interleave.
pub fn line(stream: Stream, tag: &str, message: fmt::Arguments<'_>) {
    let mut text = String::with_capacity(128);

    if json() {
        text.push_str(&format!(
            "{{\"time\":\"{}\",\"kind\":\"muru\",\"message\":{}}}\n",
            full(SystemTime::now()),
            json_string(&message.to_string())
        ));
        write(stream, text.as_bytes());
        return;
    }

    if stamped(stream) {
        text.push_str(&full(SystemTime::now()));
        text.push(' ');
    }

    text.push_str(tag);
    text.push_str(": ");
    let _ = fmt::write(&mut text, message);
    text.push('\n');
    write(stream, text.as_bytes());
}

/// Write a line that already carries its tag, such as a host's or a
/// worker's relayed output, with the time in front when the stream is
/// stamped. `time` is when the line arrived.
pub fn relayed(stream: Stream, time: SystemTime, text: &str) {
    let mut out = String::with_capacity(text.len() + 32);

    if json() {
        // A relayed line is the application's output, tagged by the host
        // with the worker or the host it came from.

        let (source, message) = match text.split_once(": ") {
            Some((source, message)) if source.starts_with("muru.") => (source, message),
            _ => ("", text),
        };

        out.push_str(&format!(
            "{{\"time\":\"{}\",\"kind\":\"app\",\"source\":{},\"message\":{}}}\n",
            full(time),
            json_string(source),
            json_string(message)
        ));
        write(stream, out.as_bytes());
        return;
    }

    if stamped(stream) {
        out.push_str(&full(time));
        out.push(' ');
    }

    out.push_str(text);
    out.push('\n');
    write(stream, out.as_bytes());
}

/// Write bytes that hold whole lines to `stream`, or to standard output
/// when every line goes there, in one write.
pub fn write(stream: Stream, bytes: &[u8]) {
    // A stream that cannot be written to (a closed terminal, a full disk)
    // never stops the process that logs to it.

    let _ = match route(stream) {
        Stream::Stdout => io::stdout().lock().write_all(bytes),
        Stream::Stderr => io::stderr().lock().write_all(bytes),
    };
}

/// Print one of Muru's own lines to standard output, as `println!` does,
/// with `muru: ` in front and the time when the stream is stamped.
#[macro_export]
macro_rules! muru_println {
    ($($arg:tt)*) => {
        $crate::log::line($crate::log::Stream::Stdout, "muru", format_args!($($arg)*))
    };
}

/// Print one of Muru's own lines to standard error, as `eprintln!` does,
/// with `muru: ` in front and the time when the stream is stamped.
#[macro_export]
macro_rules! muru_eprintln {
    ($($arg:tt)*) => {
        $crate::log::line($crate::log::Stream::Stderr, "muru", format_args!($($arg)*))
    };
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;

    #[test]
    fn the_full_form_is_rfc_3339_in_utc_with_milliseconds() {
        let time = UNIX_EPOCH + Duration::from_millis(1_793_597_342_836);
        assert_eq!(full(time), "2026-11-02T05:29:02.836Z");
        assert_eq!(full(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn json_strings_are_escaped() {
        assert_eq!(json_string("plain"), "\"plain\"");
        assert_eq!(
            json_string("a \"b\" \\ c\n\u{1}"),
            "\"a \\\"b\\\" \\\\ c\\n\\u0001\""
        );
    }

    #[test]
    fn a_leading_time_splits_off() {
        assert_eq!(
            split_time("2026-11-02T05:42:22.836Z muru.host[42]: ready"),
            (Some("2026-11-02T05:42:22.836Z"), "muru.host[42]: ready")
        );
        assert_eq!(
            split_time("muru.host[42]: ready"),
            (None, "muru.host[42]: ready")
        );
        assert_eq!(
            split_time("2026-11-02 not a time at all, no"),
            (None, "2026-11-02 not a time at all, no")
        );
    }

    #[test]
    fn the_short_form_is_a_clock_with_milliseconds() {
        let time = UNIX_EPOCH + Duration::from_millis(1_793_597_342_836);
        let text = clock(time);
        assert_eq!(text.len(), 12, "{text}");
        assert!(text.ends_with(":02.836"), "{text}");
    }
}
