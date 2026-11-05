package main

import (
	"context"
	"database/sql"
	"encoding/json"
	"fmt"
	"strings"
	"time"

	"github.com/heroiclabs/nakama-common/runtime"
)

// The feed config: settings of the crew feed that an admin changes without a
// release. Today one: the free window of the zone feed, in days. Everything
// older than the window is m3llo+ (Crew history).
//
// The admin RPCs are server-to-server only: the mello-admin Worker calls them
// with the http_key and writes the audit log. A client session is refused.

const (
	FeedConfigCollection = "feed_config"
	FeedConfigKey        = "config"

	feedFreeDaysDefault = 7
	feedFreeDaysMin     = 1
	feedFreeDaysMax     = 90
	// feedConfigMaxActor bounds the admin name stamped into the config.
	feedConfigMaxActor = 64
)

// FeedConfig is the stored feed config.
type FeedConfig struct {
	// FreeDays is the free window of the zone feed. Clips, sessions and
	// streams younger than this are free; older ones are Crew history.
	FreeDays int `json:"free_days"`
	// ConfigVersion is "<time>:admin:<actor>" of the last admin write.
	ConfigVersion string `json:"config_version,omitempty"`
}

func defaultFeedConfig() FeedConfig {
	return FeedConfig{FreeDays: feedFreeDaysDefault}
}

// parseFeedConfig reads a stored value. A value out of range falls back to
// the default, so a bad write can never close or open the whole history.
func parseFeedConfig(raw string) FeedConfig {
	var c FeedConfig
	if err := json.Unmarshal([]byte(raw), &c); err != nil {
		return defaultFeedConfig()
	}
	if c.FreeDays < feedFreeDaysMin || c.FreeDays > feedFreeDaysMax {
		c.FreeDays = feedFreeDaysDefault
	}
	return c
}

// AdminFeedConfigRead is the admin_feed_config_get response.
type AdminFeedConfigRead struct {
	Config FeedConfig `json:"config"`
	// Version is the storage version for a conditional write. Empty when no
	// config is stored and the default is in use.
	Version   string `json:"version"`
	Stored    bool   `json:"stored"`
	UpdatedAt string `json:"updated_at"`
	// Min and Max bound free_days, for the admin form.
	Min int `json:"min"`
	Max int `json:"max"`
}

// AdminFeedConfigWrite is the admin_feed_config_set request.
type AdminFeedConfigWrite struct {
	FreeDays int `json:"free_days"`
	// Version is the storage version the admin read. Empty means the admin
	// read the default, so the write succeeds only if nothing is stored.
	Version string `json:"version"`
	// Actor is the admin console user. It is stamped into config_version.
	Actor string `json:"actor"`
}

// FeedConfigDiff tells the audit log what a write changed.
type FeedConfigDiff struct {
	FreeDaysFrom int `json:"free_days_from"`
	FreeDaysTo   int `json:"free_days_to"`
}

// AdminFeedConfigSetResponse is the admin_feed_config_set response.
type AdminFeedConfigSetResponse struct {
	Config  FeedConfig     `json:"config"`
	Version string         `json:"version"`
	Diff    FeedConfigDiff `json:"diff"`
}

// readStoredFeedConfig returns the stored config, or the default with
// stored=false when nothing is stored.
func readStoredFeedConfig(ctx context.Context, nk runtime.NakamaModule) (AdminFeedConfigRead, error) {
	read := AdminFeedConfigRead{Config: defaultFeedConfig(), Min: feedFreeDaysMin, Max: feedFreeDaysMax}
	objects, err := nk.StorageRead(ctx, []*runtime.StorageRead{
		{Collection: FeedConfigCollection, Key: FeedConfigKey, UserID: SystemUserID},
	})
	if err != nil {
		return read, err
	}
	if len(objects) == 0 {
		return read, nil
	}
	obj := objects[0]
	read.Config = parseFeedConfig(obj.GetValue())
	read.Version = obj.GetVersion()
	read.Stored = true
	if ts := obj.GetUpdateTime(); ts != nil {
		read.UpdatedAt = ts.AsTime().UTC().Format(time.RFC3339)
	}
	return read, nil
}

