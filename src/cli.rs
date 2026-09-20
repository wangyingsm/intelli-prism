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
    fn the_command_tree_is_one_clap_accepts() {
        use clap::CommandFactory;

        Cli::command().debug_assert();
    }
}
