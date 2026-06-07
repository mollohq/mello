package main

// IsUserPremium reports whether a user has an active m3llo+ subscription.
// Single gate for future paid behavior: unbounded clip history, locked-card
// unlock, extended retention. No gating is built yet, so this returns false
// and nothing consults it for deletion or limiting. It exists so the m3llo+
// work has one well-known seam to wire, not a scattered set of checks.
func IsUserPremium(userID string) bool {
	return false
}
