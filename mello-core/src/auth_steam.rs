use crate::oauth::{OAuthError, OAuthFlow, OAuthMode, REDIRECT_URI};

pub struct SteamAuth;

impl SteamAuth {
    /// Run the Steam OpenID 2.0 browser flow (blocking). Returns the raw `openid.*`
    /// response query string; the backend verifies it via `check_authentication`
    /// and derives the steamid. Must be called from a blocking context.
    pub fn authenticate() -> Result<String, OAuthError> {
        // OpenID requires `return_to` to live under `realm`.
        let realm = REDIRECT_URI
            .rsplit_once('/')
            .map(|(base, _)| base)
            .unwrap_or(REDIRECT_URI);

        let id_select = "http://specs.openid.net/auth/2.0/identifier_select";
        let auth_url = format!(
            "https://steamcommunity.com/openid/login\
             ?openid.ns={ns}\
             &openid.mode=checkid_setup\
             &openid.return_to={return_to}\
             &openid.realm={realm}\
             &openid.identity={id_select}\
             &openid.claimed_id={id_select}",
            ns = urlencoding::encode("http://specs.openid.net/auth/2.0"),
            return_to = urlencoding::encode(REDIRECT_URI),
            realm = urlencoding::encode(realm),
            id_select = urlencoding::encode(id_select),
        );

        OAuthFlow::execute(&auth_url, OAuthMode::OpenIDQuery)
    }
}
