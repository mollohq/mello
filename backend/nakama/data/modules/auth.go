package main

import (
	"context"
	"database/sql"
	"encoding/json"
	"fmt"
	"io"
	"math/rand"
	"net/http"
	"os"
	"time"

	"github.com/heroiclabs/nakama-common/api"
	"github.com/heroiclabs/nakama-common/runtime"
)

// ---------------------------------------------------------------------------
// Provider user types (for custom auth validation)
// ---------------------------------------------------------------------------

type DiscordUser struct {
	ID       string `json:"id"`
	Username string `json:"username"`
	Avatar   string `json:"avatar"`
}

type TwitchUser struct {
	ID          string `json:"id"`
	Login       string `json:"login"`
	DisplayName string `json:"display_name"`
	Email       string `json:"email"`
}

// ---------------------------------------------------------------------------
// BeforeAuthenticateCustom — dispatches on provider var to validate tokens
// ---------------------------------------------------------------------------

func BeforeAuthenticateCustom(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, in *api.AuthenticateCustomRequest) (*api.AuthenticateCustomRequest, error) {
	if in.Account.Vars == nil {
		return in, nil
	}

	provider := in.Account.Vars["provider"]
	token := in.Account.Id // token is passed as the custom ID

	switch provider {
	case "discord":
		return handleDiscordAuth(logger, in, token)
	case "twitch":
		return handleTwitchAuth(logger, in, token)
	default:
		return in, nil
	}
}

func handleDiscordAuth(logger runtime.Logger, in *api.AuthenticateCustomRequest, token string) (*api.AuthenticateCustomRequest, error) {
	user, err := validateDiscordToken(token)
	if err != nil {
		logger.Error("Discord validation failed: %v", err)
		return nil, runtime.NewError("Invalid Discord token", 16)
	}

	in.Account.Id = fmt.Sprintf("discord_%s", user.ID)
	if in.Username == "" {
		in.Username = user.Username
	}

	logger.Info("Discord auth for user: %s (%s)", user.Username, user.ID)
	return in, nil
}

func handleTwitchAuth(logger runtime.Logger, in *api.AuthenticateCustomRequest, token string) (*api.AuthenticateCustomRequest, error) {
	user, err := validateTwitchToken(token)
	if err != nil {
		logger.Error("Twitch validation failed: %v", err)
		return nil, runtime.NewError("Invalid Twitch token", 16)
	}

	in.Account.Id = fmt.Sprintf("twitch_%s", user.ID)
	if in.Username == "" {
		in.Username = user.Login
	}

	logger.Info("Twitch auth for user: %s (%s)", user.DisplayName, user.ID)
	return in, nil
}

// ---------------------------------------------------------------------------
// BeforeLinkCustom — same validation as BeforeAuthenticateCustom so that
// linking a Discord/Twitch identity during onboarding works correctly.
// ---------------------------------------------------------------------------

func BeforeLinkCustom(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, in *api.AccountCustom) (*api.AccountCustom, error) {
	if in.Vars == nil {
		return in, nil
	}

	provider := in.Vars["provider"]
	token := in.Id

	switch provider {
	case "discord":
		user, err := validateDiscordToken(token)
		if err != nil {
			logger.Error("Discord validation failed (link): %v", err)
			return nil, runtime.NewError("Invalid Discord token", 16)
		}
		in.Id = fmt.Sprintf("discord_%s", user.ID)
		logger.Info("Discord link for user: %s (%s)", user.Username, user.ID)
	case "twitch":
		user, err := validateTwitchToken(token)
		if err != nil {
			logger.Error("Twitch validation failed (link): %v", err)
			return nil, runtime.NewError("Invalid Twitch token", 16)
		}
		in.Id = fmt.Sprintf("twitch_%s", user.ID)
		logger.Info("Twitch link for user: %s (%s)", user.DisplayName, user.ID)
	}

	return in, nil
}

// ---------------------------------------------------------------------------
// Token validation helpers
// ---------------------------------------------------------------------------

func validateDiscordToken(token string) (*DiscordUser, error) {
	req, err := http.NewRequest("GET", "https://discord.com/api/users/@me", nil)
	if err != nil {
		return nil, err
	}
	req.Header.Set("Authorization", "Bearer "+token)

	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()

	if resp.StatusCode != 200 {
		body, _ := io.ReadAll(resp.Body)
		return nil, fmt.Errorf("discord API error: %s", body)
	}

	var user DiscordUser
	if err := json.NewDecoder(resp.Body).Decode(&user); err != nil {
		return nil, err
	}
	return &user, nil
}

func validateTwitchToken(token string) (*TwitchUser, error) {
	req, err := http.NewRequest("GET", "https://api.twitch.tv/helix/users", nil)
	if err != nil {
		return nil, err
	}
	req.Header.Set("Authorization", "Bearer "+token)
	req.Header.Set("Client-Id", os.Getenv("TWITCH_CLIENT_ID"))

	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()

	if resp.StatusCode != 200 {
		body, _ := io.ReadAll(resp.Body)
		return nil, fmt.Errorf("twitch API error: %s", body)
	}

	var result struct {
		Data []TwitchUser `json:"data"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&result); err != nil {
		return nil, err
	}
	if len(result.Data) == 0 {
		return nil, fmt.Errorf("no user data from Twitch")
	}
	return &result.Data[0], nil
}

// ---------------------------------------------------------------------------
// AuthProvidersRPC — returns list of enabled auth providers
// ---------------------------------------------------------------------------

func AuthProvidersRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
	providers := []string{"email"}

	if os.Getenv("STEAM_PUBLISHER_KEY") != "" {
		providers = append(providers, "steam")
	}
	if os.Getenv("GOOGLE_CLIENT_ID") != "" {
		providers = append(providers, "google")
	}
	if os.Getenv("TWITCH_CLIENT_ID") != "" {
		providers = append(providers, "twitch")
	}
	if os.Getenv("DISCORD_CLIENT_ID") != "" {
		providers = append(providers, "discord")
	}
	if os.Getenv("APPLE_CLIENT_ID") != "" {
		providers = append(providers, "apple")
	}

	resp, _ := json.Marshal(map[string]interface{}{
		"providers": providers,
	})
	return string(resp), nil
}

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
