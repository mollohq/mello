package main

import (
	"context"
	"database/sql"
	"fmt"
	"math/rand"
	"time"

	"github.com/heroiclabs/nakama-common/api"
	"github.com/heroiclabs/nakama-common/runtime"
)

func AfterAuthenticateEmail(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, out *api.Session, in *api.AuthenticateEmailRequest) error {
	userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
	if !ok {
		return nil
	}

	account, err := nk.AccountGetId(ctx, userID)
	if err != nil {
		logger.Error("failed to get account: %v", err)
		return nil
	}

	user := account.GetUser()
	metadata := user.GetMetadata()
	if metadata != "" && metadata != "{}" {
		return nil
	}

	rng := rand.New(rand.NewSource(time.Now().UnixNano()))
	tag := fmt.Sprintf("#%04d", rng.Intn(10000))

	displayName := user.GetDisplayName()
	if displayName == "" && in.GetAccount() != nil {
		email := in.GetAccount().GetEmail()
		for i, c := range email {
			if c == '@' {
				displayName = email[:i]
				break
			}
		}
	}

	meta := map[string]interface{}{
		"tag":        tag,
		"created_at": time.Now().Unix(),
	}

	if err := nk.AccountUpdateId(ctx, userID, "", meta, displayName, "", "", "", ""); err != nil {
		logger.Error("failed to initialize user metadata: %v", err)
		return nil
	}

	logger.Info("Initialized new user %s with tag %s", userID, tag)
	return nil
}
