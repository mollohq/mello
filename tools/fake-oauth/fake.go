package main

import (
	"crypto"
	"crypto/rand"
	"crypto/rsa"
	"crypto/sha256"
	"crypto/x509"
	"crypto/x509/pkix"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"encoding/pem"
	"fmt"
	"html/template"
	"log"
	"math/big"
	"net/http"
	"net/url"
	"strings"
	"sync"
	"time"
)

// Outcome is what the next consent does. The driver sets it before a journey
// clicks a sign-in button; after one consent it returns to OutcomeApprove.
type Outcome string

const (
	OutcomeApprove     Outcome = "approve"      // the user approves
	OutcomeDeny        Outcome = "deny"         // the user denies: error=access_denied
	OutcomeWrongState  Outcome = "wrong_state"  // redirect with a different state
	OutcomeNoCallback  Outcome = "no_callback"  // the user closes the browser
	OutcomeRejectToken Outcome = "reject_token" // approve, but the API refuses the token later
	OutcomeDown        Outcome = "down"         // approve, but the API answers 500 later
)

type script struct {
	Outcome  Outcome `json:"outcome"`
	Identity string  `json:"identity"`
}

// grant is what the fake remembers about a token or a code.
type grant struct {
	Provider  string
	Identity  string
	Outcome   Outcome
	ClientID  string
	Challenge string // Google PKCE S256 challenge
	Redirect  string
}

type logEntry struct {
	Time     time.Time `json:"time"`
	Provider string    `json:"provider"`
	Event    string    `json:"event"`
	Detail   string    `json:"detail,omitempty"`
}

type fake struct {
	redirectURI string

	mu     sync.Mutex
	next   script
	tokens map[string]grant // access tokens, Steam claimed IDs
	codes  map[string]grant // Google authorization codes
	log    []logEntry

	googleKey  *rsa.PrivateKey
	googleKid  string
	googleCert []byte // PEM
}

func newFake(redirectURI string) (*fake, error) {
	key, err := rsa.GenerateKey(rand.Reader, 2048)
	if err != nil {
		return nil, err
	}
	// Nakama reads Google's keys as PEM X.509 certificates by key ID.
	tmpl := &x509.Certificate{
		SerialNumber: big.NewInt(3),
		Subject:      pkix.Name{CommonName: "fake Google id_token signer"},
		NotBefore:    time.Now().Add(-time.Hour),
		NotAfter:     time.Now().Add(7 * 24 * time.Hour),
	}
	der, err := x509.CreateCertificate(rand.Reader, tmpl, tmpl, &key.PublicKey, key)
	if err != nil {
		return nil, err
	}
	return &fake{
		redirectURI: redirectURI,
		next:        script{Outcome: OutcomeApprove},
		tokens:      map[string]grant{},
		codes:       map[string]grant{},
		googleKey:   key,
		googleKid:   "fake-kid-1",
		googleCert:  pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: der}),
	}, nil
}

func (f *fake) record(provider, event, detail string) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.log = append(f.log, logEntry{time.Now(), provider, event, detail})
	log.Printf("fake-oauth: %s %s %s", provider, event, detail)
}

// takeNext returns the scripted outcome and resets it to a plain approve.
func (f *fake) takeNext() script {
	f.mu.Lock()
	defer f.mu.Unlock()
	s := f.next
	f.next = script{Outcome: OutcomeApprove}
	return s
}

func (f *fake) routes() *http.ServeMux {
	m := http.NewServeMux()
	m.HandleFunc("/healthz", func(w http.ResponseWriter, _ *http.Request) { fmt.Fprint(w, "ok") })
	m.HandleFunc("/_control/next", f.controlNext)
	m.HandleFunc("/_control/log", f.controlLog)
	m.HandleFunc("/_consent", f.consentSubmit)

	m.HandleFunc("/api/oauth2/authorize", f.authorize("discord"))
	m.HandleFunc("/oauth2/authorize", f.authorize("twitch"))
	m.HandleFunc("/o/oauth2/v2/auth", f.authorize("google"))
	m.HandleFunc("/openid/login", f.steamLogin)

	m.HandleFunc("/token", f.googleToken)
	m.HandleFunc("/oauth2/v1/certs", f.googleCerts)
	m.HandleFunc("/api/users/@me", f.discordMe)
	m.HandleFunc("/helix/users", f.twitchUsers)
	m.HandleFunc("/ISteamUser/GetPlayerSummaries/v2/", f.steamPersona)
	return m
}

