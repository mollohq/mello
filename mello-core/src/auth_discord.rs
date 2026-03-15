use crate::oauth::{OAuthFlow, OAuthMode, OAuthError, REDIRECT_URI};

pub struct DiscordAuth;

impl DiscordAuth {
    /// Run the Discord OAuth2 implicit browser flow (blocking).
    /// Returns the `access_token`.
    pub fn authenticate(client_id: &str) -> Result<String, OAuthError> {
        let auth_url = format!(
            "https://discord.com/api/oauth2/authorize\
             ?client_id={client_id}\
             &redirect_uri={redirect_uri}\
             &response_type=token\
             &scope=identify",
            client_id = client_id,
            redirect_uri = urlencoding::encode(REDIRECT_URI),
        );

        OAuthFlow::execute(&auth_url, OAuthMode::Implicit)
    }
}
