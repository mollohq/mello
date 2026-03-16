use crate::oauth::{OAuthError, OAuthFlow, OAuthMode, PkceChallenge, REDIRECT_URI};

pub struct GoogleAuth;

impl GoogleAuth {
    /// Run the full Google OAuth2 PKCE browser flow (blocking).
    /// Returns `(authorization_code, pkce_verifier)`.
    pub fn authenticate(client_id: &str) -> Result<(String, String), OAuthError> {
        let pkce = PkceChallenge::generate();

        let auth_url = format!(
            "https://accounts.google.com/o/oauth2/v2/auth\
             ?client_id={client_id}\
             &redirect_uri={redirect_uri}\
             &response_type=code\
             &scope=openid%20profile%20email\
             &code_challenge={challenge}\
             &code_challenge_method=S256",
            client_id = client_id,
            redirect_uri = urlencoding::encode(REDIRECT_URI),
            challenge = pkce.challenge,
        );

        let code = OAuthFlow::execute(&auth_url, OAuthMode::AuthorizationCode)?;
        Ok((code, pkce.verifier))
    }
}
