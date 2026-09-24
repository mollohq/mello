use crate::oauth::{generate_state, OAuthError, OAuthFlow, OAuthMode, REDIRECT_URI};

pub struct SteamAuth;

impl SteamAuth {
    /// Run the Steam OpenID 2.0 browser flow (blocking). Returns the raw `openid.*`
    /// response query string; the backend verifies it via `check_authentication`
    /// and derives the steamid. Must be called from a blocking context.
    pub fn authenticate() -> Result<String, OAuthError> {
        let state = generate_state();
        let auth_url = Self::authorize_url(&state);

        OAuthFlow::execute(&auth_url, &state, OAuthMode::OpenIDQuery)
    }

    /// OpenID 2.0 has no `state` parameter. The nonce goes in `return_to`, and
    /// Steam appends its `openid.*` response to that URL.
    fn authorize_url(state: &str) -> String {
        // OpenID requires `return_to` to live under `realm`.
        let realm = REDIRECT_URI
            .rsplit_once('/')
            .map(|(base, _)| base)
            .unwrap_or(REDIRECT_URI);
        let return_to = format!("{REDIRECT_URI}?state={state}");

        let id_select = "http://specs.openid.net/auth/2.0/identifier_select";
        format!(
            "https://steamcommunity.com/openid/login\
             ?openid.ns={ns}\
             &openid.mode=checkid_setup\
             &openid.return_to={return_to}\
             &openid.realm={realm}\
             &openid.identity={id_select}\
             &openid.claimed_id={id_select}",
            ns = urlencoding::encode("http://specs.openid.net/auth/2.0"),
            return_to = urlencoding::encode(&return_to),
            realm = urlencoding::encode(realm),
            id_select = urlencoding::encode(id_select),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn return_to_carries_state_under_realm() {
        let url = url::Url::parse(&SteamAuth::authorize_url("st4te")).unwrap();
        let get = |key: &str| {
            url.query_pairs()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.into_owned())
        };

        let return_to = get("openid.return_to").expect("return_to present");
        assert_eq!(return_to, format!("{REDIRECT_URI}?state=st4te"));
        let realm = get("openid.realm").expect("realm present");
        assert!(return_to.starts_with(&realm));
    }
}
