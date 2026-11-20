package main

import (
	"crypto/hmac"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
)

type signedBridgeState struct {
	State json.RawMessage
	MAC   string
}

func bridgeStateMAC(key, data []byte, binding string) []byte {
	mac := hmac.New(sha256.New, key)
	mac.Write([]byte("caos-delta-continuation-v1\x00"))
	mac.Write([]byte(binding))
	mac.Write([]byte{0})
	mac.Write(data)
	return mac.Sum(nil)
}

func signBridgeState(state contextBridgeState, key []byte, binding string) ([]byte, error) {
	if len(key) == 0 {
		return nil, errors.New("missing bridge signing identity")
	}
	data, err := json.Marshal(state)
	if err != nil {
		return nil, err
	}
	return json.Marshal(signedBridgeState{State: data, MAC: hex.EncodeToString(bridgeStateMAC(key, data, binding))})
}

func verifyBridgeState(data, key []byte, binding string) (contextBridgeState, error) {
	var envelope signedBridgeState
	var state contextBridgeState
	if len(key) == 0 || json.Unmarshal(data, &envelope) != nil {
		return state, errors.New("invalid signed bridge continuation")
	}
	mac, err := hex.DecodeString(envelope.MAC)
	if err != nil || !hmac.Equal(mac, bridgeStateMAC(key, envelope.State, binding)) {
		return state, errors.New("bridge continuation signature does not match this operation")
	}
	if json.Unmarshal(envelope.State, &state) != nil {
		return state, errors.New("invalid signed bridge state")
	}
	return state, nil
}
