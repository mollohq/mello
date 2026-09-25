package main

import (
	"crypto"
	"crypto/rsa"
	"crypto/sha256"
	"crypto/x509"
	"encoding/base64"
	"encoding/json"
	"encoding/pem"
	"io"
	"net/http"
	"net/http/httptest"
	"net/url"
	"strings"
	"testing"
)

const redirect = "http://localhost:29405/callback"

func newTestServer(t *testing.T) (*httptest.Server, *fake) {
	t.Helper()
	f, err := newFake(redirect)
	if err != nil {
		t.Fatal(err)
	}
	srv := httptest.NewServer(f.routes())
	t.Cleanup(srv.Close)
	return srv, f
}

// noRedirect returns a client that stops at the first redirect, so the test
// can read where the provider sends the browser.
func noRedirect() *http.Client {
	return &http.Client{CheckRedirect: func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse }}
}

func scriptNext(t *testing.T, base string, outcome Outcome, identity string) {
	t.Helper()
	body := `{"outcome":"` + string(outcome) + `","identity":"` + identity + `"}`
	resp, err := http.Post(base+"/_control/next", "application/json", strings.NewReader(body))
	if err != nil || resp.StatusCode != http.StatusNoContent {
		t.Fatalf("script next: %v %v", err, resp)
	}
}

// consent submits the consent form the way the browser does.
func consent(t *testing.T, base string, form url.Values) *http.Response {
	t.Helper()
	resp, err := noRedirect().PostForm(base+"/_consent", form)
	if err != nil {
		t.Fatal(err)
	}
	return resp
}

func implicitForm(provider, state, identity, decision string) url.Values {
	return url.Values{
		"provider": {provider}, "client_id": {"cid"}, "redirect_uri": {redirect},
		"state": {state}, "identity": {identity}, "decision": {decision},
	}
}

func fragment(t *testing.T, resp *http.Response) url.Values {
	t.Helper()
	loc, err := url.Parse(resp.Header.Get("Location"))
	if err != nil {
		t.Fatal(err)
	}
	v, err := url.ParseQuery(loc.Fragment)
	if err != nil {
		t.Fatal(err)
	}
	return v
}

func TestAuthorizeRejectsAWrongRedirectURI(t *testing.T) {
	srv, _ := newTestServer(t)
	resp, err := http.Get(srv.URL + "/api/oauth2/authorize?client_id=cid&response_type=token&redirect_uri=" +
		url.QueryEscape("http://evil.test/cb"))
	if err != nil {
		t.Fatal(err)
	}
	body, _ := io.ReadAll(resp.Body)
	if resp.StatusCode != http.StatusBadRequest || !strings.Contains(string(body), "redirect_uri_mismatch") {
		t.Fatalf("want redirect_uri_mismatch, got %d %s", resp.StatusCode, body)
	}
}

func TestAuthorizeShowsConsentForAValidRequest(t *testing.T) {
	srv, _ := newTestServer(t)
	resp, err := http.Get(srv.URL + "/api/oauth2/authorize?client_id=cid&response_type=token&state=s1&redirect_uri=" +
		url.QueryEscape(redirect))
	if err != nil {
		t.Fatal(err)
	}
	body, _ := io.ReadAll(resp.Body)
	for _, want := range []string{">Approve<", ">Deny<", `value="s1"`} {
		if !strings.Contains(string(body), want) {
			t.Fatalf("consent page lacks %q:\n%s", want, body)
		}
	}
}

func TestDiscordApproveReturnsTheStateAndAUsableToken(t *testing.T) {
	srv, _ := newTestServer(t)
	frag := fragment(t, consent(t, srv.URL, implicitForm("discord", "s1", "dana", "approve")))
	if frag.Get("state") != "s1" || frag.Get("access_token") == "" {
		t.Fatalf("fragment: %v", frag)
	}

	req, _ := http.NewRequest("GET", srv.URL+"/api/users/@me", nil)
	req.Header.Set("Authorization", "Bearer "+frag.Get("access_token"))
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	var user map[string]string
	_ = json.NewDecoder(resp.Body).Decode(&user)
	if resp.StatusCode != 200 || user["username"] != "dana" || user["id"] != stableID("discord", "dana") {
		t.Fatalf("users/@me: %d %v", resp.StatusCode, user)
	}
}

