use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, PoisonError, RwLock};
use std::thread::JoinHandle;
use std::time::Duration;

use ip_core::Checksum;
use wasmtime::{Config, Engine, Instance, Module, Store, StoreLimits, StoreLimitsBuilder};

use crate::abi::{self, ALLOC, DEALLOC, MEMORY, Packed, TRANSFORM, Transformed};
use crate::error::PluginError;
use crate::limits::PluginLimits;

/// Compiles plugins once, keeps them by checksum, and runs each call within its limits.
pub struct PluginHost {
    engine: Engine,
    modules: RwLock<HashMap<Checksum, Module>>,
    limits: PluginLimits,
    _clock: EpochClock,
}

impl PluginHost {
    /// Builds an engine that meters fuel and honours deadlines, and starts its clock.
    pub fn new(limits: PluginLimits) -> Result<Self, PluginError> {
        let mut config = Config::new();
        config.consume_fuel(true).epoch_interruption(true);
        let engine =
            Engine::new(&config).map_err(|error| PluginError::Engine(error.to_string()))?;
        let clock = EpochClock::start(engine.clone(), limits.tick)?;
        Ok(Self {
            engine,
            modules: RwLock::new(HashMap::new()),
            limits,
            _clock: clock,
        })
    }

    /// What each call may spend.
    pub fn limits(&self) -> PluginLimits {
        self.limits
    }

    /// Compiles wasm under its checksum. Refuses wasm that is not what the checksum names,
    /// that imports anything, or that does not export the plugin abi. Loading a checksum
    /// that is already loaded does nothing.
    pub fn load(&self, checksum: &Checksum, wasm: &[u8]) -> Result<(), PluginError> {
        if self.is_loaded(checksum) {
            return Ok(());
        }
        if !checksum.matches(wasm) {
            return Err(PluginError::ChecksumMismatch {
                checksum: *checksum,
            });
        }
        let module =
            Module::from_binary(&self.engine, wasm).map_err(|error| PluginError::Compile {
                checksum: *checksum,
                detail: error.to_string(),
            })?;
        if let Some(import) = module.imports().next() {
            return Err(PluginError::Imports {
                checksum: *checksum,
                import: format!("{}::{}", import.module(), import.name()),
            });
        }
        abi::check_exports(*checksum, &module)?;
        self.modules
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(*checksum, module);
        Ok(())
    }

    /// Whether a module is loaded under this checksum.
    pub fn is_loaded(&self, checksum: &Checksum) -> bool {
        self.modules
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .contains_key(checksum)
    }

    /// Drops the module under this checksum, reporting whether one was there.
    pub fn unload(&self, checksum: &Checksum) -> bool {
        self.modules
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(checksum)
            .is_some()
    }

    /// How many modules are loaded.
    pub fn len(&self) -> usize {
        self.modules
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// Whether no module is loaded.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Instantiates a loaded module in a fresh store that carries one call's limits.
    pub fn instantiate(&self, checksum: &Checksum) -> Result<Invocation, PluginError> {
        let module = self
            .modules
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(checksum)
            .cloned()
            .ok_or(PluginError::NotLoaded {
                checksum: *checksum,
            })?;
        let mut store = Store::new(
            &self.engine,
            CallState {
                limits: StoreLimitsBuilder::new()
                    .memory_size(self.limits.memory_bytes)
                    .instances(1)
                    .memories(1)
                    .tables(1)
                    .trap_on_grow_failure(true)
                    .build(),
            },
        );
        store.limiter(|state| &mut state.limits);
        store
            .set_fuel(self.limits.fuel)
            .map_err(|error| PluginError::Engine(error.to_string()))?;
        store.set_epoch_deadline(self.limits.deadline_ticks());
        store.epoch_deadline_trap();
        let instance = Instance::new(&mut store, &module, &[]).map_err(|error| {
            PluginError::trapped(*checksum, &error, |detail| PluginError::Instantiate {
                checksum: *checksum,
                detail,
            })
        })?;
        Ok(Invocation {
            checksum: *checksum,
            store,
            instance,
        })
    }
}

/// What a store carries for one call.
pub(crate) struct CallState {
    limits: StoreLimits,
}

/// One module instance and the store that bounds it, good for a single call.
pub struct Invocation {
    checksum: Checksum,
    store: Store<CallState>,
    instance: Instance,
}

impl Invocation {
    /// The plugin this invocation runs.
    pub fn checksum(&self) -> &Checksum {
        &self.checksum
    }

