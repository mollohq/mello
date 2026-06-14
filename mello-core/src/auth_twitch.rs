use crate::oauth::{OAuthError, OAuthFlow, OAuthMode, REDIRECT_URI};

pub struct TwitchAuth;

impl TwitchAuth {
    /// Run the Twitch OAuth2 implicit browser flow (blocking).
    /// Returns the `access_token`, which the backend validates via Helix.
    pub fn authenticate(client_id: &str) -> Result<String, OAuthError> {
        let auth_url = format!(
            "https://id.twitch.tv/oauth2/authorize\
             ?client_id={client_id}\
             &redirect_uri={redirect_uri}\
             &response_type=token\
             &scope={scope}",
            client_id = client_id,
            redirect_uri = urlencoding::encode(REDIRECT_URI),
            scope = urlencoding::encode("user:read:email"),
        );

        OAuthFlow::execute(&auth_url, OAuthMode::Implicit)
    }
}
