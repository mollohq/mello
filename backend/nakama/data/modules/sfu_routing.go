package main

import (
	"os"
	"strings"
)

var sfuEndpoints = map[string]string{
	"eu-west": "wss://sfu-eu.m3llo.app/ws",
	"us-east": "wss://sfu-us.m3llo.app/ws",
}

// fallbackSFURegion is the region when neither the client nor
// SFU_DEFAULT_REGION names a configured one.
const fallbackSFURegion = "eu-west"

func init() {
	if eu := os.Getenv("SFU_ENDPOINT_EU"); eu != "" {
		sfuEndpoints["eu-west"] = eu
	}
	if us := os.Getenv("SFU_ENDPOINT_US"); us != "" {
		sfuEndpoints["us-east"] = us
	}
}

// isSFURegion reports whether region names a configured SFU endpoint.
func isSFURegion(region string) bool {
	_, ok := sfuEndpoints[region]
	return ok && region != ""
}

// defaultSFURegion is SFU_DEFAULT_REGION when it names a configured endpoint,
// else eu-west. Read on every call, so a deployment can change it without a
// code change.
func defaultSFURegion() string {
	if r := strings.TrimSpace(os.Getenv("SFU_DEFAULT_REGION")); isSFURegion(r) {
		return r
	}
	return fallbackSFURegion
}

// selectSFURegion picks the SFU region for a new session. The client's
// preferred_region wins when it names a configured endpoint (stage 6 of
// plans/voice-quality.md adds the client ping that chooses it). An empty or
// unknown value falls back to defaultSFURegion(): a stale region list on the
// client must not fail a join.
func selectSFURegion(preferred string) string {
	if isSFURegion(preferred) {
		return preferred
	}
	return defaultSFURegion()
}

func sfuEndpointForRegion(region string) string {
	if ep, ok := sfuEndpoints[region]; ok {
		return ep
	}
	return sfuEndpoints[fallbackSFURegion]
}