    /// Fuel left for the rest of this call.
    pub fn fuel_left(&self) -> u64 {
        self.store.get_fuel().unwrap_or(0)
    }

    /// Hands the input to the plugin and reads back what it made of it. Consumes the
    /// invocation, so one instance only ever serves one call.
    pub fn transform(mut self, input: &[u8]) -> Result<Transformed, PluginError> {
        let checksum = self.checksum;
        let broken = |detail: String| PluginError::Abi { checksum, detail };
        let trapped = |error: wasmtime::Error| {
            PluginError::trapped(checksum, &error, |detail| PluginError::Trap {
                checksum,
                detail,
            })
        };

        let memory = self
            .instance
            .get_memory(&mut self.store, MEMORY)
            .ok_or_else(|| broken(format!("no `{MEMORY}` memory export")))?;
        let alloc = self
            .instance
            .get_typed_func::<i32, i32>(&mut self.store, ALLOC)
            .map_err(|error| broken(error.to_string()))?;
        let dealloc = self
            .instance
            .get_typed_func::<(i32, i32), ()>(&mut self.store, DEALLOC)
            .map_err(|error| broken(error.to_string()))?;
        let transform = self
            .instance
            .get_typed_func::<(i32, i32), i64>(&mut self.store, TRANSFORM)
            .map_err(|error| broken(error.to_string()))?;

        let len = i32::try_from(input.len()).map_err(|_| {
            broken(format!(
                "an input of {} bytes cannot be addressed",
                input.len()
            ))
        })?;
        let at = alloc.call(&mut self.store, len).map_err(trapped)?;
        let offset = u32::try_from(at)
            .map_err(|_| broken(format!("`{ALLOC}` returned the negative address {at}")))?;
        memory
            .write(&mut self.store, offset as usize, input)
            .map_err(|_| {
                broken(format!(
                    "`{ALLOC}` returned memory the input does not fit in"
                ))
            })?;

        let raw = transform
            .call(&mut self.store, (at, len))
            .map_err(trapped)?;
        let span = Packed::unpack(raw.cast_unsigned());
        let mut out = vec![0_u8; span.len as usize];
        memory
            .read(&self.store, span.ptr as usize, &mut out)
            .map_err(|_| broken(format!("`{TRANSFORM}` returned a span outside its memory")))?;

        dealloc.call(&mut self.store, (at, len)).map_err(trapped)?;
        dealloc
            .call(
                &mut self.store,
                (span.ptr.cast_signed(), span.len.cast_signed()),
            )
            .map_err(trapped)?;

        if span.refused {
            return String::from_utf8(out)
                .map(Transformed::Refused)
                .map_err(|_| broken("the refusal reason is not utf-8".to_owned()));
        }
        Ok(Transformed::Output(out))
    }
}

/// Advances the engine's epoch in the background, which is what lets a deadline pass.
struct EpochClock {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl EpochClock {
    fn start(engine: Engine, tick: Duration) -> Result<Self, PluginError> {
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("ip-plugin-epoch".to_owned())
            .spawn(move || {
                while !stopping.load(Ordering::Relaxed) {
                    std::thread::sleep(tick);
                    engine.increment_epoch();
                }
            })
            .map_err(|error| PluginError::Engine(error.to_string()))?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for EpochClock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wasm(text: &str) -> (Checksum, Vec<u8>) {
        let bytes = wat::parse_str(text).unwrap();
        (Checksum::of(&bytes), bytes)
    }

    fn host() -> PluginHost {
        PluginHost::new(PluginLimits::default()).unwrap()
    }

    fn loaded(host: &PluginHost, text: &str) -> Checksum {
        let (checksum, bytes) = wasm(text);
        host.load(&checksum, &bytes).unwrap();
        checksum
    }

    /// A module that keeps to the plugin abi, with `pages` of memory and `extra` definitions.
    fn conforming(pages: u32, extra: &str) -> String {
        format!(
            r#"(module
  (memory (export "memory") {pages})
  (func (export "alloc") (param i32) (result i32) (i32.const 1024))
  (func (export "dealloc") (param i32 i32))
  (func (export "transform") (param i32 i32) (result i64) (i64.const 0))
  {extra})"#
        )
    }

