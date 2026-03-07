package main

const (
	StatusOnline       = "online"
	StatusIdle         = "idle"
	StatusDoNotDisturb = "dnd"
	StatusOffline      = "offline"
)

func IsValidStatus(status string) bool {
	switch status {
	case StatusOnline, StatusIdle, StatusDoNotDisturb, StatusOffline:
		return true
	}
	return false
}
