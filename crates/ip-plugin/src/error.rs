use ip_core::Checksum;
use ip_storage::StorageError;
use wasmtime::Trap;

/// Every way loading or running a plugin can fail.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PluginError {
    /// The wasm is not the wasm its checksum names.
    #[error("wasm does not hash to {checksum}")]
    ChecksumMismatch {
        /// The checksum the wasm was loaded under.
        checksum: Checksum,
    },

    /// The wasm does not compile.
    #[error("plugin {checksum} does not compile: {detail}")]
    Compile {
        /// The plugin that failed.
        checksum: Checksum,
        /// What the compiler said.
        detail: String,
    },

    /// The module imports something, and a plugin is given nothing to import.
    #[error("plugin {checksum} imports {import}, and plugins may import nothing")]
    Imports {
        /// The plugin that was refused.
        checksum: Checksum,
        /// The first import it declared.
        import: String,
    },

    /// The module does not keep to the plugin abi.
    #[error("plugin {checksum} breaks the plugin abi: {detail}")]
    Abi {
        /// The plugin at fault.
        checksum: Checksum,
        /// What it did wrong.
        detail: String,
    },

    /// No module is loaded under this checksum.
    #[error("plugin {checksum} is not loaded")]
    NotLoaded {
        /// The plugin that was asked for.
        checksum: Checksum,
    },

    /// The module could not be instantiated.
    #[error("plugin {checksum} could not be instantiated: {detail}")]
    Instantiate {
        /// The plugin that failed.
        checksum: Checksum,
        /// What went wrong.
        detail: String,
    },

    /// The call ran out of instructions.
    #[error("plugin {checksum} ran out of fuel")]
    OutOfFuel {
        /// The plugin that was stopped.
        checksum: Checksum,
    },

    /// The call ran past its deadline.
    #[error("plugin {checksum} ran past its deadline")]
    DeadlineExceeded {
        /// The plugin that was stopped.
        checksum: Checksum,
    },

    /// The call trapped for any other reason.
    #[error("plugin {checksum} trapped: {detail}")]
    Trap {
        /// The plugin that trapped.
        checksum: Checksum,
        /// What the trap was.
        detail: String,
    },

    /// The engine itself could not be set up.
    #[error("plugin engine failed: {0}")]
    Engine(String),
}

impl PluginError {
    /// Reads the trap out of a wasmtime error, or wraps the error as `otherwise` says.
    pub(crate) fn trapped(
        checksum: Checksum,
        error: &wasmtime::Error,
        otherwise: impl FnOnce(String) -> Self,
    ) -> Self {
        match error.downcast_ref::<Trap>() {
            Some(Trap::OutOfFuel) => Self::OutOfFuel { checksum },
            Some(Trap::Interrupt) => Self::DeadlineExceeded { checksum },
            Some(trap) => Self::Trap {
                checksum,
                detail: trap.to_string(),
            },
            None => otherwise(error.to_string()),
        }
    }
}

/// A header block a plugin returned that does not parse as headers.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("line {line} of the header block {problem}")]
pub struct HeaderBlockError {
    /// The line at fault, counting from one.
    pub line: usize,
    /// What is wrong with it.
    pub problem: &'static str,
}

/// Every way building the plugin chains at startup can fail.
#[derive(Debug, thiserror::Error)]
pub enum ChainError {
    /// The stored plugins or rules could not be read.
    #[error(transparent)]
    Storage(#[from] StorageError),

    /// A global plugin will not load, and every flow runs the global chain.
    #[error("global plugin {checksum} will not load: {detail}")]
    GlobalPlugin {
        /// The plugin that failed.
        checksum: Checksum,
        /// Why it will not load.
        detail: String,
    },
}
