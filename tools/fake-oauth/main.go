// fake-oauth stands in for Discord, Twitch, Steam and Google in e2e runs
// (plans/E2E-QA.md §8). Every mello step runs unmodified; only the provider
// is fake.
//
// It serves the providers' real URL paths, so the client needs only a new
// scheme and host (MELLO_E2E_OAUTH_BASE), and the backend reaches it through
// Docker network aliases on the real host names, over TLS with a test CA.
//
// Browser side (plain HTTP on -http, default :8080):
//
//	Discord  GET /api/oauth2/authorize   implicit: token in the fragment
//	Twitch   GET /oauth2/authorize       implicit: token in the fragment
//	Google   GET /o/oauth2/v2/auth       code + PKCE; POST /token exchanges it
//	Steam    GET /openid/login           OpenID 2.0 checkid_setup
//
// Backend side (the same handlers, also on -https, default :443):
//
//	GET  /api/users/@me                        Discord user
//	GET  /helix/users                          Twitch user
//	POST /openid/login                         Steam check_authentication
//	GET  /ISteamUser/GetPlayerSummaries/v2/    Steam persona
//	GET  /oauth2/v1/certs                      Google signing certificates
//
// Control (for the driver): POST /_control/next sets the outcome of the next
// consent; GET /_control/log lists what happened; GET /healthz.
package main

import (
	"flag"
	"log"
	"net/http"
	"os"
	"path/filepath"
)

func main() {
	httpAddr := flag.String("http", ":8080", "browser-facing HTTP address")
	httpsAddr := flag.String("https", "", "backend-facing HTTPS address, for example :443 (empty: off)")
	outDir := flag.String("out", "", "write ca.pem and bundle.pem (system roots + test CA) here")
	systemRoots := flag.String("system-roots", "/etc/ssl/certs/ca-certificates.crt", "system CA bundle to extend")
	redirectURI := flag.String("redirect-uri", "http://localhost:29405/callback", "the only redirect URI the fake accepts")
	flag.Parse()

	f, err := newFake(*redirectURI)
	if err != nil {
		log.Fatalf("fake-oauth: %v", err)
	}
	mux := f.routes()

	if *httpsAddr != "" {
		ca, err := newCA()
		if err != nil {
			log.Fatalf("fake-oauth: CA: %v", err)
		}
		if *outDir != "" {
			if err := writeTrust(ca, *outDir, *systemRoots); err != nil {
				log.Fatalf("fake-oauth: write trust: %v", err)
			}
			log.Printf("fake-oauth: wrote %s", filepath.Join(*outDir, "bundle.pem"))
		}
		tlsCfg, err := ca.serverTLS(providerHosts)
		if err != nil {
			log.Fatalf("fake-oauth: TLS: %v", err)
		}
		srv := &http.Server{Addr: *httpsAddr, Handler: mux, TLSConfig: tlsCfg}
		go func() {
			log.Printf("fake-oauth: HTTPS on %s for %v", *httpsAddr, providerHosts)
			if err := srv.ListenAndServeTLS("", ""); err != nil {
				log.Fatalf("fake-oauth: HTTPS: %v", err)
			}
		}()
	}

	log.Printf("fake-oauth: HTTP on %s, redirect URI %s", *httpAddr, *redirectURI)
	if err := http.ListenAndServe(*httpAddr, mux); err != nil {
		log.Fatalf("fake-oauth: HTTP: %v", err)
	}
	os.Exit(1)
}

// providerHosts are the names the backend calls. Docker network aliases point
// them at this container; the leaf certificate covers all of them.
var providerHosts = []string{
	"discord.com",
	"api.twitch.tv",
	"id.twitch.tv",
	"steamcommunity.com",
	"api.steampowered.com",
	"www.googleapis.com",
	"oauth2.googleapis.com",
	"accounts.google.com",
	"fake-oauth",
	"localhost",
}
