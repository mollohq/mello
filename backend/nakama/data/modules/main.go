package main

import (
	"context"
	"database/sql"

	"github.com/heroiclabs/nakama-common/runtime"
)

func InitModule(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, initializer runtime.Initializer) error {
	logger.Info("Mello backend initializing...")

	// Auth hooks
	if err := initializer.RegisterAfterAuthenticateEmail(AfterAuthenticateEmail); err != nil {
		return err
	}

	// Group (crew) hooks
	if err := initializer.RegisterAfterJoinGroup(AfterJoinCrew); err != nil {
		return err
	}
	if err := initializer.RegisterAfterLeaveGroup(AfterLeaveCrew); err != nil {
		return err
	}

	// RPCs
	if err := initializer.RegisterRpc("health", HealthCheckRPC); err != nil {
		return err
	}
	if err := initializer.RegisterRpc("create_crew", CreateCrewRPC); err != nil {
		return err
	}
	if err := initializer.RegisterRpc("get_ice_servers", GetIceServersRPC); err != nil {
		return err
	}
	if err := initializer.RegisterRpc("start_stream", StartStreamRPC); err != nil {
		return err
	}
	if err := initializer.RegisterRpc("stop_stream", StopStreamRPC); err != nil {
		return err
	}

	logger.Info("Mello backend initialized successfully")
	return nil
}

func HealthCheckRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	if err := db.PingContext(ctx); err != nil {
		return "", runtime.NewError("database unhealthy", 13)
	}
	return `{"status":"healthy","version":"0.2.0"}`, nil
}
