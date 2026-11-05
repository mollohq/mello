package main

import (
	"context"
	"database/sql"
	"encoding/json"
	"fmt"
	"net"
	"net/url"
	"os"
	"sort"
	"strings"

	"github.com/heroiclabs/nakama-common/runtime"
)

// sfuEndpoints maps each configured SFU region to its signaling endpoint.
// It has no built-in entries: a deployment names its own SFUs (SFU_REGIONS),
// and a backend with none configured has no voice or streams. The official
// network sets its regions on Render, like any self-hoster.
var sfuEndpoints = map[string]string{}

// sfuRegionOrder keeps the configured order. The first region is the default
// when SFU_DEFAULT_REGION names none.
var sfuRegionOrder []string

// sfuRegionProblems lists the SFU_REGIONS entries that were left out, for the
// start log (InitModule).
var sfuRegionProblems []string

func init() {
	loadSFURegions(os.Getenv)
}

// loadSFURegions reads the SFU regions from the environment.
//
//	SFU_REGIONS="eu-west=wss://sfu-eu.example.com:8443/ws,us-east=wss://sfu-us.example.com:8443/ws"
//
// Without SFU_REGIONS it reads the older SFU_ENDPOINT_EU (region eu-west) and
// SFU_ENDPOINT_US (region us-east). A region name must match the SFU_REGION of
// the SFU at that endpoint: the token carries it and the SFU checks it.
func loadSFURegions(getenv func(string) string) {
	sfuEndpoints = map[string]string{}
	sfuRegionOrder = nil
	sfuRegionProblems = nil

	add := func(region, endpoint, source string) {
		region, endpoint = strings.TrimSpace(region), strings.TrimSpace(endpoint)
		switch {
		case region == "":
			sfuRegionProblems = append(sfuRegionProblems, fmt.Sprintf("%s: an entry has no region name", source))
		case !validSFUEndpoint(endpoint):
			sfuRegionProblems = append(sfuRegionProblems, fmt.Sprintf("%s: region %q: %q is not a ws:// or wss:// URL with a host", source, region, endpoint))
		default:
			if _, dup := sfuEndpoints[region]; dup {
				sfuRegionProblems = append(sfuRegionProblems, fmt.Sprintf("%s: region %q is listed twice; the first entry is used", source, region))
				return
			}
			sfuEndpoints[region] = endpoint
			sfuRegionOrder = append(sfuRegionOrder, region)
		}
	}

	if list := strings.TrimSpace(getenv("SFU_REGIONS")); list != "" {
		for _, entry := range strings.Split(list, ",") {
			if strings.TrimSpace(entry) == "" {
				continue
			}
			region, endpoint, ok := strings.Cut(entry, "=")
			if !ok {
				sfuRegionProblems = append(sfuRegionProblems, fmt.Sprintf("SFU_REGIONS: %q is not region=endpoint", strings.TrimSpace(entry)))
				continue
			}
			add(region, endpoint, "SFU_REGIONS")
		}
		return
	}
	if eu := strings.TrimSpace(getenv("SFU_ENDPOINT_EU")); eu != "" {
		add("eu-west", eu, "SFU_ENDPOINT_EU")
	}
	if us := strings.TrimSpace(getenv("SFU_ENDPOINT_US")); us != "" {
		add("us-east", us, "SFU_ENDPOINT_US")
	}
}

// validSFUEndpoint reports whether endpoint is a ws:// or wss:// URL with a
// host.
func validSFUEndpoint(endpoint string) bool {
	u, err := url.Parse(endpoint)
	return err == nil && (u.Scheme == "ws" || u.Scheme == "wss") && u.Hostname() != ""
}

// sfuRegionsConfigured reports whether at least one SFU region is configured.
func sfuRegionsConfigured() bool {
	return len(sfuRegionOrder) > 0
}

// sfuAvailable reports whether the backend can hand out SFU sessions: it can
// sign tokens and has at least one SFU to send them to.
func sfuAvailable() bool {
	return sfuAuthEnabled() && sfuRegionsConfigured()
}

// isSFURegion reports whether region names a configured SFU endpoint.
func isSFURegion(region string) bool {
	_, ok := sfuEndpoints[region]
	return ok && region != ""
}

// defaultSFURegion is SFU_DEFAULT_REGION when it names a configured region,
// else the first configured region, else "". Read on every call, so a
// deployment can change it without a code change.
func defaultSFURegion() string {
	if r := strings.TrimSpace(os.Getenv("SFU_DEFAULT_REGION")); isSFURegion(r) {
		return r
	}
	if len(sfuRegionOrder) > 0 {
		return sfuRegionOrder[0]
	}
	return ""
}

// selectSFURegion picks the SFU region for a new session. The client's
// preferred_region wins when it names a configured region (the client
// measures each region's /health, spec 10 section 7.6). An empty or unknown
// value falls back to defaultSFURegion(): a stale region list on the client
// must not fail a join.
func selectSFURegion(preferred string) string {
	if isSFURegion(preferred) {
		return preferred
	}
	return defaultSFURegion()
}

// sfuEndpointForRegion is the endpoint of region, else of the default region
// (a session stored with a region that is no longer configured), else "".
func sfuEndpointForRegion(region string) string {
	if ep, ok := sfuEndpoints[region]; ok {
		return ep
	}
	return sfuEndpoints[defaultSFURegion()]
}

// defaultSFUHealthPort is the port of the SFU's health and admin server
// (mello-sfu SFU_ADMIN_ADDR, ":8080" by default). The signaling server on the
// endpoint's port does not serve /health.
const defaultSFUHealthPort = "8080"

// sfuHealthPort is SFU_HEALTH_PORT, else 8080.
func sfuHealthPort() string {
	if p := strings.TrimSpace(os.Getenv("SFU_HEALTH_PORT")); p != "" {
		return p
	}
	return defaultSFUHealthPort
}

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
		Host:   net.JoinHostPort(u.Hostname(), sfuHealthPort()),
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
	regions := make([]SFURegion, 0, len(sfuRegionOrder))
	for _, region := range sfuRegionOrder {
		health := sfuHealthURL(sfuEndpoints[region])
		if health == "" {
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