// ── Control ──────────────────────────────────────────────────────────

func (f *fake) controlNext(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodPost {
		http.Error(w, "POST only", http.StatusMethodNotAllowed)
		return
	}
	var s script
	if err := json.NewDecoder(r.Body).Decode(&s); err != nil {
		http.Error(w, err.Error(), http.StatusBadRequest)
		return
	}
	switch s.Outcome {
	case OutcomeApprove, OutcomeDeny, OutcomeWrongState, OutcomeNoCallback, OutcomeRejectToken, OutcomeDown:
	default:
		http.Error(w, "unknown outcome "+string(s.Outcome), http.StatusBadRequest)
		return
	}
	f.mu.Lock()
	f.next = s
	f.mu.Unlock()
	f.record("control", "next", fmt.Sprintf("%s as %q", s.Outcome, s.Identity))
	w.WriteHeader(http.StatusNoContent)
}

func (f *fake) controlLog(w http.ResponseWriter, _ *http.Request) {
	f.mu.Lock()
	defer f.mu.Unlock()
	w.Header().Set("Content-Type", "application/json")
	_ = json.NewEncoder(w).Encode(f.log)
}

// ── Browser: authorize and consent ───────────────────────────────────

var consentPage = template.Must(template.New("consent").Parse(`<!doctype html>
<html><head><meta charset="utf-8"><title>Fake {{.Provider}} sign-in</title></head>
<body style="font-family: sans-serif; max-width: 420px; margin: 60px auto">
<h1>Fake {{.Provider}}</h1>
<p><strong>mello</strong> wants to sign you in.</p>
<form method="post" action="/_consent">
  {{range $k, $v := .Hidden}}<input type="hidden" name="{{$k}}" value="{{$v}}">{{end}}
  <label>Signed in as <input name="identity" value="{{.Identity}}"></label>
  <p><button name="decision" value="approve">Approve</button>
     <button name="decision" value="deny">Deny</button></p>
</form>
</body></html>`))

var errorPage = template.Must(template.New("error").Parse(`<!doctype html>
<html><head><title>Fake {{.Provider}} error</title></head>
<body><h1>{{.Code}}</h1><p>{{.Detail}}</p></body></html>`))

// authorize validates the request like the real provider and shows consent.
func (f *fake) authorize(provider string) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		q := r.URL.Query()
		if q.Get("client_id") == "" {
			f.fail(w, provider, "invalid_client", "client_id is missing")
			return
		}
		if q.Get("redirect_uri") != f.redirectURI {
			f.fail(w, provider, "redirect_uri_mismatch", fmt.Sprintf("got %q", q.Get("redirect_uri")))
			return
		}
		want := map[string]string{"discord": "token", "twitch": "token", "google": "code"}[provider]
		if q.Get("response_type") != want {
			f.fail(w, provider, "unsupported_response_type", fmt.Sprintf("want %q, got %q", want, q.Get("response_type")))
			return
		}
		if provider == "google" && (q.Get("code_challenge") == "" || q.Get("code_challenge_method") != "S256") {
			f.fail(w, provider, "invalid_request", "PKCE S256 code_challenge is required")
			return
		}
		f.record(provider, "authorize", "state="+q.Get("state"))
		f.showConsent(w, provider, map[string]string{
			"provider":       provider,
			"client_id":      q.Get("client_id"),
			"redirect_uri":   q.Get("redirect_uri"),
			"state":          q.Get("state"),
			"code_challenge": q.Get("code_challenge"),
		})
	}
}

func (f *fake) showConsent(w http.ResponseWriter, provider string, hidden map[string]string) {
	f.mu.Lock()
	identity := f.next.Identity
	f.mu.Unlock()
	if identity == "" {
		identity = "player-" + randHex(3)
	}
	w.Header().Set("Content-Type", "text/html; charset=utf-8")
	_ = consentPage.Execute(w, map[string]any{"Provider": provider, "Hidden": hidden, "Identity": identity})
}

