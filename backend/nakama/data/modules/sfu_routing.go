package main

import "os"

var sfuEndpoints = map[string]string{
	"eu-west": "wss://sfu-eu.m3llo.app/ws",
	"us-east": "wss://sfu-us.m3llo.app/ws",
}

func init() {
	if eu := os.Getenv("SFU_ENDPOINT_EU"); eu != "" {
		sfuEndpoints["eu-west"] = eu
	}
	if us := os.Getenv("SFU_ENDPOINT_US"); us != "" {
		sfuEndpoints["us-east"] = us
	}
}

func selectSFURegion(userRegion string) string {
	switch userRegion {
	case "NA", "SA":
		return "us-east"
	default:
		return "eu-west"
	}
}

func sfuEndpointForRegion(region string) string {
	if ep, ok := sfuEndpoints[region]; ok {
		return ep
	}
	return sfuEndpoints["eu-west"]
}
