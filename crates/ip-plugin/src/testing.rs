//! Guests, hosts and the one subscriber the plugin tests share.

use std::sync::{Arc, Mutex, OnceLock};

use ip_core::Checksum;
use tracing_subscriber::layer::SubscriberExt;

use crate::abi::Transformed;
use crate::error::PluginError;
use crate::host::PluginHost;
use crate::limits::PluginLimits;

/// Memory, a growing bump allocator and a no-op dealloc, which every guest here shares.
pub(crate) const PRELUDE: &str = r#"
  (memory (export "memory") 1)
  (global $next (mut i32) (i32.const 1024))
  (func $alloc (export "alloc") (param $len i32) (result i32)
    (local $at i32)
    (local $end i32)
    (local.set $at (global.get $next))
    (local.set $end (i32.add (local.get $at) (local.get $len)))
    (if (i32.gt_u (local.get $end) (i32.mul (memory.size) (i32.const 65536)))
      (then
        (drop (memory.grow
          (i32.sub
            (i32.add (i32.div_u (local.get $end) (i32.const 65536)) (i32.const 1))
            (memory.size))))))
    (global.set $next (local.get $end))
    (local.get $at))
  (func (export "dealloc") (param i32 i32))"#;

/// A whole guest that writes its input back as a log line at `level`, then passes it on.
///
/// The import comes before everything else, which is where wasm insists imports go.
pub(crate) fn talking(level: i32) -> String {
    format!(
        r#"(module
  (import "ip" "log" (func $log (param i32 i32 i32)))
  {PRELUDE}
  (func (export "transform") (param $ptr i32) (param $len i32) (result i64)
    (call $log (i32.const {level}) (local.get $ptr) (local.get $len))
    (i64.or
      (i64.shl (i64.extend_i32_u (local.get $ptr)) (i64.const 32))
      (i64.extend_i32_u (local.get $len)))))"#
    )
}

/// Copies the input and appends `!`.
pub(crate) const BANG: &str = r#"
  (func (export "transform") (param $ptr i32) (param $len i32) (result i64)
    (local $out i32)
    (local.set $out (call $alloc (i32.add (local.get $len) (i32.const 1))))
    (memory.copy (local.get $out) (local.get $ptr) (local.get $len))
    (i32.store8 (i32.add (local.get $out) (local.get $len)) (i32.const 33))
    (i64.or
      (i64.shl (i64.extend_i32_u (local.get $out)) (i64.const 32))
      (i64.extend_i32_u (i32.add (local.get $len) (i32.const 1)))))"#;

pub(crate) fn guest(transform: &str) -> String {
    format!("(module {PRELUDE} {transform})")
}

pub(crate) fn host() -> PluginHost {
    PluginHost::new(PluginLimits::default()).unwrap()
}

pub(crate) fn load(host: &PluginHost, text: &str) -> Result<Checksum, PluginError> {
    let bytes = wat::parse_str(text).unwrap();
    let checksum = Checksum::of(&bytes);
    host.load(&checksum, &bytes).map(|()| checksum)
}

pub(crate) fn run(host: &PluginHost, text: &str, input: &[u8]) -> Result<Transformed, PluginError> {
    let checksum = load(host, text).unwrap();
    host.instantiate(&checksum)?.transform(input)
}

/// One line a plugin wrote, and what it was written under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Line {
    pub(crate) level: tracing::Level,
    pub(crate) span: String,
    pub(crate) plugin: String,
    pub(crate) message: String,
}

/// Every line this crate's tests hear.
///
/// A plugin call runs on a blocking thread, which sees only the subscriber installed for the
/// whole process, and a process takes one of those — so every test shares this one.
#[derive(Clone, Default)]
pub(crate) struct Heard(Arc<Mutex<Vec<Line>>>);

impl Heard {
    /// The one line whose message is `message`, or a panic naming everything heard instead.
    pub(crate) fn saying(&self, message: &str) -> Line {
        let heard = self.0.lock().unwrap();
        let mut said = heard.iter().filter(|line| line.message == message);
        let found = said
            .next()
            .unwrap_or_else(|| panic!("nothing said {message:?}; heard {heard:?}"))
            .clone();
        assert!(said.next().is_none(), "{message:?} was said more than once");
        found
    }
}

impl<S> tracing_subscriber::Layer<S> for Heard
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        context: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut said = Said::default();
        event.record(&mut said);
        self.0.lock().unwrap().push(Line {
            level: *event.metadata().level(),
            span: context
                .event_span(event)
                .map(|span| span.name().to_owned())
                .unwrap_or_default(),
            plugin: said.plugin,
            message: said.message,
        });
    }
}

/// Reads the fields a plugin's line carries.
#[derive(Default)]
struct Said {
    plugin: String,
    message: String,
}

impl tracing::field::Visit for Said {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let value = format!("{value:?}");
        let value = value.trim_matches('"').to_owned();
        match field.name() {
            "plugin" => self.plugin = value,
            "message" => self.message = value,
            _ => {}
        }
    }
}

/// The lines every test shares, installing the subscriber that collects them on first use.
///
/// Whether an event is worth emitting is decided once per callsite for the whole process, so
/// this must be in place before the first test that reads one runs — installing it rebuilds
/// that decision.
pub(crate) fn heard() -> &'static Heard {
    static HEARD: OnceLock<Heard> = OnceLock::new();
    HEARD.get_or_init(|| {
        let heard = Heard::default();
        let _ = tracing::subscriber::set_global_default(
            tracing_subscriber::registry().with(heard.clone()),
        );
        heard
    })
}
