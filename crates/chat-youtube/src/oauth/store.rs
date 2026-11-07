//! Where the refresh token lives between runs. The refresh token is the
//! long-lived secret (it can mint access tokens until revoked), so it goes
//! into the OS keyring, through the store all secrets share
//! (`chat_core::secrets`).

use super::OAuthApp;

pub use chat_core::secrets::SecretStore as TokenStore;

/// The token's name in the store. One per OAuth client: switching to
/// another Google project must not reuse a token that belongs to a
/// different one.
pub(crate) fn account(app: &OAuthApp) -> String {
    format!("youtube:{}", app.client_id())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_name_stays_what_existing_logins_are_stored_under() {
        let app = OAuthApp::new("123-abc.apps.googleusercontent.com", "secret");
        assert_eq!(account(&app), "youtube:123-abc.apps.googleusercontent.com");
    }
}
