//! The object tree a run may see, and nothing outside it.

use std::collections::BTreeMap;
use std::fmt;

use crate::error::AgentError;

/// The longest one path segment may be, in bytes.
const SEGMENT_MAX_BYTES: usize = 255;
/// The longest an object storage location may be, in bytes.
const LOCATION_MAX_BYTES: usize = 1024;
/// The deepest a sandbox tree may nest.
const SANDBOX_MAX_DEPTH: usize = 32;
/// The most entries one sandbox may carry, files and directories together.
const SANDBOX_MAX_ENTRIES: usize = 4096;

/// Every entry one directory of a sandbox holds, by name.
pub type Entries = BTreeMap<PathSegment, SandboxEntry>;

/// One name in a sandbox path.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Deserialize)]
#[serde(try_from = "String")]
pub struct PathSegment(Box<str>);

impl PathSegment {
    /// Name of this kind, as it appears in errors.
    pub const KIND: &'static str = "path segment";

    /// Validates and wraps one name, refusing one that is not a name at all.
    pub fn new(raw: &str) -> Result<Self, AgentError> {
        Self::try_from(raw.to_owned())
    }

    /// The segment as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for PathSegment {
    type Error = AgentError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        if raw.is_empty() {
            return Err(AgentError::Empty { kind: Self::KIND });
        }
        if raw.len() > SEGMENT_MAX_BYTES {
            return Err(AgentError::TooLong {
                kind: Self::KIND,
                len: raw.len(),
                max: SEGMENT_MAX_BYTES,
            });
        }
        // A segment that climbs is refused here rather than by whatever opens the sandbox, since
        // nothing opens one yet and a stored path that escapes would be stored for months first.
        if raw == "." || raw == ".." {
            return Err(AgentError::ReservedSegment { value: raw });
        }
        match raw
            .chars()
            .find(|ch| matches!(ch, '/' | '\\') || ch.is_control())
        {
            Some(ch) => Err(AgentError::IllegalChar {
                kind: Self::KIND,
                ch,
            }),
            None => Ok(Self(raw.into_boxed_str())),
        }
    }
}

impl fmt::Display for PathSegment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl serde::Serialize for PathSegment {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

/// Where one sandbox file's bytes live.
///
/// Checked for shape and nothing else: no object storage exists yet, so this code does not know
/// which schemes it will be asked to serve, and refusing the wrong ones now would refuse the
/// right ones later.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ObjectLocation(Box<str>);

impl ObjectLocation {
    /// Name of this kind, as it appears in errors.
    pub const KIND: &'static str = "object location";

    /// Validates and wraps a location.
    pub fn new(raw: &str) -> Result<Self, AgentError> {
        if raw.is_empty() {
            return Err(AgentError::Empty { kind: Self::KIND });
        }
        if raw.len() > LOCATION_MAX_BYTES {
            return Err(AgentError::TooLong {
                kind: Self::KIND,
                len: raw.len(),
                max: LOCATION_MAX_BYTES,
            });
        }
        match raw.chars().find(char::is_ascii_control) {
            Some(ch) => Err(AgentError::IllegalChar {
                kind: Self::KIND,
                ch,
            }),
            None => Ok(Self(Box::from(raw))),
        }
    }

    /// The location as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ObjectLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What sits at one name of a sandbox: a file, or a directory of further entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxEntry {
    /// Further entries, by name.
    Directory(Entries),
    /// One file, and where its bytes live.
    File(ObjectLocation),
}

impl serde::Serialize for SandboxEntry {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::File(location) => serializer.serialize_str(location.as_str()),
            Self::Directory(entries) => entries.serialize(serializer),
        }
    }
}

impl<'de> serde::Deserialize<'de> for SandboxEntry {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(EntryVisitor)
    }
}

/// Reads one entry, so a refusal says which name and which reason rather than that nothing matched.
struct EntryVisitor;

impl<'de> serde::de::Visitor<'de> for EntryVisitor {
    type Value = SandboxEntry;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an object storage location, or an object of further entries")
    }

    fn visit_str<E: serde::de::Error>(self, raw: &str) -> Result<Self::Value, E> {
        ObjectLocation::new(raw)
            .map(SandboxEntry::File)
            .map_err(serde::de::Error::custom)
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut entries = Entries::new();
        while let Some((name, entry)) = map.next_entry::<PathSegment, SandboxEntry>()? {
            if entries.insert(name.clone(), entry).is_some() {
                return Err(serde::de::Error::custom(AgentError::Duplicate {
                    kind: PathSegment::KIND,
                    value: name.as_str().to_owned(),
                }));
            }
        }
        Ok(SandboxEntry::Directory(entries))
    }
}

/// The file tree one agent would see, every file of it standing for an object in storage.
///
/// Empty is allowed: an agent that needs no files of its own is a legitimate thing to configure.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "Entries")]
pub struct Sandbox(Entries);

impl Sandbox {
    /// Name of this kind, as it appears in errors.
    pub const KIND: &'static str = "sandbox";

