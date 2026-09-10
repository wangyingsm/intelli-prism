use std::fmt;

/// Where in the dataflow a request had got to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stage {
    /// The kernel handed the connection over.
    Receive,
    /// The caller's signature was checked.
    Authentication,
    /// The request headers were read.
    HeaderRead,
    /// The request header plugin chain ran.
    HeaderProcess,
    /// The caller's capabilities were checked.
    Authorization,
    /// The request body was read.
    BodyRead,
    /// The request body plugin chain ran.
    BodyProcess,
    /// The request was sent upstream.
    Route,
    /// The response headers were read.
    ResponseHeaderRead,
    /// The response header plugin chain ran.
    ResponseHeaderProcess,
    /// The response body was read.
    ResponseBodyRead,
    /// The response body plugin chain ran.
    ResponseBodyProcess,
    /// The response went back to the kernel.
    Send,
}

impl Stage {
    /// The stage's name, as it appears in logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Receive => "receive",
            Self::Authentication => "authentication",
            Self::HeaderRead => "header read",
            Self::HeaderProcess => "header process",
            Self::Authorization => "authorization",
            Self::BodyRead => "body read",
            Self::BodyProcess => "body process",
            Self::Route => "route",
            Self::ResponseHeaderRead => "response header read",
            Self::ResponseHeaderProcess => "response header process",
            Self::ResponseBodyRead => "response body read",
            Self::ResponseBodyProcess => "response body process",
            Self::Send => "send",
        }
    }
}

impl fmt::Display for Stage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
