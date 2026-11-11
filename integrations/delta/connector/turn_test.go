package main

import (
	"encoding/hex"
	"math"
	"reflect"
	"testing"
)

func turnFixture(t *testing.T, value string) []byte {
	t.Helper()
	raw, err := hex.DecodeString(value)
	if err != nil {
		t.Fatal(err)
	}
	return raw
}

func TestStartTurnDefaults(t *testing.T) {
	// Checked against the public reference decoder, including every shorter
	// array containing the two required fields and explicit nil defaults.
	const sender = "c410303132333435363738393a3b3c3d3e3f"
	expected := startTurn{Sender: uuidFixture(48), Epoch: 42}
	for _, value := range []string{
		"92" + sender + "2a",
		"93" + sender + "2a90",
		"94" + sender + "2a9090",
		"95" + sender + "2a909090",
		"96" + sender + "2a90909090",
		"97" + sender + "2a90909090c2",
		"97" + sender + "2ac0c0c0c0c0",
		"98" + sender + "2a90909090c281a6667574757265c3",
	} {
		actual, err := decodeStartTurn(turnFixture(t, value))
		if err != nil || !reflect.DeepEqual(actual, expected) {
			t.Fatalf("wrong defaulted turn for %s: %+v %v", value, actual, err)
		}
	}
}

func TestStartTurnPresentFields(t *testing.T) {
	const sender = "c410303132333435363738393a3b3c3d3e3f"
	for _, actor := range []struct {
		wire string
		tag  uint8
		size int
	}{
		{"9200c414000102030405060708090a0b0c0d0e0f10111213", 0, 20},
		{"9201c410000102030405060708090a0b0c0d0e0f", 1, 16},
	} {
		value := "97" + sender + "cfffffffffffffffff" +
			"91a36f726791cfffffffffffffffff91a47475726e" +
			"9192cfffffffffffffffff" + actor.wire + "c3"
		turn, err := decodeStartTurn(turnFixture(t, value))
		if err != nil {
			t.Fatal(err)
		}
		if turn.Sender != uuidFixture(48) || turn.Epoch != math.MaxUint64 ||
			turn.Organization == nil || *turn.Organization != "org" ||
			turn.GenerationStateWriteTimestamp == nil || *turn.GenerationStateWriteTimestamp != math.MaxUint64 ||
			turn.AssistantTurn == nil || *turn.AssistantTurn != "turn" || !turn.RunnerStarted {
			t.Fatalf("incorrect present turn fields: %+v", turn)
		}
		var expectedActor historyActor
		expectedActor.Tag = actor.tag
		for i := 0; i < actor.size; i++ {
			expectedActor.ID[i] = byte(i)
		}
		expectedID := deltaID{Timestamp: math.MaxUint64, Actor: expectedActor}
		if turn.Timeline == nil || *turn.Timeline != expectedID {
			t.Fatalf("incorrect timeline: %+v", turn.Timeline)
		}
	}
}

func TestStartTurnRejectsMalformedFields(t *testing.T) {
	fields := []string{"c410303132333435363738393a3b3c3d3e3f", "2a", "90", "90", "90", "90", "c2"}
	for _, test := range []struct {
		name  string
		index int
		value string
	}{
		{"missing sender", 0, "c0"},
		{"string sender", 0, "b0303132333435363738393a3b3c3d3e3f"},
		{"array sender", 0, "dc001001010101010101010101010101010101"},
		{"missing epoch", 1, "c0"},
		{"negative epoch", 1, "ff"},
		{"signed negative epoch", 1, "d0ff"},
		{"float epoch", 1, "ca42280000"},
		{"binary organization", 2, "91c4036f7267"},
		{"two option elements", 2, "92a36f726701"},
		{"negative timestamp", 3, "91ff"},
		{"nil timestamp value", 3, "91c0"},
		{"binary turn id", 4, "91c4047475726e"},
		{"nil timeline value", 5, "91c0"},
		{"negative timeline timestamp", 5, "9192ff9201c410000102030405060708090a0b0c0d0e0f"},
		{"unknown actor", 5, "9192019202c410000102030405060708090a0b0c0d0e0f"},
		{"unit actor", 5, "91920101"},
		{"short commit", 5, "9192019200c410000102030405060708090a0b0c0d0e0f"},
		{"long UUID", 5, "9192019201c414000102030405060708090a0b0c0d0e0f10111213"},
		{"numeric flag", 6, "00"},
	} {
		t.Run(test.name, func(t *testing.T) {
			value := "97"
			for index, field := range fields {
				if index == test.index {
					field = test.value
				}
				value += field
			}
			if _, err := decodeStartTurn(turnFixture(t, value)); err == nil {
				t.Fatal("accepted malformed turn")
			}
		})
	}
	for _, value := range []string{
		"", "c0", "90", "91" + fields[0], "92" + fields[0] + "2a00",
		"ddffffffff",
	} {
		if _, err := decodeStartTurn(turnFixture(t, value)); err == nil {
			t.Errorf("accepted malformed turn: %s", value)
		}
	}
}

func TestDispatchUsesStartTurnValidation(t *testing.T) {
	for _, test := range []struct {
		epoch string
		valid bool
	}{
		{"2a", true},
		{"ff", false},
	} {
		wire := "921294c410000102030405060708090a0b0c0d0e0f" +
			"c410101112131415161718191a1b1c1d1e1f" +
			"c410202122232425262728292a2b2c2d2e2f" +
			"92c410303132333435363738393a3b3c3d3e3f" + test.epoch
		message, err := decodeVariant(turnFixture(t, wire))
		if err != nil {
			t.Fatal(err)
		}
		if _, err := decodeTurn(message); (err == nil) != test.valid {
			t.Fatalf("dispatch validation did not match start validation: %v", err)
		}
	}
}