func TestDenyReturnsAccessDenied(t *testing.T) {
	srv, _ := newTestServer(t)
	frag := fragment(t, consent(t, srv.URL, implicitForm("twitch", "s1", "dana", "deny")))
	if frag.Get("error") != "access_denied" || frag.Get("access_token") != "" || frag.Get("state") != "s1" {
		t.Fatalf("fragment: %v", frag)
	}
}

func TestWrongStateOutcomeChangesTheStateOnce(t *testing.T) {
	srv, _ := newTestServer(t)
	scriptNext(t, srv.URL, OutcomeWrongState, "")
	if got := fragment(t, consent(t, srv.URL, implicitForm("discord", "s1", "dana", "approve"))).Get("state"); got == "s1" {
		t.Fatal("wrong_state kept the state")
	}
	if got := fragment(t, consent(t, srv.URL, implicitForm("discord", "s2", "dana", "approve"))).Get("state"); got != "s2" {
		t.Fatalf("the next consent is a plain approve again, got state %q", got)
	}
}

func TestRejectTokenApprovesButTheAPIRefuses(t *testing.T) {
	srv, _ := newTestServer(t)
	scriptNext(t, srv.URL, OutcomeRejectToken, "dana")
	token := fragment(t, consent(t, srv.URL, implicitForm("discord", "s1", "dana", "approve"))).Get("access_token")
	req, _ := http.NewRequest("GET", srv.URL+"/api/users/@me", nil)
	req.Header.Set("Authorization", "Bearer "+token)
	resp, _ := http.DefaultClient.Do(req)
	if token == "" || resp.StatusCode != http.StatusUnauthorized {
		t.Fatalf("want a token and a 401, got %q %d", token, resp.StatusCode)
	}
}

func TestTwitchUsersNeedsAKnownToken(t *testing.T) {
	srv, _ := newTestServer(t)
	req, _ := http.NewRequest("GET", srv.URL+"/helix/users", nil)
	req.Header.Set("Authorization", "Bearer made-up")
	resp, _ := http.DefaultClient.Do(req)
	if resp.StatusCode != http.StatusUnauthorized {
		t.Fatalf("want 401, got %d", resp.StatusCode)
	}
}

func pkce(verifier string) string {
	sum := sha256.Sum256([]byte(verifier))
	return base64.RawURLEncoding.EncodeToString(sum[:])
}

func googleCode(t *testing.T, base, verifier string) string {
	t.Helper()
	form := implicitForm("google", "s1", "gina", "approve")
	form.Set("code_challenge", pkce(verifier))
	loc, _ := url.Parse(consent(t, base, form).Header.Get("Location"))
	if loc.Query().Get("state") != "s1" || loc.Query().Get("code") == "" {
		t.Fatalf("google redirect: %v", loc)
	}
	return loc.Query().Get("code")
}

func exchange(base, code, verifier string) (*http.Response, map[string]any) {
	resp, _ := http.PostForm(base+"/token", url.Values{
		"code": {code}, "code_verifier": {verifier}, "redirect_uri": {redirect}, "client_id": {"cid"},
	})
	var body map[string]any
	_ = json.NewDecoder(resp.Body).Decode(&body)
	return resp, body
}

func TestGoogleAuthorizeRequiresPKCE(t *testing.T) {
	srv, _ := newTestServer(t)
	resp, _ := http.Get(srv.URL + "/o/oauth2/v2/auth?client_id=cid&response_type=code&redirect_uri=" + url.QueryEscape(redirect))
	if resp.StatusCode != http.StatusBadRequest {
		t.Fatalf("want 400 without a code_challenge, got %d", resp.StatusCode)
	}
}

