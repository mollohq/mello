package main

import (
	"context"
	"database/sql"

	"github.com/heroiclabs/nakama-common/runtime"
)

func InitModule(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, initializer runtime.Initializer) error {
	logger.Info("Mello backend initializing...")

	// Register RPC functions
	if err := initializer.RegisterRpc("health", HealthCheckRPC); err != nil {
		return err
	}

	logger.Info("Mello backend initialized successfully")
	return nil
}

func HealthCheckRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	return `{"status": "ok"}`, nil
}
