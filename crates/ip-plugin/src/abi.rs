use ip_core::Checksum;
use wasmtime::{ExternType, Module, ValType};

use crate::error::PluginError;

/// The export holding the guest's linear memory.
pub const MEMORY: &str = "memory";
/// The export that reserves guest memory: `alloc(len: i32) -> i32`.
pub const ALLOC: &str = "alloc";
/// The export that releases guest memory: `dealloc(ptr: i32, len: i32)`.
pub const DEALLOC: &str = "dealloc";
/// The export that does the work: `transform(ptr: i32, len: i32) -> i64`.
pub const TRANSFORM: &str = "transform";
/// Set in `transform`'s return when the plugin refuses; the span then holds its reason.
pub const REFUSED: u64 = 1 << 63;

/// What a plugin made of its input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transformed {
    /// The bytes that go on in place of the input.
    Output(Vec<u8>),
    /// The plugin refused the request, and why.
    Refused(String),
}

/// A span of guest memory as `transform` packs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Packed {
    pub(crate) refused: bool,
    pub(crate) ptr: u32,
    pub(crate) len: u32,
}

impl Packed {
    /// Splits the return value: bit 63 refused, bits 32 to 62 pointer, bits 0 to 31 length.
    pub(crate) fn unpack(raw: u64) -> Self {
        Self {
            refused: raw & REFUSED != 0,
            ptr: ((raw >> 32) & 0x7fff_ffff) as u32,
            len: (raw & 0xffff_ffff) as u32,
        }
    }
}

/// Refuses a module that does not export the plugin abi with the right signatures.
pub(crate) fn check_exports(checksum: Checksum, module: &Module) -> Result<(), PluginError> {
    let broken = |detail: String| PluginError::Abi { checksum, detail };
    let exports_memory = module
        .exports()
        .any(|export| export.name() == MEMORY && matches!(export.ty(), ExternType::Memory(_)));
    if !exports_memory {
        return Err(broken(format!("no `{MEMORY}` memory export")));
    }
    for (name, params, results) in [
        (ALLOC, "i32", "i32"),
        (DEALLOC, "i32 i32", ""),
        (TRANSFORM, "i32 i32", "i64"),
    ] {
        let Some(export) = module.exports().find(|export| export.name() == name) else {
            return Err(broken(format!("no `{name}` function export")));
        };
        let ExternType::Func(func) = export.ty() else {
            return Err(broken(format!("`{name}` is not a function")));
        };
        let found_params = render(func.params());
        let found_results = render(func.results());
        if found_params != params || found_results != results {
            return Err(broken(format!(
                "`{name}` takes ({found_params}) and returns ({found_results}), \
                 not ({params}) and ({results})"
            )));
        }
    }
    Ok(())
}

