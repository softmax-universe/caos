package main

import (
	"context"
	"encoding/json"
	"errors"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func authorizationFixture() deviceAuthorization {
	return deviceAuthorization{
		DeviceSecret:    "test-device-secret",
		UserCode:        "TEST-CODE",
		VerificationURL: "https://dashboard.zed.dev/delta/device?user_code=TEST-CODE",
		IntervalSeconds: 1,
		ExpiresAt:       time.Now().Add(time.Hour),
	}
}

func TestDeviceStart(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Method != http.MethodPost || r.URL.Path != "/frontend/auth/delta/device/start" {
			t.Errorf("unexpected request: %s %s", r.Method, r.URL.Path)
		}
		if r.Header.Get("Content-Type") != "application/json" {
			t.Error("missing JSON content type")
		}
		if r.Header.Get("Authorization") != "" {
			t.Error("device start must not use another account's credentials")
		}
		json.NewEncoder(w).Encode(authorizationFixture())
	}))
	defer server.Close()
	client, err := newAuthClient(server.URL)
	if err != nil {
		t.Fatal(err)
	}
	result, err := client.start(context.Background())
	if err != nil || result.DeviceSecret != "test-device-secret" {
		t.Fatalf("unexpected start: %+v %v", result, err)
	}
}

func TestDevicePollStates(t *testing.T) {
	for _, body := range []string{
		`{"status":"pending"}`, `{"status":"slow_down"}`,
		`{"status":"expired"}`, `{"status":"consumed"}`,
		`{"status":"approved","legacy_user_id":123,"access_token":"test-token"}`,
		`{"status":"approved","legacy_user_id":"123","access_token":"test-token"}`,
	} {
		t.Run(body, func(t *testing.T) {
			server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
				if r.Method != http.MethodPost || r.URL.Path != "/frontend/auth/delta/device/poll" {
					t.Error("incorrect poll endpoint")
				}
				var payload map[string]string
				if err := json.NewDecoder(r.Body).Decode(&payload); err != nil {
					t.Error(err)
				}
				if payload["device_secret"] != "test-device-secret" {
					t.Error("incorrect device secret")
				}
				w.Write([]byte(body))
			}))
			defer server.Close()
			client, err := newAuthClient(server.URL)
			if err != nil {
				t.Fatal(err)
			}
			if _, err := client.poll(context.Background(), "test-device-secret"); err != nil {
				t.Fatal(err)
			}
		})
	}
}

func TestInvalidDevicePoll(t *testing.T) {
	for _, body := range []string{
		`{"status":"approved"}`, `{"status":"approved","legacy_user_id":0,"access_token":"x"}`,
		`{"status":"approved","legacy_user_id":"not-an-id","access_token":"x"}`,
		`{"status":"approved","legacy_user_id":4294967296,"access_token":"x"}`,
		`{"status":"unknown"}`, `not json`, `{} {}`,
	} {
		t.Run(body, func(t *testing.T) {
			server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
				w.Write([]byte(body))
			}))
			defer server.Close()
			client, err := newAuthClient(server.URL)
			if err != nil {
				t.Fatal(err)
			}
			if _, err := client.poll(context.Background(), "test-device-secret"); err == nil {
				t.Fatal("invalid response accepted")
			}
		})
	}
}

func TestDeviceVerificationURLAndExpiry(t *testing.T) {
	for _, target := range []string{
		"http://dashboard.zed.dev/delta/device",
		"https://dashboard.zed.dev.evil.example/delta/device",
		"https://dashboard.zed.dev@evil.example/delta/device",
		"https://dashboard.zed.dev:8443/delta/device",
		"https://dashboard.zed.dev/wrong",
	} {
		a := authorizationFixture()
		a.VerificationURL = target
		if a.validate(time.Now()) == nil {
			t.Errorf("accepted unsafe verification URL: %s", target)
		}
	}
	a := authorizationFixture()
	a.ExpiresAt = time.Now().Add(-time.Second)
	if a.validate(time.Now()) == nil {
		t.Error("accepted expired authorization")
	}
}

func TestAuthenticationErrorDoesNotExposeResponse(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		w.WriteHeader(http.StatusUnauthorized)
		w.Write([]byte(`{"access_token":"must-not-appear"}`))
	}))
	defer server.Close()
	client, err := newAuthClient(server.URL)
	if err != nil {
		t.Fatal(err)
	}
	_, err = client.poll(context.Background(), "test-device-secret")
	if err == nil || strings.Contains(err.Error(), "must-not-appear") {
		t.Fatalf("unsafe authentication error: %v", err)
	}
}

func TestDeltaTokenIssuance(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/client/delta_tokens" || r.Method != http.MethodPost {
			t.Error("incorrect Delta token endpoint")
		}
		if r.Header.Get("Authorization") != "123 test-account-token" {
			t.Error("incorrect account authorization format")
		}
		w.Write([]byte(`{"token":"test-delta-token"}`))
	}))
	defer server.Close()
	client, err := newAuthClient(server.URL)
	if err != nil {
		t.Fatal(err)
	}
	got, err := client.deltaToken(context.Background(), credentials{123, "test-account-token"})
	if err != nil || got != "test-delta-token" {
		t.Fatalf("incorrect token: %v", err)
	}
}

func TestAuthenticationRedirectDoesNotForwardSecret(t *testing.T) {
	contacted := false
	target := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		contacted = true
	}))
	defer target.Close()
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		http.Redirect(w, r, target.URL, http.StatusTemporaryRedirect)
	}))
	defer server.Close()
	client, err := newAuthClient(server.URL)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := client.poll(context.Background(), "test-device-secret"); err == nil {
		t.Fatal("authentication redirect accepted")
	}
	if contacted {
		t.Fatal("secret was forwarded to a redirect target")
	}
}

func TestWaitCancellation(t *testing.T) {
	client, err := newAuthClient(defaultCloudURL)
	if err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	if _, err := client.wait(ctx, authorizationFixture()); !errors.Is(err, context.Canceled) {
		t.Fatalf("expected cancellation, got %v", err)
	}
}

func TestCredentialFilePrivacy(t *testing.T) {
	path := filepath.Join(t.TempDir(), "private", "credentials.json")
	want := credentials{UserID: 123, AccessToken: "test-token"}
	if err := writePrivateJSON(path, want); err != nil {
		t.Fatal(err)
	}
	info, err := os.Stat(path)
	if err != nil || info.Mode().Perm() != 0600 {
		t.Fatalf("unsafe credential permissions: %v %v", info, err)
	}
	var got credentials
	if err := readPrivateJSON(path, &got); err != nil || got != want {
		t.Fatalf("credential round trip failed: %v", err)
	}
	if err := writePrivateJSON(path, credentials{}); err == nil {
		t.Fatal("existing credentials overwritten")
	}
	link := filepath.Join(filepath.Dir(path), "link")
	if err := os.Symlink(path, link); err != nil {
		t.Fatal(err)
	}
	if err := readPrivateJSON(link, &got); err == nil {
		t.Fatal("credential symlink accepted")
	}
	if err := os.Chmod(path, 0644); err != nil {
		t.Fatal(err)
	}
	if err := readPrivateJSON(path, &got); err == nil {
		t.Fatal("world-readable credentials accepted")
	}
}