func (f *fake) fail(w http.ResponseWriter, provider, code, detail string) {
	f.record(provider, "error", code+": "+detail)
	w.WriteHeader(http.StatusBadRequest)
	_ = errorPage.Execute(w, map[string]string{"Provider": provider, "Code": code, "Detail": detail})
}

// consentSubmit sends the browser back to the app, per provider and outcome.
func (f *fake) consentSubmit(w http.ResponseWriter, r *http.Request) {
	if err := r.ParseForm(); err != nil {
		http.Error(w, err.Error(), http.StatusBadRequest)
		return
	}
	provider := r.PostForm.Get("provider")
	identity := r.PostForm.Get("identity")
	state := r.PostForm.Get("state")
	redirect := r.PostForm.Get("redirect_uri")
	s := f.takeNext()
	outcome := s.Outcome
	if r.PostForm.Get("decision") == "deny" {
		outcome = OutcomeDeny
	}
	f.record(provider, "consent", fmt.Sprintf("%s as %q", outcome, identity))

	switch outcome {
	case OutcomeNoCallback:
		// The user closes the tab: nothing reaches the app.
		fmt.Fprint(w, "You closed the window.")
		return
	case OutcomeWrongState:
		state = "not-" + state
	}

	g := grant{Provider: provider, Identity: identity, Outcome: outcome, ClientID: r.PostForm.Get("client_id"), Redirect: redirect}
	switch provider {
	case "discord", "twitch":
		frag := url.Values{"state": {state}}
		if outcome == OutcomeDeny {
			frag.Set("error", "access_denied")
		} else {
			token := fmt.Sprintf("fake-%s-%s", provider, randHex(8))
			f.putToken(token, g)
			frag.Set("access_token", token)
			frag.Set("token_type", "Bearer")
		}
		http.Redirect(w, r, redirect+"#"+frag.Encode(), http.StatusFound)
	case "google":
		q := url.Values{"state": {state}}
		if outcome == OutcomeDeny {
			q.Set("error", "access_denied")
		} else {
			code := "fake-code-" + randHex(8)
			g.Challenge = r.PostForm.Get("code_challenge")
			f.mu.Lock()
			f.codes[code] = g
			f.mu.Unlock()
			q.Set("code", code)
		}
		http.Redirect(w, r, redirect+"?"+q.Encode(), http.StatusFound)
	case "steam":
		f.steamRedirect(w, r, g, redirect)
	default:
		http.Error(w, "unknown provider", http.StatusBadRequest)
	}
}

func (f *fake) putToken(token string, g grant) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.tokens[token] = g
}

func (f *fake) grantFor(token string) (grant, bool) {
	f.mu.Lock()
	defer f.mu.Unlock()
	g, ok := f.tokens[token]
	return g, ok
}

// apiGrant resolves a bearer token for a provider API call, and applies the
// scripted reject_token and down outcomes.
func (f *fake) apiGrant(w http.ResponseWriter, r *http.Request, provider string) (grant, bool) {
	token := strings.TrimPrefix(r.Header.Get("Authorization"), "Bearer ")
	g, ok := f.grantFor(token)
	switch {
	case !ok || g.Provider != provider:
		f.record(provider, "api", "unknown token")
		http.Error(w, `{"message": "401: Unauthorized"}`, http.StatusUnauthorized)
		return g, false
	case g.Outcome == OutcomeRejectToken:
		f.record(provider, "api", "token rejected (scripted)")
		http.Error(w, `{"message": "401: Unauthorized"}`, http.StatusUnauthorized)
		return g, false
	case g.Outcome == OutcomeDown:
		f.record(provider, "api", "provider down (scripted)")
		http.Error(w, "upstream unavailable", http.StatusInternalServerError)
		return g, false
	}
	f.record(provider, "api", "ok for "+g.Identity)
	return g, true
}

// ── Discord and Twitch APIs ──────────────────────────────────────────

func (f *fake) discordMe(w http.ResponseWriter, r *http.Request) {
	g, ok := f.apiGrant(w, r, "discord")
	if !ok {
		return
	}
	writeJSON(w, map[string]string{"id": stableID("discord", g.Identity), "username": g.Identity, "avatar": ""})
}