fn render(types: impl Iterator<Item = ValType>) -> String {
    types.map(|ty| ty.to_string()).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::host::PluginHost;
    use crate::limits::PluginLimits;

    /// Memory, a growing bump allocator and a no-op dealloc, which every guest here shares.
    const PRELUDE: &str = r#"
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
    const BANG: &str = r#"
  (func (export "transform") (param $ptr i32) (param $len i32) (result i64)
    (local $out i32)
    (local.set $out (call $alloc (i32.add (local.get $len) (i32.const 1))))
    (memory.copy (local.get $out) (local.get $ptr) (local.get $len))
    (i32.store8 (i32.add (local.get $out) (local.get $len)) (i32.const 33))
    (i64.or
      (i64.shl (i64.extend_i32_u (local.get $out)) (i64.const 32))
      (i64.extend_i32_u (i32.add (local.get $len) (i32.const 1)))))"#;

    fn guest(transform: &str) -> String {
        format!("(module {PRELUDE} {transform})")
    }

    fn host() -> PluginHost {
        PluginHost::new(PluginLimits::default()).unwrap()
    }

    fn load(host: &PluginHost, text: &str) -> Result<Checksum, PluginError> {
        let bytes = wat::parse_str(text).unwrap();
        let checksum = Checksum::of(&bytes);
        host.load(&checksum, &bytes).map(|()| checksum)
    }

    fn run(host: &PluginHost, text: &str, input: &[u8]) -> Result<Transformed, PluginError> {
        let checksum = load(host, text).unwrap();
        host.instantiate(&checksum)?.transform(input)
    }

    #[test]
    fn the_return_value_unpacks_into_flag_pointer_and_length() {
        assert_eq!(
            Packed::unpack((16 << 32) | 17),
            Packed {
                refused: false,
                ptr: 16,
                len: 17,
            }
        );
        assert_eq!(
            Packed::unpack(REFUSED | (16 << 32) | 17),
            Packed {
                refused: true,
                ptr: 16,
                len: 17,
            }
        );
    }

    #[test]
    fn the_output_replaces_the_input() {
        assert_eq!(
            run(&host(), &guest(BANG), b"hello"),
            Ok(Transformed::Output(b"hello!".to_vec()))
        );
    }

    #[test]
    fn an_empty_input_is_still_transformed() {
        assert_eq!(
            run(&host(), &guest(BANG), b""),
            Ok(Transformed::Output(b"!".to_vec()))
        );
    }

    #[test]
    fn binary_input_crosses_untouched() {
        assert_eq!(
            run(&host(), &guest(BANG), &[0, 1, 2, 0, 255]),
            Ok(Transformed::Output(vec![0, 1, 2, 0, 255, 33]))
        );
    }

    #[test]
    fn an_input_larger_than_one_page_makes_the_guest_grow() {
        let input = vec![7_u8; 100_000];
        let Ok(Transformed::Output(output)) = run(&host(), &guest(BANG), &input) else {
            panic!("a 100 kB input within the memory limit must transform");
        };
        assert_eq!(output.len(), 100_001);
        assert_eq!(output.last(), Some(&33));
    }

    #[test]
    fn a_refusal_carries_its_reason() {
        let refusing = guest(
            r#"
  (data (i32.const 16) "blocked by policy")
  (func (export "transform") (param i32 i32) (result i64)
    (i64.or (i64.shl (i64.const 1) (i64.const 63))
            (i64.or (i64.shl (i64.const 16) (i64.const 32)) (i64.const 17))))"#,
        );
        assert_eq!(
            run(&host(), &refusing, b"anything"),
            Ok(Transformed::Refused("blocked by policy".to_owned()))
        );
    }

    #[test]
    fn a_reason_that_is_not_utf8_breaks_the_abi() {
        let garbled = guest(
            r#"
  (data (i32.const 16) "\ff\fe")
  (func (export "transform") (param i32 i32) (result i64)
    (i64.or (i64.shl (i64.const 1) (i64.const 63))
            (i64.or (i64.shl (i64.const 16) (i64.const 32)) (i64.const 2))))"#,
        );
        assert!(matches!(
            run(&host(), &garbled, b"anything"),
            Err(PluginError::Abi { .. })
        ));
    }

    #[test]
    fn a_span_outside_guest_memory_breaks_the_abi() {
        let wild = guest(
            r#"
  (func (export "transform") (param i32 i32) (result i64)
    (i64.or (i64.shl (i64.const 0x7fff0000) (i64.const 32)) (i64.const 16)))"#,
        );
        assert!(matches!(
            run(&host(), &wild, b"anything"),
            Err(PluginError::Abi { .. })
        ));
    }

    #[test]
    fn a_transform_that_loops_runs_out_of_fuel() {
        let host = PluginHost::new(PluginLimits {
            fuel: 50_000,
            deadline: Duration::from_secs(30),
            ..PluginLimits::default()
        })
        .unwrap();
        let spinning = guest(
            r#"
  (func (export "transform") (param i32 i32) (result i64)
    (loop (br 0))
    (i64.const 0))"#,
        );
        assert!(matches!(
            run(&host, &spinning, b"anything"),
            Err(PluginError::OutOfFuel { .. })
        ));
    }

    #[test]
    fn an_input_past_the_memory_limit_stops_the_call() {
        let host = PluginHost::new(PluginLimits {
            memory_bytes: 2 * 65536,
            ..PluginLimits::default()
        })
        .unwrap();
        let outcome = run(&host, &guest(BANG), &vec![7_u8; 200_000]);
        assert!(
            outcome.is_err(),
            "a 200 kB input past a 128 kB memory limit must fail, got {outcome:?}"
        );
    }

    #[test]
    fn a_module_missing_an_abi_export_is_refused_at_load() {
        let text = format!("(module {PRELUDE})");
        assert!(matches!(load(&host(), &text), Err(PluginError::Abi { .. })));
    }

    #[test]
    fn a_transform_with_the_wrong_signature_is_refused_at_load() {
        let wrong = guest(r#"(func (export "transform") (param i32) (result i32) (i32.const 0))"#);
        let Err(PluginError::Abi { detail, .. }) = load(&host(), &wrong) else {
            panic!("a transform of the wrong shape must be refused at load");
        };
        assert!(detail.contains("transform"), "{detail}");
    }

    #[test]
    fn a_module_that_keeps_its_memory_private_is_refused_at_load() {
        let private = r#"(module
  (memory 1)
  (func (export "alloc") (param i32) (result i32) (i32.const 0))
  (func (export "dealloc") (param i32 i32))
  (func (export "transform") (param i32 i32) (result i64) (i64.const 0)))"#;
        assert!(matches!(
            load(&host(), private),
            Err(PluginError::Abi { .. })
        ));
    }
}
