//! The layout of muructl's tracing output (section 11.8): the same as
//! Muru's other lines, `muru: ` and the message, the level named only when
//! it is not info, the event's fields in brackets after the message, and
//! the full time in front when the stream is stamped.

use std::fmt;
use std::time::SystemTime;

use muru_common::log::{Stream, full, json, json_string, stamped};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::registry::LookupSpan;

/// The event format installed on muructl's subscriber, which writes to
/// standard error.
pub struct Layout;

impl<S, N> FormatEvent<S, N> for Layout
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        _context: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        if stamped(Stream::Stderr) {
            write!(writer, "{} ", full(SystemTime::now()))?;
        }

        let level = match *event.metadata().level() {
            Level::ERROR => "error: ",
            Level::WARN => "warning: ",
            Level::DEBUG => "debug: ",
            Level::TRACE => "trace: ",
            Level::INFO => "",
        };

        let mut fields = Fields::default();
        event.record(&mut fields);

        if json() {
            let name = match *event.metadata().level() {
                Level::ERROR => "error",
                Level::WARN => "warning",
                Level::INFO => "info",
                Level::DEBUG => "debug",
                Level::TRACE => "trace",
            };

            write!(
                writer,
                "{{\"time\":\"{}\",\"kind\":\"muru\",\"level\":\"{name}\",\"message\":{}",
                full(SystemTime::now()),
                json_string(&fields.message)
            )?;

            for (field, value) in &fields.pairs {
                write!(writer, ",{}:{}", json_string(field), json_string(value))?;
            }

            return writeln!(writer, "}}");
        }

        write!(writer, "muru: {level}{}", fields.message)?;

        if !fields.rest.is_empty() {
            write!(writer, " ({})", fields.rest.join(", "))?;
        }

        writeln!(writer)
    }
}

/// An event's message and its other fields, as `name=value` and as pairs.
#[derive(Default)]
struct Fields {
    message: String,
    rest: Vec<String>,
    pairs: Vec<(String, String)>,
}

impl Fields {
    fn add(&mut self, field: &Field, value: String) {
        if field.name() == "message" {
            self.message = value;
        } else {
            self.rest.push(format!("{}={value}", field.name()));
            self.pairs.push((field.name().to_owned(), value));
        }
    }
}

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.add(field, value.to_owned());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.add(field, format!("{value:?}"));
    }
}

/// Standard error, or standard output when every line goes there, for
/// the subscriber's writer.
pub struct Routed;

impl std::io::Write for Routed {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        muru_common::log::write(Stream::Stderr, bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::fmt::MakeWriter;

    use super::*;

    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("poisoned").extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Captured {
        type Writer = Captured;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    #[test]
    fn events_read_like_muru_lines() {
        let captured = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .event_format(Layout)
            .with_writer(captured.clone())
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(pid = 42, "re-adopted the edge");
            let subject = "shop@r2";
            tracing::warn!(%subject, "the release is draining");
            tracing::error!("recording an event failed");
        });
        let text = String::from_utf8(captured.0.lock().expect("poisoned").clone()).expect("utf-8");
        let lines: Vec<&str> = text.lines().collect();

        // The test's standard error is a pipe or a terminal, not a file,
        // so no time is in front.

        assert_eq!(
            lines,
            [
                "muru: re-adopted the edge (pid=42)",
                "muru: warning: the release is draining (subject=shop@r2)",
                "muru: error: recording an event failed",
            ]
        );
    }
}
