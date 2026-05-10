package main

import (
	"encoding/json"
	"testing"
)

func TestStreamSessionDataToObjectKeepsJSONObjectShape(t *testing.T) {
	data := StreamSessionData{
		SessionID:    "stream_test_1",
		StreamerID:   "user_1",
		StreamerName: "alice",
		Title:        "test stream",
		DurationMin:  12,
		PeakViewers:  3,
		ViewerIDs:    []string{"user_2"},
		SnapshotURLs: []string{"https://cdn.example/snapshots/1.jpg"},
	}

	obj, err := streamSessionDataToObject(data)
	if err != nil {
		t.Fatalf("streamSessionDataToObject returned error: %v", err)
	}

	encoded, err := json.Marshal(map[string]interface{}{"data": obj})
	if err != nil {
		t.Fatalf("json.Marshal failed: %v", err)
	}

	var parsed map[string]interface{}
	if err := json.Unmarshal(encoded, &parsed); err != nil {
		t.Fatalf("json.Unmarshal failed: %v", err)
	}

	dataObj, ok := parsed["data"].(map[string]interface{})
	if !ok {
		t.Fatalf("data is not a JSON object: %T", parsed["data"])
	}
	if got := dataObj["session_id"]; got != "stream_test_1" {
		t.Fatalf("session_id mismatch: got %v", got)
	}
	if _, ok := dataObj["snapshot_urls"].([]interface{}); !ok {
		t.Fatalf("snapshot_urls is not an array: %T", dataObj["snapshot_urls"])
	}
}

func TestDecodeStreamSessionDataFromObject(t *testing.T) {
	expected := StreamSessionData{
		SessionID:    "stream_test_2",
		StreamerID:   "user_2",
		StreamerName: "bob",
		Title:        "another stream",
		DurationMin:  7,
		PeakViewers:  2,
		ViewerIDs:    []string{"user_3"},
		SnapshotURLs: []string{"https://cdn.example/snapshots/2.jpg"},
	}

	obj, err := streamSessionDataToObject(expected)
	if err != nil {
		t.Fatalf("streamSessionDataToObject returned error: %v", err)
	}

	decoded, err := decodeStreamSessionData(obj)
	if err != nil {
		t.Fatalf("decodeStreamSessionData returned error: %v", err)
	}

	if decoded.SessionID != expected.SessionID {
		t.Fatalf("session_id mismatch: got %q want %q", decoded.SessionID, expected.SessionID)
	}
	if decoded.StreamerID != expected.StreamerID {
		t.Fatalf("streamer_id mismatch: got %q want %q", decoded.StreamerID, expected.StreamerID)
	}
	if len(decoded.SnapshotURLs) != len(expected.SnapshotURLs) || decoded.SnapshotURLs[0] != expected.SnapshotURLs[0] {
		t.Fatalf("snapshot_urls mismatch: got %v want %v", decoded.SnapshotURLs, expected.SnapshotURLs)
	}
}
