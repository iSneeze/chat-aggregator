//! Where the refresh tokens live between runs. A refresh token is the
//! long-lived secret (it can mint access tokens until revoked), so it goes
//! into the OS keyring, through the store all secrets share
//! (`chat_core::secrets`).

use super::OAuthApp;

pub use chat_core::secrets::SecretStore as TokenStore;

/// Which stored login a call is about.
///
/// A login belongs to one channel (Google's consent page asks which), so a
/// streamer with several channels has several logins, one per channel.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Account {
    /// A channel's login, by its channel id (see [`crate::OwnChannel`]).
    Channel(String),
    /// The single login stored before several channels could be connected,
    /// whose channel isn't known without asking YouTube. It keeps working
    /// until it's been moved to its channel's name (see
    /// [`super::adopt_legacy_login`]).
    Legacy,
}

/// The token's name in the store. Always with the client id: switching to
/// another Google project must not reuse a token that belongs to a
/// different one.
pub(crate) fn secret_name(app: &OAuthApp, account: &Account) -> String {
    match account {
        // The name logins were stored under before: kept, so they're found.
        Account::Legacy => format!("youtube:{}", app.client_id()),
        Account::Channel(id) => format!("youtube:{}:{id}", app.client_id()),
    }
}
