//! Guests and hosts the plugin tests share.

use ip_core::Checksum;

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
