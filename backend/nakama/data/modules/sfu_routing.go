package main

import (
	"context"
	"database/sql"
	"encoding/json"
	"net"
	"net/url"
	"os"
	"sort"
	"strings"

	"github.com/heroiclabs/nakama-common/runtime"
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

// sfuHealthPort is the port of the SFU's health and admin server
// (mello-sfu SFU_ADMIN_ADDR, ":8080" in every deployment). The signaling
// server on the endpoint's port does not serve /health.
const sfuHealthPort = "8080"

// sfuHealthURL derives the public health URL of the SFU that serves the
// signaling endpoint: the same host, the health port, and https for wss
// (http for ws). It returns "" for an endpoint it cannot parse.
func sfuHealthURL(endpoint string) string {
	u, err := url.Parse(endpoint)
	if err != nil || u.Hostname() == "" {
		return ""
	}
	scheme := ""
	switch u.Scheme {
	case "wss":
		scheme = "https"
	case "ws":
		scheme = "http"
	default:
		return ""
	}
	health := url.URL{
		Scheme: scheme,
		Host:   net.JoinHostPort(u.Hostname(), sfuHealthPort),
		Path:   "/health",
	}
	return health.String()
}

// SFURegion is one configured SFU region and the URL the client measures
// its round trip against.
type SFURegion struct {
	Region    string `json:"region"`
	HealthURL string `json:"health_url"`
}

// GetSFURegionsResponse is the get_sfu_regions reply.
type GetSFURegionsResponse struct {
	Regions []SFURegion `json:"regions"`
}

// sfuRegions lists the configured regions, sorted by name. A region whose
// endpoint gives no health URL is left out: the client cannot measure it.
func sfuRegions() []SFURegion {
	regions := make([]SFURegion, 0, len(sfuEndpoints))
	for region, endpoint := range sfuEndpoints {
		health := sfuHealthURL(endpoint)
		if region == "" || health == "" {
			continue
		}
		regions = append(regions, SFURegion{Region: region, HealthURL: health})
	}
	sort.Slice(regions, func(i, j int) bool { return regions[i].Region < regions[j].Region })
	return regions
}

// GetSFURegionsRPC returns the SFU regions and their health URLs. The client
// measures the round trip to each and sends the fastest as preferred_region
// in voice_join and start_stream (plans/voice-quality.md stage 6).
func GetSFURegionsRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	if _, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string); !ok {
		return "", runtime.NewError("authentication required", 16)
	}
	resp, err := json.Marshal(GetSFURegionsResponse{Regions: sfuRegions()})
	if err != nil {
		logger.Error("get_sfu_regions: marshal: %v", err)
		return "", runtime.NewError("internal error", 13)
	}
	return string(resp), nil
}
