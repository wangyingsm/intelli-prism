/// How a run refuses to start.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RunError {
    /// A model named by a template or a request that no upstream could be asked for.
    ///
    /// A template checks the shape of a name and not the name itself, so one that passed when
    /// it was written can still be refused here, where it meets the stricter rule the gateway
    /// spells an upstream's models under.
    #[error("{name:?} is not a model this gateway can ask for: {detail}")]
    NotAModel {
        /// What was named.
        name: String,
        /// Why it will not do.
        detail: String,
    },

    /// A tool naming no server, which is a system tool, and none of those run yet.
    #[error("the tool {tool:?} names no server, and no system tool runs yet")]
    UnqualifiedTool {
        /// What the template asked for.
        tool: String,
    },

    /// A qualified name whose halves are not a server and a tool.
    #[error("the tool {tool:?} does not name a server and a tool: {why}")]
    NotAToolAddress {
        /// What the template asked for.
        tool: String,
        /// Which half will not do, and why.
        why: String,
    },

    /// A tool on a server this deployment has not registered.
    #[error("the tool {tool:?} wants the server {server:?}, which is not registered")]
    NoSuchServer {
        /// What the template asked for.
        tool: String,
        /// The server it named.
        server: String,
    },

    /// A tool the server it names does not offer.
    #[error("the server {server:?} offers no tool called {tool:?}")]
    NoSuchTool {
        /// The server that was asked.
        server: String,
        /// The tool it does not offer.
        tool: String,
    },

    /// A qualified name longer than a model may be told about.
    #[error("{tool:?} becomes {wire:?}, past the {max} bytes a model may be told")]
    ToolNameTooLong {
        /// What the template asked for.
        tool: String,
        /// What it would be declared as.
        wire: String,
        /// What is allowed.
        max: usize,
    },

    /// Two tools a model could not tell apart once both are named for it.
    #[error("{first:?} and {second:?} are both declared as {wire:?}")]
    ToolNameCollides {
        /// What both would be declared as.
        wire: String,
        /// The first that wanted it.
        first: String,
        /// The second.
        second: String,
    },
}
