use std::io::{IsTerminal, Read};
use std::path::Path;

use ip_auth::{Passphrase, PassphraseHasher};
use ip_config::Config;
use ip_core::UserId;
use ip_storage::{AccountKind, NewUser};
use zeroize::Zeroizing;

use crate::cli::AdminCommand;
use crate::error::StartupError;
use crate::state::open_store;

/// Runs an administrative task against the store the configuration names.
pub async fn run(
    command: AdminCommand,
    config: &Path,
    passphrase: &Passphrase,
) -> Result<(), StartupError> {
    let AdminCommand::Create { user } = command;
    let config = Config::load(config)?;
    create_sysadmin(&config, user, passphrase).await
}

/// Asks for the passphrase the account will be created with.
///
/// At a terminal it is asked for twice and never echoed, since a passphrase nobody can see
/// is a passphrase that can be mistyped. Piped in, it is read once: a script that pipes the
/// same value twice has confirmed nothing.
pub fn ask_passphrase() -> Result<Passphrase, StartupError> {
    match std::io::stdin().is_terminal() {
        true => {
            // The prompt hands its buffer over rather than copying it, so wiping ours wipes it.
            let first =
                Zeroizing::new(rpassword::prompt_password("Passphrase: ").map_err(unreadable)?);
            let again = Zeroizing::new(
                rpassword::prompt_password("Repeat passphrase: ").map_err(unreadable)?,
            );
            confirmed(&first, &again)
        }
        false => read_passphrase(std::io::stdin()),
    }
}

/// Takes a passphrase only when it was typed the same way twice.
fn confirmed(first: &str, again: &str) -> Result<Passphrase, StartupError> {
    if first != again {
        return Err(StartupError::Usage {
            detail: "the passphrases do not match".to_owned(),
        });
    }
    Ok(Passphrase::new(first)?)
}

fn unreadable(source: std::io::Error) -> StartupError {
    StartupError::Usage {
        detail: format!("could not read the passphrase: {source}"),
    }
}

/// Creates the system administrator, which no api can do.
async fn create_sysadmin(
    config: &Config,
    user: UserId,
    passphrase: &Passphrase,
) -> Result<(), StartupError> {
    let store = open_store(&config.storage).await?;
    let hashed = PassphraseHasher::new().hash(passphrase)?;
    store
        .create_user(NewUser {
            id: user.clone(),
            passphrase: hashed,
            kind: AccountKind::SystemAdministrator,
        })
        .await?;
    println!("created the system administrator {user}");
    Ok(())
}

/// Reads a piped passphrase, which never sits in a shell history.
fn read_passphrase(mut source: impl Read) -> Result<Passphrase, StartupError> {
    let mut raw = Zeroizing::new(String::new());
    source.read_to_string(&mut raw).map_err(unreadable)?;
    Ok(Passphrase::new(raw.trim_end_matches(['\r', '\n']))?)
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "standalone-storage")]
    use ip_storage::UserStore;

    use super::*;

    #[test]
    fn a_passphrase_keeps_what_was_typed_without_its_newline() {
        let read = read_passphrase("correct horse staple\n".as_bytes()).unwrap();
        let hashed = PassphraseHasher::new().hash(&read).unwrap();
        assert!(
            PassphraseHasher::new()
                .verify(&Passphrase::new("correct horse staple").unwrap(), &hashed)
                .unwrap()
        );
    }

    #[test]
    fn a_passphrase_typed_twice_the_same_way_is_taken() {
        let taken = confirmed("correct horse staple", "correct horse staple").unwrap();
        let hashed = PassphraseHasher::new().hash(&taken).unwrap();
        assert!(
            PassphraseHasher::new()
                .verify(&Passphrase::new("correct horse staple").unwrap(), &hashed)
                .unwrap()
        );
    }

    #[test]
    fn a_passphrase_typed_differently_the_second_time_is_refused() {
        let refused = confirmed("correct horse staple", "correct horse stapler");
        assert!(matches!(
            refused,
            Err(StartupError::Usage { ref detail }) if detail.contains("do not match")
        ));
    }

    #[test]
    fn a_confirmed_passphrase_the_policy_refuses_is_still_refused() {
        assert!(matches!(
            confirmed("short", "short"),
            Err(StartupError::Auth(_))
        ));
    }

    #[test]
    fn a_passphrase_the_policy_refuses_stops_the_command() {
        assert!(matches!(
            read_passphrase("short".as_bytes()),
            Err(StartupError::Auth(_))
        ));
    }

    /// A configuration naming a sqlite file of this test's own.
    #[cfg(all(feature = "standalone-storage", feature = "standalone-cache"))]
    fn scratch_config(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let database = std::env::temp_dir().join(format!("ip-{tag}-{}.db", std::process::id()));
        let written = std::env::temp_dir().join(format!("ip-{tag}-{}.toml", std::process::id()));
        let _ = std::fs::remove_file(&database);
        std::fs::write(
            &written,
            format!(
                r#"
[server]
listen = "127.0.0.1:8080"

[storage]
backend = "sqlite"
path = {:?}

[cache]
backend = "sled"
path = "./unopened"

[auth.jwt]
issuer = "intelli-prism"
secret = "0123456789abcdef0123456789abcdef"
"#,
                database.display().to_string()
            ),
        )
        .unwrap();
        (written, database)
    }

    #[cfg(all(feature = "standalone-storage", feature = "standalone-cache"))]
    #[tokio::test]
    async fn the_created_account_is_the_system_administrator() {
        let (config, database) = scratch_config("admin-created");
        run(
            AdminCommand::Create {
                user: UserId::new("root").unwrap(),
            },
            &config,
            &Passphrase::new("correct horse staple").unwrap(),
        )
        .await
        .unwrap();

        let store = ip_storage::SqliteStore::open(&database).await.unwrap();
        let created = store
            .user(&UserId::new("root").unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(created.kind, AccountKind::SystemAdministrator);
        assert!(
            PassphraseHasher::new()
                .verify(
                    &Passphrase::new("correct horse staple").unwrap(),
                    &created.passphrase
                )
                .unwrap()
        );
        let _ = std::fs::remove_file(&config);
        let _ = std::fs::remove_file(&database);
    }

    #[cfg(all(feature = "standalone-storage", feature = "standalone-cache"))]
    #[tokio::test]
    async fn creating_the_same_administrator_twice_conflicts() {
        let (config, database) = scratch_config("admin-twice");
        let command = || AdminCommand::Create {
            user: UserId::new("root").unwrap(),
        };
        let typed = Passphrase::new("correct horse staple").unwrap();
        run(command(), &config, &typed).await.unwrap();
        let again = run(command(), &config, &typed).await;
        let _ = std::fs::remove_file(&config);
        let _ = std::fs::remove_file(&database);
        assert!(matches!(again, Err(StartupError::Storage(_))));
    }
}
