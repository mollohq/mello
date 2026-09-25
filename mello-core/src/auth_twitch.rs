use crate::oauth::{generate_state, OAuthError, OAuthFlow, OAuthMode, REDIRECT_URI};

pub struct TwitchAuth;

impl TwitchAuth {
    /// Run the Twitch OAuth2 implicit browser flow (blocking).
    /// Returns the `access_token`, which the backend validates via Helix.
    pub fn authenticate(client_id: &str) -> Result<String, OAuthError> {
        let state = generate_state();
        let auth_url = Self::authorize_url(client_id, &state);

        OAuthFlow::execute(&auth_url, &state, OAuthMode::Implicit)
    }

    fn authorize_url(client_id: &str, state: &str) -> String {
        format!(
            "https://id.twitch.tv/oauth2/authorize\
             ?client_id={client_id}\
             &redirect_uri={redirect_uri}\
             &response_type=token\
             &scope={scope}\
             &state={state}",
            redirect_uri = urlencoding::encode(REDIRECT_URI),
            scope = urlencoding::encode("user:read:email"),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorize_url_carries_state() {
        let url = url::Url::parse(&TwitchAuth::authorize_url("cid", "st4te")).unwrap();
        let pairs: Vec<(String, String)> = url.query_pairs().into_owned().collect();

        assert!(pairs.contains(&("state".into(), "st4te".into())));
        assert!(pairs.contains(&("response_type".into(), "token".into())));
    }
}
