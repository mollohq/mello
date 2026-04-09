package main

import (
	"context"
	"database/sql"
	"fmt"
	"time"

	"github.com/heroiclabs/nakama-common/runtime"
)

// Global references so session event handlers (which don't receive nk/db) can
// access them. Set once during InitModule, read-only afterwards.
var (
	globalNk     runtime.NakamaModule
	globalLogger runtime.Logger
	globalDb     *sql.DB
)

func InitModule(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, initializer runtime.Initializer) error {
	logger.Info("Mello backend initializing...")

	// Store globals for session event handlers
	globalNk = nk
	globalLogger = logger
	globalDb = db

	// SFU auth init (non-fatal — SFU features are just unavailable if not configured)
	if err := initSFUAuth(); err != nil {
		logger.Error("SFU auth init failed: %v", err)
	} else if sfuAuthEnabled() {
		logger.Info("SFU JWT signing enabled")
	} else {
		logger.Warn("SFU JWT signing disabled (SFU_JWT_PRIVATE_KEY not set)")
	}

	// -----------------------------------------------------------------------
	// Auth hooks
	// -----------------------------------------------------------------------
	if err := initializer.RegisterAfterAuthenticateEmail(AfterAuthenticateEmail); err != nil {
		return err
	}
	if err := initializer.RegisterBeforeAuthenticateCustom(BeforeAuthenticateCustom); err != nil {
		return err
	}
	if err := initializer.RegisterBeforeLinkCustom(BeforeLinkCustom); err != nil {
		return err
	}

	// -----------------------------------------------------------------------
	// Session lifecycle hooks
	// -----------------------------------------------------------------------
	if err := initializer.RegisterEventSessionStart(OnSessionStart); err != nil {
		return err
	}
	if err := initializer.RegisterEventSessionEnd(OnSessionEnd); err != nil {
		return err
	}

	// -----------------------------------------------------------------------
	// Group (crew) hooks
	// -----------------------------------------------------------------------
	if err := initializer.RegisterAfterJoinGroup(AfterJoinCrew); err != nil {
		return err
	}
	if err := initializer.RegisterAfterLeaveGroup(AfterLeaveCrew); err != nil {
		return err
	}

	// -----------------------------------------------------------------------
	// Chat hooks
	// -----------------------------------------------------------------------
	if err := initializer.RegisterBeforeRt("ChannelMessageSend", BeforeChannelMessageSendHook); err != nil {
		return err
	}
	if err := initializer.RegisterAfterRt("ChannelMessageSend", OnChatMessage); err != nil {
		return err
	}

	// -----------------------------------------------------------------------
	// RPCs — auth
	// -----------------------------------------------------------------------
	if err := initializer.RegisterRpc("auth/providers", AuthProvidersRPC); err != nil {
		return err
	}

	// -----------------------------------------------------------------------
	// RPCs — health
	// -----------------------------------------------------------------------
	if err := initializer.RegisterRpc("health", HealthCheckRPC); err != nil {
		return err
	}

	// -----------------------------------------------------------------------
	// RPCs — presence
	// -----------------------------------------------------------------------
	if err := initializer.RegisterRpc("presence_update", PresenceUpdateRPC); err != nil {
		return err
	}
	if err := initializer.RegisterRpc("presence_get", PresenceGetRPC); err != nil {
		return err
	}

	// -----------------------------------------------------------------------
	// RPCs — crew state
	// -----------------------------------------------------------------------
	if err := initializer.RegisterRpc("crew_state_get", CrewStateGetRPC); err != nil {
		return err
	}
	if err := initializer.RegisterRpc("crew_state_get_sidebar", CrewStateGetSidebarRPC); err != nil {
		return err
	}

	// -----------------------------------------------------------------------
	// RPCs — subscriptions (push)
	// -----------------------------------------------------------------------
	if err := initializer.RegisterRpc("set_active_crew", SetActiveCrewRPC); err != nil {
		return err
	}
	if err := initializer.RegisterRpc("subscribe_sidebar", SubscribeSidebarRPC); err != nil {
		return err
	}

	// -----------------------------------------------------------------------
	// RPCs — crews
	// -----------------------------------------------------------------------
	if err := initializer.RegisterRpc("discover_crews", DiscoverCrewsRPC); err != nil {
		return err
	}
	if err := initializer.RegisterRpc("create_crew", CreateCrewRPC); err != nil {
		return err
	}
	if err := initializer.RegisterRpc("search_users", SearchUsersRPC); err != nil {
		return err
	}
	if err := initializer.RegisterRpc("join_by_invite_code", JoinByInviteCodeRPC); err != nil {
		return err
	}
	if err := initializer.RegisterRpc("get_crew_avatar", GetCrewAvatarRPC); err != nil {
		return err
	}

	// -----------------------------------------------------------------------
	// RPCs — voice
	// -----------------------------------------------------------------------
	if err := initializer.RegisterRpc("voice_join", VoiceJoinRPC); err != nil {
		return err
	}
	if err := initializer.RegisterRpc("voice_leave", VoiceLeaveRPC); err != nil {
		return err
	}
	if err := initializer.RegisterRpc("voice_speaking", VoiceSpeakingRPC); err != nil {
		return err
	}

	// -----------------------------------------------------------------------
	// RPCs — voice channels
	// -----------------------------------------------------------------------
	if err := initializer.RegisterRpc("channel_create", ChannelCreateRPC); err != nil {
		return err
	}
	if err := initializer.RegisterRpc("channel_rename", ChannelRenameRPC); err != nil {
		return err
	}
	if err := initializer.RegisterRpc("channel_delete", ChannelDeleteRPC); err != nil {
		return err
	}
	if err := initializer.RegisterRpc("channel_reorder", ChannelReorderRPC); err != nil {
		return err
	}

	// -----------------------------------------------------------------------
	// RPCs — streaming
	// -----------------------------------------------------------------------
	if err := initializer.RegisterRpc("get_ice_servers", GetIceServersRPC); err != nil {
		return err
	}
	if err := initializer.RegisterRpc("start_stream", StartStreamRPC); err != nil {
		return err
	}
	if err := initializer.RegisterRpc("stop_stream", StopStreamRPC); err != nil {
		return err
	}
	if err := initializer.RegisterRpc("stream_thumbnail_upload", StreamThumbnailUploadRPC); err != nil {
		return err
	}
	if err := initializer.RegisterRpc("watch_stream", WatchStreamRPC); err != nil {
		return err
	}
	if err := initializer.RegisterRpc("update_stream_resolution", UpdateStreamResolutionRPC); err != nil {
		return err
	}

	// -----------------------------------------------------------------------
	// RPCs — crew events (event ledger, catch-up, moments)
	// -----------------------------------------------------------------------
	if err := initializer.RegisterRpc("crew_catchup", CrewCatchupRPC); err != nil {
		return err
	}
	if err := initializer.RegisterRpc("post_moment", PostMomentRPC); err != nil {
		return err
	}
	if err := initializer.RegisterRpc("game_session_end", GameSessionEndRPC); err != nil {
		return err
	}

	// -----------------------------------------------------------------------
	// RPCs — clips
	// -----------------------------------------------------------------------
	if err := initializer.RegisterRpc("post_clip", PostClipRPC); err != nil {
		return err
	}
	if err := initializer.RegisterRpc("crew_timeline", CrewTimelineRPC); err != nil {
		return err
	}
	if err := initializer.RegisterRpc("clip_upload_url", ClipUploadURLRPC); err != nil {
		return err
	}

	// -----------------------------------------------------------------------
	// RPCs — dev tools
	// -----------------------------------------------------------------------
	if err := initializer.RegisterRpc("dev_seed_state", DevSeedStateRPC); err != nil {
		return err
	}

	// -----------------------------------------------------------------------
	// Background goroutines
	// -----------------------------------------------------------------------
	go StartSidebarBatchLoop(nk, logger, 30*time.Second)
	go StartMessageThrottleLoop(nk, logger, 10*time.Second)
	go startChatActivityTicker(ctx, nk, logger, 30*time.Minute)
	go StartVoiceRoomGC(ctx, nk, logger, 30*time.Second)
	go StartStreamGC(ctx, nk, logger, 60*time.Second)
	go StartWeeklyRecapJob(ctx, nk, logger)

	logger.Info("Mello backend initialized successfully")
	return nil
}

const (
	ProtocolVersion   = 1
	MinClientProtocol = 1
)

func HealthCheckRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	if err := db.PingContext(ctx); err != nil {
		return "", runtime.NewError("database unhealthy", 13)
	}
	return fmt.Sprintf(
		`{"status":"healthy","version":"0.3.0","protocol_version":%d,"min_client_protocol":%d}`,
		ProtocolVersion, MinClientProtocol,
	), nil
}