    /// Validates and wraps a tree, refusing one too deep or too large to hold.
    pub fn new(entries: Entries) -> Result<Self, AgentError> {
        Self::try_from(entries)
    }

    /// The entries at the top of the tree, by name.
    #[must_use]
    pub const fn entries(&self) -> &Entries {
        &self.0
    }

    /// Whether the agent would see nothing at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl TryFrom<Entries> for Sandbox {
    type Error = AgentError;

    fn try_from(entries: Entries) -> Result<Self, Self::Error> {
        let count = walk(&entries).ok_or(AgentError::TooDeep {
            kind: Self::KIND,
            max: SANDBOX_MAX_DEPTH,
        })?;
        if count > SANDBOX_MAX_ENTRIES {
            return Err(AgentError::TooMany {
                kind: Self::KIND,
                count,
                max: SANDBOX_MAX_ENTRIES,
            });
        }
        Ok(Self(entries))
    }
}

/// Counts every entry of a tree, or reports that it nests deeper than one may.
///
/// Iterative rather than recursive: a tree deep enough to refuse is also deep enough to exhaust
/// the stack on the way to refusing it.
fn walk(entries: &Entries) -> Option<usize> {
    let mut pending: Vec<(&Entries, usize)> = vec![(entries, 1)];
    let mut count = 0;
    while let Some((level, depth)) = pending.pop() {
        if depth > SANDBOX_MAX_DEPTH {
            return None;
        }
        for entry in level.values() {
            count += 1;
            if let SandboxEntry::Directory(below) = entry {
                pending.push((below, depth + 1));
            }
        }
    }
    Some(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(name: &str) -> PathSegment {
        PathSegment::new(name).unwrap()
    }

    fn loc(where_: &str) -> ObjectLocation {
        ObjectLocation::new(where_).unwrap()
    }

    fn file(name: &str, where_: &str) -> (PathSegment, SandboxEntry) {
        (seg(name), SandboxEntry::File(loc(where_)))
    }

    /// A chain of directories `levels` maps deep, with one file at the bottom.
    fn chain(levels: usize) -> Entries {
        let mut entries = Entries::from([file("leaf", "s3://bucket/leaf")]);
        for n in (1..levels).rev() {
            entries = Entries::from([(seg(&format!("d{n}")), SandboxEntry::Directory(entries))]);
        }
        entries
    }

    #[test]
    fn a_segment_takes_a_name_and_refuses_a_path() {
        assert_eq!(seg("libc.so.6").as_str(), "libc.so.6");
        assert_eq!(seg("my file").to_string(), "my file");

        for (raw, ch) in [("bin/bash", '/'), ("bin\\bash", '\\'), ("tab\there", '\t')] {
            assert_eq!(
                PathSegment::new(raw),
                Err(AgentError::IllegalChar {
                    kind: PathSegment::KIND,
                    ch,
                }),
                "{raw:?}"
            );
        }
    }

    #[test]
    fn a_segment_may_not_name_a_place_instead_of_a_thing() {
        for raw in [".", ".."] {
            assert_eq!(
                PathSegment::new(raw),
                Err(AgentError::ReservedSegment {
                    value: raw.to_owned()
                }),
                "{raw:?}"
            );
        }
        // Only those two: a name that merely begins with a dot is an ordinary hidden file.
        assert_eq!(seg(".bashrc").as_str(), ".bashrc");
        assert_eq!(seg("...").as_str(), "...");
    }

    #[test]
    fn a_segment_is_refused_empty_and_past_its_ceiling() {
        assert_eq!(
            PathSegment::new(""),
            Err(AgentError::Empty {
                kind: PathSegment::KIND
            })
        );
        assert_eq!(
            PathSegment::new(&"a".repeat(SEGMENT_MAX_BYTES))
                .unwrap()
                .as_str()
                .len(),
            SEGMENT_MAX_BYTES
        );
        assert!(matches!(
            PathSegment::new(&"a".repeat(SEGMENT_MAX_BYTES + 1)),
            Err(AgentError::TooLong { .. })
        ));
    }

    #[test]
    fn a_location_is_taken_whatever_scheme_it_names() {
        for raw in [
            "s3://mybucket/bin/python",
            "gs://other/thing",
            "not-a-url-at-all",
        ] {
            assert_eq!(loc(raw).as_str(), raw);
        }
        assert_eq!(loc("s3://b/f").to_string(), "s3://b/f");
    }

    #[test]
    fn a_location_is_refused_empty_past_its_ceiling_and_with_a_control_character() {
        assert_eq!(
            ObjectLocation::new(""),
            Err(AgentError::Empty {
                kind: ObjectLocation::KIND
            })
        );
        assert!(matches!(
            ObjectLocation::new(&"a".repeat(LOCATION_MAX_BYTES + 1)),
            Err(AgentError::TooLong { .. })
        ));
        assert_eq!(
            ObjectLocation::new("s3://bucket/\nfile"),
            Err(AgentError::IllegalChar {
                kind: ObjectLocation::KIND,
                ch: '\n',
            })
        );
    }

    #[test]
    fn a_sandbox_reads_and_writes_the_tree_the_design_spells() {
        let written = r#"{"root":{"bin":{"bash":"s3://mybucket/bin/bash","python":"s3://mybucket/bin/python"},"lib":{"libc.so.6":"s3://mybucket/lib/libc.so.6"}}}"#;
        let sandbox: Sandbox = serde_json::from_str(written).unwrap();
        assert_eq!(serde_json::to_string(&sandbox).unwrap(), written);

        let Some(SandboxEntry::Directory(root)) = sandbox.entries().get(&seg("root")) else {
            panic!("root is a directory");
        };
        let Some(SandboxEntry::Directory(bin)) = root.get(&seg("bin")) else {
            panic!("bin is a directory");
        };
        assert_eq!(
            bin.get(&seg("python")),
            Some(&SandboxEntry::File(loc("s3://mybucket/bin/python")))
        );
    }

    #[test]
    fn a_sandbox_of_nothing_is_allowed() {
        let sandbox = Sandbox::new(Entries::new()).unwrap();
        assert!(sandbox.is_empty());
        assert_eq!(sandbox, Sandbox::default());
        assert_eq!(serde_json::to_string(&sandbox).unwrap(), "{}");
    }

    #[test]
    fn an_empty_directory_is_allowed_because_a_mount_point_is_one() {
        let sandbox = Sandbox::new(Entries::from([(
            seg("data"),
            SandboxEntry::Directory(Entries::new()),
        )]))
        .unwrap();
        assert!(!sandbox.is_empty());
        assert_eq!(serde_json::to_string(&sandbox).unwrap(), r#"{"data":{}}"#);
    }

    #[test]
    fn a_stored_tree_is_refused_a_segment_the_type_refuses() {
        let error = serde_json::from_str::<Sandbox>(r#"{"..":"s3://b/f"}"#).unwrap_err();
        assert!(error.to_string().contains("may not name"), "{error}");
    }

    #[test]
    fn a_refusal_deep_in_a_tree_says_which_reason_not_that_nothing_matched() {
        let error = serde_json::from_str::<Sandbox>(r#"{"root":{"bin":{"sh":""}}}"#).unwrap_err();
        assert!(
            error.to_string().contains("object location is empty"),
            "{error}"
        );
    }

    #[test]
    fn a_name_given_twice_in_one_directory_is_refused_rather_than_overwritten() {
        let error =
            serde_json::from_str::<Sandbox>(r#"{"bin":{"sh":"s3://b/one","sh":"s3://b/two"}}"#)
                .unwrap_err();
        assert!(error.to_string().contains("more than once"), "{error}");
    }

    #[test]
    fn a_tree_at_the_depth_limit_is_taken_and_one_level_past_it_is_not() {
        assert!(Sandbox::new(chain(SANDBOX_MAX_DEPTH)).is_ok());
        assert_eq!(
            Sandbox::new(chain(SANDBOX_MAX_DEPTH + 1)),
            Err(AgentError::TooDeep {
                kind: Sandbox::KIND,
                max: SANDBOX_MAX_DEPTH,
            })
        );
    }

    #[test]
    fn a_tree_at_the_entry_limit_is_taken_and_one_entry_past_it_is_not() {
        let mut entries = Entries::new();
        for n in 0..SANDBOX_MAX_ENTRIES {
            entries.insert(seg(&format!("f{n}")), SandboxEntry::File(loc("s3://b/f")));
        }
        assert!(Sandbox::new(entries.clone()).is_ok());

        entries.insert(seg("one-more"), SandboxEntry::File(loc("s3://b/f")));
        assert_eq!(
            Sandbox::new(entries),
            Err(AgentError::TooMany {
                kind: Sandbox::KIND,
                count: SANDBOX_MAX_ENTRIES + 1,
                max: SANDBOX_MAX_ENTRIES,
            })
        );
    }

    #[test]
    fn a_directory_counts_towards_the_limit_as_a_file_does() {
        let mut entries = Entries::new();
        for n in 0..SANDBOX_MAX_ENTRIES {
            entries.insert(
                seg(&format!("d{n}")),
                SandboxEntry::Directory(Entries::new()),
            );
        }
        assert!(Sandbox::new(entries.clone()).is_ok());
        entries.insert(seg("f"), SandboxEntry::File(loc("s3://b/f")));
        assert!(matches!(
            Sandbox::new(entries),
            Err(AgentError::TooMany { .. })
        ));
    }

    #[test]
    fn a_tree_nested_far_past_the_limit_is_refused_rather_than_read() {
        // A reader meets its own recursion limit before ours: what matters is that a body
        // nesting without bound is an error and not a crash.
        let deep = format!("{}{}", "{\"d\":".repeat(400), "\"s3://b/f\"") + &"}".repeat(400);
        assert!(serde_json::from_str::<Sandbox>(&deep).is_err());
    }
}
