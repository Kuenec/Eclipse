use std::io::Write as _;

use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::{FormatTime, SystemTime};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::registry::Registry;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

struct PanicSafeStderr;

struct EventFieldVisitor<'a>(&'a mut String);

impl Visit for EventFieldVisitor<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write as _;
        if field.name() == "message" {
            let _ = write!(self.0, "{value:?}");
        } else {
            let _ = write!(self.0, " {}={value:?}", field.name());
        }
    }
}

impl<S: Subscriber> Layer<S> for PanicSafeStderr {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        use std::fmt::Write as _;
        let meta = event.metadata();
        let mut line = String::with_capacity(256);

        let _ = SystemTime.format_time(&mut Writer::new(&mut line));
        let _ = write!(line, " {:>5} {}: ", meta.level(), meta.target());
        event.record(&mut EventFieldVisitor(&mut line));
        line.push('\n');

        let _ = std::io::stderr().write_all(line.as_bytes());
    }
}

pub fn init() {
    let filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env_lossy();

    let _ = Registry::default()
        .with(filter)
        .with(PanicSafeStderr)
        .try_init();
}

#[cfg(test)]
mod tests {

    #[test]
    fn init_is_idempotent() {
        super::init();
        super::init();
    }
}
