use crate::oauth::{generate_state, OAuthError, OAuthFlow, OAuthMode, PkceChallenge, REDIRECT_URI};

pub struct GoogleAuth;

impl GoogleAuth {
    /// Run the full Google OAuth2 PKCE browser flow (blocking).
    /// Returns `(authorization_code, pkce_verifier)`.
    pub fn authenticate(client_id: &str) -> Result<(String, String), OAuthError> {
        let pkce = PkceChallenge::generate();
        let state = generate_state();
        let auth_url = Self::authorize_url(client_id, &pkce.challenge, &state);

        let code = OAuthFlow::execute(&auth_url, &state, OAuthMode::AuthorizationCode)?;
        Ok((code, pkce.verifier))
    }

    fn authorize_url(client_id: &str, challenge: &str, state: &str) -> String {
        format!(
            "https://accounts.google.com/o/oauth2/v2/auth\
             ?client_id={client_id}\
             &redirect_uri={redirect_uri}\
             &response_type=code\
             &scope=openid%20profile%20email\
             &code_challenge={challenge}\
             &code_challenge_method=S256\
             &state={state}",
            redirect_uri = urlencoding::encode(REDIRECT_URI),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorize_url_carries_state_and_pkce_challenge() {
        let url = url::Url::parse(&GoogleAuth::authorize_url("cid", "chal", "st4te")).unwrap();
        let pairs: Vec<(String, String)> = url.query_pairs().into_owned().collect();

        assert!(pairs.contains(&("state".into(), "st4te".into())));
        assert!(pairs.contains(&("code_challenge".into(), "chal".into())));
        assert!(pairs.contains(&("redirect_uri".into(), REDIRECT_URI.into())));
    }
}