    /// Calls an export that takes and returns nothing, classifying whatever stops it.
    fn call(invocation: &mut Invocation, export: &str) -> Result<(), PluginError> {
        let checksum = invocation.checksum;
        let function = invocation
            .instance
            .get_typed_func::<(), ()>(&mut invocation.store, export)
            .unwrap();
        function.call(&mut invocation.store, ()).map_err(|error| {
            PluginError::trapped(checksum, &error, |detail| PluginError::Trap {
                checksum,
                detail,
            })
        })
    }

    fn noop() -> String {
        conforming(1, r#"(func (export "noop"))"#)
    }

    fn spin() -> String {
        conforming(1, r#"(func (export "spin") (loop (br 0)))"#)
    }

    #[test]
    fn a_module_loads_under_its_checksum() {
        let host = host();
        let checksum = loaded(&host, &noop());
        assert!(host.is_loaded(&checksum));
        assert_eq!(host.len(), 1);
    }

    #[test]
    fn loading_the_same_checksum_twice_compiles_once() {
        let host = host();
        let first = loaded(&host, &noop());
        let second = loaded(&host, &noop());
        assert_eq!(first, second);
        assert_eq!(host.len(), 1);
    }

    #[test]
    fn wasm_that_is_not_what_its_checksum_names_is_refused() {
        let host = host();
        let (_, bytes) = wasm(&noop());
        let claimed = Checksum::of(b"some other module");
        assert_eq!(
            host.load(&claimed, &bytes),
            Err(PluginError::ChecksumMismatch { checksum: claimed })
        );
        assert!(host.is_empty());
    }

    #[test]
    fn bytes_that_are_not_wasm_do_not_compile() {
        let host = host();
        let bytes = b"not a module".to_vec();
        let checksum = Checksum::of(&bytes);
        assert!(matches!(
            host.load(&checksum, &bytes),
            Err(PluginError::Compile { .. })
        ));
    }

    #[test]
    fn a_module_that_imports_anything_is_refused() {
        let host = host();
        let (checksum, bytes) = wasm(
            r#"(module (import "wasi_snapshot_preview1" "fd_write"
                   (func (param i32 i32 i32 i32) (result i32))))"#,
        );
        assert_eq!(
            host.load(&checksum, &bytes),
            Err(PluginError::Imports {
                checksum,
                import: "wasi_snapshot_preview1::fd_write".to_owned(),
            })
        );
    }

    #[test]
    fn an_unloaded_module_can_no_longer_be_instantiated() {
        let host = host();
        let checksum = loaded(&host, &noop());
        assert!(host.unload(&checksum));
        assert!(!host.unload(&checksum));
        assert!(matches!(
            host.instantiate(&checksum),
            Err(PluginError::NotLoaded { .. })
        ));
    }

    #[test]
    fn a_loaded_module_runs() {
        let host = host();
        let checksum = loaded(&host, &noop());
        let mut invocation = host.instantiate(&checksum).unwrap();
        assert_eq!(call(&mut invocation, "noop"), Ok(()));
        assert!(invocation.fuel_left() < host.limits().fuel);
    }