func TestGoogleTokenRejectsAWrongVerifier(t *testing.T) {
	srv, _ := newTestServer(t)
	code := googleCode(t, srv.URL, "right-verifier-0123456789012345678901234567890123")
	resp, body := exchange(srv.URL, code, "wrong-verifier-0123456789012345678901234567890123")
	if resp.StatusCode != http.StatusBadRequest || body["error"] != "invalid_grant" {
		t.Fatalf("want invalid_grant, got %d %v", resp.StatusCode, body)
	}
}

// The id_token must pass the checks Nakama's CheckGoogleToken makes: an RS256
// signature by a certificate from /oauth2/v1/certs, issuer accounts.google.com,
// and sub, azp and aud present.
func TestGoogleIDTokenVerifiesAgainstThePublishedCertificate(t *testing.T) {
	srv, _ := newTestServer(t)
	verifier := "right-verifier-0123456789012345678901234567890123"
	resp, body := exchange(srv.URL, googleCode(t, srv.URL, verifier), verifier)
	if resp.StatusCode != 200 {
		t.Fatalf("token: %d %v", resp.StatusCode, body)
	}
	parts := strings.Split(body["id_token"].(string), ".")
	if len(parts) != 3 {
		t.Fatalf("not a JWT: %v", body["id_token"])
	}

	certsResp, _ := http.Get(srv.URL + "/oauth2/v1/certs")
	var certs map[string]string
	_ = json.NewDecoder(certsResp.Body).Decode(&certs)
	block, _ := pem.Decode([]byte(certs["fake-kid-1"]))
	cert, err := x509.ParseCertificate(block.Bytes)
	if err != nil {
		t.Fatal(err)
	}
	sig, _ := base64.RawURLEncoding.DecodeString(parts[2])
	digest := sha256.Sum256([]byte(parts[0] + "." + parts[1]))
	if err := rsa.VerifyPKCS1v15(cert.PublicKey.(*rsa.PublicKey), crypto.SHA256, digest[:], sig); err != nil {
		t.Fatalf("signature: %v", err)
	}

	raw, _ := base64.RawURLEncoding.DecodeString(parts[1])
	var claims map[string]any
	_ = json.Unmarshal(raw, &claims)
	if claims["iss"] != "https://accounts.google.com" || claims["sub"] == "" || claims["aud"] != "cid" || claims["azp"] != "cid" {
		t.Fatalf("claims: %v", claims)
	}

	// A code is single-use, as at Google.
	if again, _ := exchange(srv.URL, parts[0], verifier); again.StatusCode != http.StatusBadRequest {
		t.Fatal("a reused code must fail")
	}
}

func TestSteamKeepsTheAppStateAndValidates(t *testing.T) {
	srv, _ := newTestServer(t)
	returnTo := redirect + "?state=st1"
	resp, _ := http.Get(srv.URL + "/openid/login?openid.mode=checkid_setup&openid.return_to=" + url.QueryEscape(returnTo))
	if resp.StatusCode != 200 {
		t.Fatalf("checkid_setup: %d", resp.StatusCode)
	}
	form := implicitForm("steam", "", "sam", "approve")
	form.Set("redirect_uri", returnTo)
	loc, _ := url.Parse(consent(t, srv.URL, form).Header.Get("Location"))
	q := loc.Query()
	if q.Get("state") != "st1" || q.Get("openid.mode") != "id_res" || !strings.HasPrefix(q.Get("openid.claimed_id"), "https://steamcommunity.com/openid/id/7656119") {
		t.Fatalf("steam redirect: %v", q)
	}

	check := url.Values{}
	for k, v := range q {
		if strings.HasPrefix(k, "openid.") {
			check[k] = v
		}
	}
	check.Set("openid.mode", "check_authentication")
	r, _ := http.PostForm(srv.URL+"/openid/login", check)
	b, _ := io.ReadAll(r.Body)
	if !strings.Contains(string(b), "is_valid:true") {
		t.Fatalf("check_authentication: %s", b)
	}
}
