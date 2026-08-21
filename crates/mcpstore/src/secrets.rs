//! Per-server credentials, held in the OS keychain rather than the database.
//!
//! An API key or OAuth refresh token in a SQLite file is a plaintext secret in
//! a file the user will happily copy around, sync, and back up. The keychain is
//! where those belong, and putting them there is also what makes "persisted
//! auth" a defensible feature rather than a liability.
//!
//! These functions are deliberately thin and are not unit-tested: exercising
//! them means writing to the real login keychain, which would prompt on a
//! developer machine and has nowhere to go in CI.

use crate::{Error, Result, ServerId};

/// Keychain service name. Changing it orphans every stored credential.
const SERVICE: &str = "mcpi";

fn entry(server_id: ServerId) -> Result<keyring::Entry> {
    Ok(keyring::Entry::new(
        SERVICE,
        &format!("server-{server_id}"),
    )?)
}

pub fn set(server_id: ServerId, secret: &str) -> Result<()> {
    entry(server_id)?.set_password(secret)?;
    Ok(())
}

/// The stored secret, or `None` when the server has none.
pub fn get(server_id: ServerId) -> Result<Option<String>> {
    match entry(server_id)?.get_password() {
        Ok(secret) => Ok(Some(secret)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(Error::Keyring(e)),
    }
}

/// Remove the secret. Succeeds when there was nothing to remove.
pub fn delete(server_id: ServerId) -> Result<()> {
    match entry(server_id)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(Error::Keyring(e)),
    }
}
