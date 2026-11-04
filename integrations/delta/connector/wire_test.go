package main

import (
	"encoding/hex"
	"testing"
)

func uuidFixture(start byte) uuid {
	var id uuid
	for i := range id {
		id[i] = start + byte(i)
	}
	return id
}

func TestRequestWireFixtures(t *testing.T) {
	name := "CAOS – eshear"
	request := uuidFixture(0)
	for _, test := range []struct {
		name     string
		encode   func() ([]byte, error)
		expected string
	}{
		{"runner absent", func() ([]byte, error) { return machineRunnerRequest(nil).encode(42) },
			"922a92339190"},
		{"runner named", func() ([]byte, error) { return machineRunnerRequest(&name).encode(42) },
			"922a92339191af43414f5320e2809320657368656172"},
		{"turn acknowledgment", func() ([]byte, error) { return turnAcknowledgmentRequest(request).encode(42) },
			"922a923592c410000102030405060708090a0b0c0d0e0f90"},
		{"heartbeat", func() ([]byte, error) { return heartbeatRequest().encode(42) },
			"922a922190"},
	} {
		t.Run(test.name, func(t *testing.T) {
			encoded, err := test.encode()
			if err != nil || hex.EncodeToString(encoded) != test.expected {
				t.Fatalf("wrong wire encoding: %x %v", encoded, err)
			}
		})
	}
}

func TestMachineTurnFixture(t *testing.T) {
	fixture := "921294c410000102030405060708090a0b0c0d0e0f" +
		"c410101112131415161718191a1b1c1d1e1f" +
		"c410202122232425262728292a2b2c2d2e2f" +
		"97c410303132333435363738393a3b3c3d3e3f2a90909090c2"
	for _, encoded := range []string{
		fixture, "921295" + fixture[6:] + "81a6667574757265c3",
	} {
		raw, err := hex.DecodeString(encoded)
		if err != nil {
			t.Fatal(err)
		}
		message, err := decodeVariant(raw)
		if err != nil {
			t.Fatal(err)
		}
		turn, err := decodeTurn(message)
		if err != nil {
			t.Fatal(err)
		}
		if turn.Machine != uuidFixture(0) || turn.Request != uuidFixture(16) ||
			turn.History != uuidFixture(32) {
			t.Fatalf("incorrect dispatch identities: %+v", turn)
		}
		if turn.Start.Sender != uuidFixture(48) {
			t.Fatal("incorrect sender identity")
		}
		if turn.Start.Epoch != 42 {
			t.Fatal("incorrect turn epoch")
		}
	}
}

func TestRejectMalformedWire(t *testing.T) {
	for _, value := range []string{
		"", "c0", "90", "9212", "92129000", "9212ddffffffff",
		"92ff90", "92cd010090", "cfffffffffffffffff",
	} {
		raw, err := hex.DecodeString(value)
		if err != nil {
			t.Fatal(err)
		}
		if _, err := decodeVariant(raw); err == nil {
			t.Errorf("accepted malformed message: %s", value)
		}
	}
	if _, err := decodeFields([]byte{0x90}, 4, 4); err == nil {
		t.Error("accepted missing dispatch fields")
	}
	if _, err := decodeUUID([]byte{0xc4, 0}); err == nil {
		t.Error("accepted invalid UUID")
	}
}

func TestProtocolUnsignedIntegersDoNotWrap(t *testing.T) {
	for _, raw := range [][]byte{
		{0x92, 0x05, 0x92, 0xff, 0x21},
		{0x92, 0x05, 0x92, 0xd0, 0xff, 0x21},
		{0x92, 0x05, 0x92, 0xcf, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x21},
	} {
		message, err := decodeVariant(raw)
		if err != nil {
			t.Fatal(err)
		}
		if _, _, err := decodeResponse(message); err == nil {
			t.Fatal("negative/overflowing response id accepted")
		}
	}
	if _, err := (clientRequest{tag: 256, payload: []any{}}).encode(1); err == nil {
		t.Fatal("oversized request tag accepted")
	}
}

func TestUUIDIdentity(t *testing.T) {
	id, err := newUUID()
	if err != nil {
		t.Fatal(err)
	}
	parsed, err := parseUUID(id.String())
	if err != nil || parsed != id || id[6]>>4 != 4 || id[8]>>6 != 2 {
		t.Fatalf("invalid generated UUID: %s %v", id.String(), err)
	}
	for _, bad := range []string{"", "000102030405060708090a0b0c0d0e0f", "not-a-uuid"} {
		if _, err := parseUUID(bad); err == nil {
			t.Errorf("accepted invalid UUID: %q", bad)
		}
	}
}