func (f *fake) twitchUsers(w http.ResponseWriter, r *http.Request) {
	g, ok := f.apiGrant(w, r, "twitch")
	if !ok {
		return
	}
	writeJSON(w, map[string]any{"data": []map[string]string{{
		"id":           stableID("twitch", g.Identity),
		"login":        strings.ToLower(g.Identity),
		"display_name": g.Identity,
		"email":        g.Identity + "@example.test",
	}}})
}

// ── Google ───────────────────────────────────────────────────────────

func (f *fake) googleToken(w http.ResponseWriter, r *http.Request) {
	if err := r.ParseForm(); err != nil {
		http.Error(w, err.Error(), http.StatusBadRequest)
		return
	}
	code := r.PostForm.Get("code")
	f.mu.Lock()
	g, ok := f.codes[code]
	delete(f.codes, code) // a code is single-use
	f.mu.Unlock()
	if !ok {
		f.record("google", "token", "unknown or reused code")
		w.WriteHeader(http.StatusBadRequest)
		writeJSON(w, map[string]string{"error": "invalid_grant"})
		return
	}
	if !pkceMatches(r.PostForm.Get("code_verifier"), g.Challenge) {
		f.record("google", "token", "PKCE verifier does not match the challenge")
		w.WriteHeader(http.StatusBadRequest)
		writeJSON(w, map[string]string{"error": "invalid_grant", "error_description": "code_verifier mismatch"})
		return
	}
	if r.PostForm.Get("redirect_uri") != g.Redirect {
		f.record("google", "token", "redirect_uri differs from the authorize request")
		w.WriteHeader(http.StatusBadRequest)
		writeJSON(w, map[string]string{"error": "invalid_grant", "error_description": "redirect_uri mismatch"})
		return
	}
	if g.Outcome == OutcomeDown {
		http.Error(w, "upstream unavailable", http.StatusInternalServerError)
		return
	}
	idToken, err := f.signIDToken(g)
	if err != nil {
		http.Error(w, err.Error(), http.StatusInternalServerError)
		return
	}
	f.record("google", "token", "issued id_token for "+g.Identity)
	writeJSON(w, map[string]any{
		"access_token": "fake-google-access-" + randHex(8),
		"id_token":     idToken,
		"expires_in":   3599,
		"token_type":   "Bearer",
		"scope":        "openid profile email",
	})
}

func pkceMatches(verifier, challenge string) bool {
	sum := sha256.Sum256([]byte(verifier))
	return verifier != "" && base64.RawURLEncoding.EncodeToString(sum[:]) == challenge
}

// signIDToken makes an RS256 JWT with the claims Nakama requires:
// iss accounts.google.com, sub, azp, aud, iat, exp.
func (f *fake) signIDToken(g grant) (string, error) {
	now := time.Now().Unix()
	sub := stableID("google", g.Identity)
	if g.Outcome == OutcomeRejectToken {
		sub = "" // Nakama refuses an id_token without sub
	}
	header := map[string]string{"alg": "RS256", "kid": f.googleKid, "typ": "JWT"}
	claims := map[string]any{
		"iss": "https://accounts.google.com", "sub": sub, "azp": g.ClientID, "aud": g.ClientID,
		"email": g.Identity + "@example.test", "email_verified": true, "name": g.Identity,
		"iat": now, "exp": now + 3600,
	}
	h, _ := json.Marshal(header)
	c, _ := json.Marshal(claims)
	signing := base64.RawURLEncoding.EncodeToString(h) + "." + base64.RawURLEncoding.EncodeToString(c)
	digest := sha256.Sum256([]byte(signing))
	sig, err := rsa.SignPKCS1v15(rand.Reader, f.googleKey, crypto.SHA256, digest[:])
	if err != nil {
		return "", err
	}
	return signing + "." + base64.RawURLEncoding.EncodeToString(sig), nil
}

func (f *fake) googleCerts(w http.ResponseWriter, _ *http.Request) {
	writeJSON(w, map[string]string{f.googleKid: string(f.googleCert)})
}

// ── Steam (OpenID 2.0) ───────────────────────────────────────────────