// readFeedConfig is the config the feed uses. A read failure uses the default:
// the feed never fails on its config.
func readFeedConfig(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule) FeedConfig {
	read, err := readStoredFeedConfig(ctx, nk)
	if err != nil {
		logger.Warn("feed config: read failed, using the default: %v", err)
		return defaultFeedConfig()
	}
	return read.Config
}

// normalizeFeedConfigWrite validates a write and returns the actor.
func normalizeFeedConfigWrite(w AdminFeedConfigWrite) (string, error) {
	actor := strings.TrimSpace(w.Actor)
	if actor == "" {
		return "", fmt.Errorf("actor is required")
	}
	if len(actor) > feedConfigMaxActor {
		actor = actor[:feedConfigMaxActor]
	}
	if w.FreeDays < feedFreeDaysMin || w.FreeDays > feedFreeDaysMax {
		return "", fmt.Errorf("free_days must be %d to %d", feedFreeDaysMin, feedFreeDaysMax)
	}
	return actor, nil
}

// AdminFeedConfigGetRPC returns the stored feed config and its storage version.
func AdminFeedConfigGetRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	if err := requireServerCaller(ctx, "admin_feed_config_get"); err != nil {
		return "", err
	}
	read, err := readStoredFeedConfig(ctx, nk)
	if err != nil {
		logger.Error("admin_feed_config_get: storage read failed: %v", err)
		return "", runtime.NewError("config read failed", 13) // INTERNAL
	}
	out, err := json.Marshal(read)
	if err != nil {
		return "", runtime.NewError("encode failed", 13)
	}
	return string(out), nil
}

// AdminFeedConfigSetRPC replaces the feed config. The write is conditional on
// the storage version the admin read, so two admins cannot overwrite each
// other without seeing it.
func AdminFeedConfigSetRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	if err := requireServerCaller(ctx, "admin_feed_config_set"); err != nil {
		return "", err
	}
	var w AdminFeedConfigWrite
	if err := json.Unmarshal([]byte(payload), &w); err != nil {
		return "", runtime.NewError("invalid request: "+err.Error(), 3) // INVALID_ARGUMENT
	}
	actor, err := normalizeFeedConfigWrite(w)
	if err != nil {
		return "", runtime.NewError(err.Error(), 3)
	}
	current, err := readStoredFeedConfig(ctx, nk)
	if err != nil {
		logger.Error("admin_feed_config_set: storage read failed: %v", err)
		return "", runtime.NewError("config read failed", 13)
	}
	if current.Version != w.Version {
		return "", runtime.NewError("config changed since read", 10) // ABORTED
	}
	next := FeedConfig{
		FreeDays:      w.FreeDays,
		ConfigVersion: time.Now().UTC().Format("2006-01-02T15:04:05Z") + ":admin:" + actor,
	}
	data, err := json.Marshal(next)
	if err != nil {
		return "", runtime.NewError("encode failed", 13)
	}
	// "*" creates only: a config stored by someone else since the read fails.
	writeVersion := current.Version
	if writeVersion == "" {
		writeVersion = "*"
	}
	acks, err := nk.StorageWrite(ctx, []*runtime.StorageWrite{
		{
			Collection:      FeedConfigCollection,
			Key:             FeedConfigKey,
			UserID:          SystemUserID,
			Value:           string(data),
			Version:         writeVersion,
			PermissionRead:  0, // server only
			PermissionWrite: 0, // server only
		},
	})
	if err != nil {
		if again, rerr := readStoredFeedConfig(ctx, nk); rerr == nil && again.Version != current.Version {
			return "", runtime.NewError("config changed since read", 10)
		}
		logger.Error("admin_feed_config_set: storage write failed: %v", err)
		return "", runtime.NewError("config write failed", 13)
	}
	logger.Info("feed config: free_days %d -> %d by %s", current.Config.FreeDays, next.FreeDays, actor)
	resp := AdminFeedConfigSetResponse{
		Config: next,
		Diff:   FeedConfigDiff{FreeDaysFrom: current.Config.FreeDays, FreeDaysTo: next.FreeDays},
	}
	if len(acks) > 0 {
		resp.Version = acks[0].GetVersion()
	}
	out, err := json.Marshal(resp)
	if err != nil {
		return "", runtime.NewError("encode failed", 13)
	}
	return string(out), nil
}
