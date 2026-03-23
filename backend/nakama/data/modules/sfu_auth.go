package main

import (
	"crypto"
	"crypto/rand"
	"crypto/rsa"
	"crypto/sha256"
	"crypto/x509"
	"encoding/base64"
	"encoding/json"
	"encoding/pem"
	"fmt"
	"os"
	"strings"
	"time"
)

type SFUTokenClaims struct {
	// Registered claims
	Issuer    string `json:"iss"`
	ExpiresAt int64  `json:"exp"`
	IssuedAt  int64  `json:"iat"`

	// Custom claims
	UserID    string `json:"uid"`
	SessionID string `json:"sid"`
	Type      string `json:"type"`            // "stream" or "voice"
	Role      string `json:"role"`            // "host", "viewer", "member"
	CrewID    string `json:"crew_id"`
	ChannelID string `json:"ch_id,omitempty"` // voice sessions only
	Region    string `json:"region"`
}

var sfuPrivateKey *rsa.PrivateKey

func initSFUAuth() error {
	keyPEM := os.Getenv("SFU_JWT_PRIVATE_KEY")
	if keyPEM == "" {
		return nil
	}

	block, _ := pem.Decode([]byte(keyPEM))
	if block == nil {
		return fmt.Errorf("sfu_auth: failed to decode PEM block")
	}

	key, err := x509.ParsePKCS1PrivateKey(block.Bytes)
	if err != nil {
		k, err2 := x509.ParsePKCS8PrivateKey(block.Bytes)
		if err2 != nil {
			return fmt.Errorf("sfu_auth: parse private key: PKCS1=%v PKCS8=%v", err, err2)
		}
		var ok bool
		key, ok = k.(*rsa.PrivateKey)
		if !ok {
			return fmt.Errorf("sfu_auth: key is not RSA")
		}
	}

	sfuPrivateKey = key
	return nil
}

func sfuAuthEnabled() bool {
	return sfuPrivateKey != nil
}

func signSFUToken(claims SFUTokenClaims) (string, error) {
	if sfuPrivateKey == nil {
		return "", fmt.Errorf("SFU auth not configured")
	}

	now := time.Now()
	claims.Issuer = "mello-nakama"
	claims.IssuedAt = now.Unix()
	claims.ExpiresAt = now.Add(5 * time.Minute).Unix()

	header := base64URLEncode([]byte(`{"alg":"RS256","typ":"JWT"}`))

	payload, err := json.Marshal(claims)
	if err != nil {
		return "", fmt.Errorf("sfu_auth: marshal claims: %w", err)
	}
	payloadEnc := base64URLEncode(payload)

	signingInput := header + "." + payloadEnc
	hash := sha256.Sum256([]byte(signingInput))
	sig, err := rsa.SignPKCS1v15(rand.Reader, sfuPrivateKey, crypto.SHA256, hash[:])
	if err != nil {
		return "", fmt.Errorf("sfu_auth: sign: %w", err)
	}

	return signingInput + "." + base64URLEncode(sig), nil
}

func base64URLEncode(data []byte) string {
	return strings.TrimRight(base64.URLEncoding.EncodeToString(data), "=")
}