func (f *fake) steamLogin(w http.ResponseWriter, r *http.Request) {
	if r.Method == http.MethodPost {
		f.steamCheck(w, r)
		return
	}
	q := r.URL.Query()
	if q.Get("openid.mode") != "checkid_setup" {
		f.fail(w, "steam", "invalid_request", "openid.mode must be checkid_setup")
		return
	}
	returnTo := q.Get("openid.return_to")
	base := strings.SplitN(returnTo, "?", 2)[0]
	if base != f.redirectURI {
		f.fail(w, "steam", "invalid_return_to", fmt.Sprintf("got %q", returnTo))
		return
	}
	f.record("steam", "authorize", "return_to="+returnTo)
	f.showConsent(w, "steam", map[string]string{"provider": "steam", "client_id": "steam", "redirect_uri": returnTo})
}

func (f *fake) steamRedirect(w http.ResponseWriter, r *http.Request, g grant, returnTo string) {
	target, err := url.Parse(returnTo)
	if err != nil {
		http.Error(w, err.Error(), http.StatusBadRequest)
		return
	}
	q := target.Query() // keeps the app's own state from return_to
	if g.Outcome == OutcomeWrongState {
		q.Set("state", "not-"+q.Get("state"))
	}
	if g.Outcome == OutcomeDeny {
		q.Set("openid.mode", "cancel")
	} else {
		claimed := "https://steamcommunity.com/openid/id/" + steamID(g.Identity)
		f.putToken(claimed, g)
		for k, v := range map[string]string{
			"openid.ns":             "http://specs.openid.net/auth/2.0",
			"openid.mode":           "id_res",
			"openid.op_endpoint":    "https://steamcommunity.com/openid/login",
			"openid.claimed_id":     claimed,
			"openid.identity":       claimed,
			"openid.return_to":      returnTo,
			"openid.response_nonce": time.Now().UTC().Format("2006-01-02T15:04:05Z") + randHex(4),
			"openid.assoc_handle":   "1234567890",
			"openid.signed":         "signed,op_endpoint,claimed_id,identity,return_to,response_nonce,assoc_handle",
			"openid.sig":            "fakesig=",
		} {
			q.Set(k, v)
		}
	}
	target.RawQuery = q.Encode()
	http.Redirect(w, r, target.String(), http.StatusFound)
}

func (f *fake) steamCheck(w http.ResponseWriter, r *http.Request) {
	if err := r.ParseForm(); err != nil {
		http.Error(w, err.Error(), http.StatusBadRequest)
		return
	}
	g, ok := f.grantFor(r.PostForm.Get("openid.claimed_id"))
	valid := ok && r.PostForm.Get("openid.mode") == "check_authentication" && g.Outcome != OutcomeRejectToken
	if ok && g.Outcome == OutcomeDown {
		http.Error(w, "upstream unavailable", http.StatusInternalServerError)
		return
	}
	f.record("steam", "check_authentication", fmt.Sprintf("valid=%v", valid))
	fmt.Fprintf(w, "ns:http://specs.openid.net/auth/2.0\nis_valid:%v\n", valid)
}

func (f *fake) steamPersona(w http.ResponseWriter, r *http.Request) {
	id := r.URL.Query().Get("steamids")
	f.mu.Lock()
	g, ok := f.tokens["https://steamcommunity.com/openid/id/"+id]
	f.mu.Unlock()
	players := []map[string]string{}
	if ok {
		players = append(players, map[string]string{"steamid": id, "personaname": g.Identity})
	}
	writeJSON(w, map[string]any{"response": map[string]any{"players": players}})
}

// ── Helpers ──────────────────────────────────────────────────────────

func writeJSON(w http.ResponseWriter, v any) {
	w.Header().Set("Content-Type", "application/json")
	_ = json.NewEncoder(w).Encode(v)
}

func randHex(n int) string {
	b := make([]byte, n)
	_, _ = rand.Read(b)
	return hex.EncodeToString(b)
}

// stableID gives one identity the same provider user ID in every run, so a
// journey can sign in again as the same person.
func stableID(provider, identity string) string {
	sum := sha256.Sum256([]byte(provider + ":" + identity))
	return new(big.Int).SetBytes(sum[:8]).String()
}

func steamID(identity string) string {
	sum := sha256.Sum256([]byte("steam:" + identity))
	n := new(big.Int).SetBytes(sum[:4])
	return fmt.Sprintf("7656119%010d", n.Uint64()%10_000_000_000)
}
