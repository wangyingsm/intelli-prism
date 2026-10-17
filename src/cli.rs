use std::path::PathBuf;

use clap::{Parser, Subcommand};
use ip_core::UserId;

use crate::DEFAULT_CONFIG_PATH;

/// An enterprise AI proxy and gateway.
#[derive(Debug, Parser)]
#[command(name = "intelli-prism", version, about, long_about = None)]
pub struct Cli {
    /// Configuration file to read.
    #[arg(short, long, global = true, default_value = DEFAULT_CONFIG_PATH)]
    pub config: PathBuf,

    /// What to do. Serves the gateway when nothing is named.
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// What the binary was asked to do.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Administer the store the configuration names, from the machine it runs on.
    Admin {
        /// Which administrative task to run.
        #[command(subcommand)]
        command: AdminCommand,
    },

    /// Get the classifier's artefact onto this machine, or check one already here.
    ///
    /// The gateway itself never fetches anything: reaching the internet at startup
    /// would undo the one thing this product claims. Getting the model is a thing an
    /// operator does once, deliberately, before serving.
    Model {
        /// Which to do.
        #[command(subcommand)]
        command: ModelCommand,
    },
}

/// Getting an artefact, or proving the one that is here.
#[derive(Debug, Subcommand)]
pub enum ModelCommand {
    /// Check an artefact already on this machine, reaching nothing.
    ///
    /// This is the air-gapped path, and it is the primary one: an operator may carry
    /// the artefact in on a disk and never give this machine a route out.
    Verify {
        /// The artefact directory to check.
        #[arg(long)]
        at: PathBuf,
    },

    /// Download an artefact, verify it, and place it where configuration expects.
    Fetch {
        /// Where from: a directory on this machine, an `https://` prefix, or a
        /// repository as `hf:owner/name` or `ms:owner/name`.
        ///
        /// Both hosts are first class. The model is half Chinese and Hugging Face is
        /// slow from much of Asia, so the mirror is a source and not a fallback.
        #[arg(long)]
        from: String,

        /// The artefact directory to write.
        #[arg(long)]
        into: PathBuf,

        /// Replace an artefact already there, which a running gateway may be serving.
        #[arg(long)]
        replace: bool,
    },
}

/// A task that writes to the store without going through the api.
#[derive(Debug, Subcommand)]
pub enum AdminCommand {
    /// Create the system administrator, asking for its passphrase.
    ///
    /// Only a system administrator is created here, since every other account is made
    /// through the api. No api creates this one: an api that did would be a way to climb to
    /// the top of the system from inside it, so shell access to this machine is the credential.
    Create {
        /// The user id to create.
        #[arg(value_parser = user_id)]
        user: UserId,
    },
}

/// Reads a user id, refusing on the command line whatever the system would refuse later.
fn user_id(raw: &str) -> Result<UserId, String> {
    UserId::new(raw).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(words: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(words)
    }

    #[test]
    fn naming_nothing_serves_the_gateway_from_the_default_configuration() {
        let cli = parse(&["intelli-prism"]).unwrap();
        assert!(cli.command.is_none());
        assert_eq!(cli.config, PathBuf::from(DEFAULT_CONFIG_PATH));
    }

    #[test]
    fn the_configuration_can_be_named_before_or_after_the_subcommand() {
        for words in [
            ["intelli-prism", "--config", "other.toml", "admin"].as_slice(),
            ["intelli-prism", "admin", "--config", "other.toml"].as_slice(),
        ] {
            let mut words = words.to_vec();
            words.extend(["create", "root"]);
            let cli = parse(&words).unwrap();
            assert_eq!(cli.config, PathBuf::from("other.toml"));
        }
    }

    #[test]
    fn create_names_the_user_it_creates() {
        let cli = parse(&["intelli-prism", "admin", "create", "root"]).unwrap();
        let Some(Command::Admin {
            command: AdminCommand::Create { user },
        }) = cli.command
        else {
            panic!("expected create");
        };
        assert_eq!(user, UserId::new("root").unwrap());
    }

    #[test]
    fn a_user_id_the_system_would_refuse_is_refused_here_too() {
        let refused = parse(&["intelli-prism", "admin", "create", "not a user id"]);
        assert!(refused.is_err());
    }

    #[test]
    fn a_subcommand_that_does_not_exist_is_refused() {
        assert!(parse(&["intelli-prism", "admin", "become-root"]).is_err());
        assert!(parse(&["intelli-prism", "admin"]).is_err());
        assert!(parse(&["intelli-prism", "whoami"]).is_err());
    }

    #[test]
    fn create_without_a_user_is_refused() {
        assert!(parse(&["intelli-prism", "admin", "create"]).is_err());
    }

    #[test]
    fn verifying_names_only_where_to_look() {
        let cli = parse(&[
            "intelli-prism",
            "model",
            "verify",
            "--at",
            "/srv/difficulty",
        ])
        .unwrap();
        let Some(Command::Model {
            command: ModelCommand::Verify { at },
        }) = cli.command
        else {
            panic!("expected verify");
        };
        assert_eq!(at, PathBuf::from("/srv/difficulty"));
    }

    #[test]
    fn fetching_names_where_from_and_where_to() {
        let cli = parse(&[
            "intelli-prism",
            "model",
            "fetch",
            "--from",
            "hf:owner/name",
            "--into",
            "/srv/difficulty",
            "--replace",
        ])
        .unwrap();
        let Some(Command::Model {
            command:
                ModelCommand::Fetch {
                    from,
                    into,
                    replace,
                },
        }) = cli.command
        else {
            panic!("expected fetch");
        };
        assert_eq!(from, "hf:owner/name");
        assert_eq!(into, PathBuf::from("/srv/difficulty"));
        assert!(replace);
    }

    /// Nothing turns verification off, so there is no flag to find that would.
    #[test]
    fn no_flag_skips_the_checking() {
        for skipping in ["--no-verify", "--skip-verify", "--force", "--insecure"] {
            assert!(
                parse(&[
                    "intelli-prism",
                    "model",
                    "fetch",
                    "--from",
                    "hf:a/b",
                    "--into",
                    "/srv",
                    skipping,
                ])
                .is_err(),
                "{skipping} was accepted"
            );
        }
    }

    #[test]
    fn fetching_without_saying_where_from_or_to_is_refused() {
        assert!(parse(&["intelli-prism", "model", "fetch", "--into", "/srv"]).is_err());
        assert!(parse(&["intelli-prism", "model", "fetch", "--from", "hf:a/b"]).is_err());
        assert!(parse(&["intelli-prism", "model", "verify"]).is_err());
        assert!(parse(&["intelli-prism", "model"]).is_err());
    }

    #[test]
    fn the_command_tree_is_one_clap_accepts() {
        use clap::CommandFactory;

        Cli::command().debug_assert();
    }
}