    #[test]
    fn an_invocation_names_the_plugin_it_runs() {
        let host = host();
        let checksum = loaded(&host, &noop());
        assert_eq!(host.instantiate(&checksum).unwrap().checksum(), &checksum);
    }

    #[test]
    fn an_alloc_that_cannot_hold_the_input_breaks_the_abi() {
        let host = host();
        let checksum = loaded(
            &host,
            r#"(module
  (memory (export "memory") 1)
  (func (export "alloc") (param i32) (result i32) (i32.const 65500))
  (func (export "dealloc") (param i32 i32))
  (func (export "transform") (param i32 i32) (result i64) (i64.const 0)))"#,
        );
        let invocation = host.instantiate(&checksum).unwrap();
        let Err(PluginError::Abi { detail, .. }) = invocation.transform(&[0_u8; 100]) else {
            panic!("writing past the end of guest memory must break the abi");
        };
        assert!(detail.contains("does not fit"), "{detail}");
    }

    #[test]
    fn a_loop_is_stopped_when_its_fuel_runs_out() {
        let host = PluginHost::new(PluginLimits {
            fuel: 10_000,
            deadline: Duration::from_secs(30),
            ..PluginLimits::default()
        })
        .unwrap();
        let checksum = loaded(&host, &spin());
        let mut invocation = host.instantiate(&checksum).unwrap();
        assert_eq!(
            call(&mut invocation, "spin"),
            Err(PluginError::OutOfFuel { checksum })
        );
    }

    #[test]
    fn a_loop_is_stopped_when_its_deadline_passes() {
        let host = PluginHost::new(PluginLimits {
            fuel: u64::MAX,
            deadline: Duration::from_millis(50),
            tick: Duration::from_millis(5),
            ..PluginLimits::default()
        })
        .unwrap();
        let checksum = loaded(&host, &spin());
        let mut invocation = host.instantiate(&checksum).unwrap();
        assert_eq!(
            call(&mut invocation, "spin"),
            Err(PluginError::DeadlineExceeded { checksum })
        );
    }

    #[test]
    fn a_start_function_is_bounded_too() {
        let host = PluginHost::new(PluginLimits {
            fuel: 10_000,
            deadline: Duration::from_secs(30),
            ..PluginLimits::default()
        })
        .unwrap();
        let checksum = loaded(
            &host,
            &conforming(1, r#"(func $spin (loop (br 0))) (start $spin)"#),
        );
        assert!(matches!(
            host.instantiate(&checksum),
            Err(PluginError::OutOfFuel { .. })
        ));
    }

    #[test]
    fn growing_memory_past_the_limit_stops_the_call() {
        let host = host();
        let checksum = loaded(
            &host,
            &conforming(
                1,
                r#"(func (export "grow") (drop (memory.grow (i32.const 2000))))"#,
            ),
        );
        let mut invocation = host.instantiate(&checksum).unwrap();
        let outcome = call(&mut invocation, "grow");
        assert!(
            outcome.is_err(),
            "growing to 2000 pages past a 64 MiB limit must fail, got {outcome:?}"
        );
    }

    #[test]
    fn a_module_that_starts_larger_than_the_limit_is_not_instantiated() {
        let host = host();
        let checksum = loaded(&host, &conforming(2000, ""));
        assert!(matches!(
            host.instantiate(&checksum),
            Err(PluginError::Instantiate { .. })
        ));
    }

    #[test]
    fn a_trap_other_than_the_limits_is_reported_as_a_trap() {
        let host = host();
        let checksum = loaded(
            &host,
            &conforming(1, r#"(func (export "fail") unreachable)"#),
        );
        let mut invocation = host.instantiate(&checksum).unwrap();
        assert!(matches!(
            call(&mut invocation, "fail"),
            Err(PluginError::Trap { .. })
        ));
    }
}
