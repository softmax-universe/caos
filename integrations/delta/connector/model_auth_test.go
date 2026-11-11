package main

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
)

func TestModelAuthorizationUsesOnlyConnectorAccountAndDefaultOrganization(t *testing.T) {
	calls := 0
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		calls++
		if r.Header.Get("Authorization") != "123 connector-account-token" {
			t.Error("model authorization did not use the connector's own credential")
		}
		switch calls {
		case 1:
			if r.Method != http.MethodGet || r.URL.Path != "/client/users/me" || r.ContentLength != 0 {
				t.Error("incorrect account lookup")
			}
			w.Write([]byte(`{"default_organization_id":"org-fixture","organizations":[{"id":"org-other"},{"id":"org-fixture"}]}`))
		case 2:
			if r.Method != http.MethodPost || r.URL.Path != "/client/llm_tokens" {
				t.Error("incorrect model-token endpoint")
			}
			var body map[string]string
			if err := json.NewDecoder(r.Body).Decode(&body); err != nil {
				t.Error(err)
			}
			if len(body) != 1 || body["organization_id"] != "org-fixture" {
				t.Error("model organization was overridden or unrelated metadata was sent")
			}
			w.Write([]byte(`{"token":"test-model-token"}`))
		default:
			t.Error("model check made an inference request")
		}
	}))
	defer server.Close()
	client, err := newAuthClient(server.URL)
	if err != nil {
		t.Fatal(err)
	}
	token, err := client.modelToken(context.Background(), credentials{123, "connector-account-token"})
	if err != nil || token != "test-model-token" || calls != 2 {
		t.Fatalf("incorrect model authorization outcome: calls=%d error=%v", calls, err)
	}
}

func TestModelAuthorizationRejectsMissingOrganizationAndToken(t *testing.T) {
	for _, fixture := range []struct{ user, token string }{
		{`{}`, `{"token":"unused"}`},
		{`{"default_organization_id":"org-fixture","organizations":[]}`, `{"token":"unused"}`},
		{`{"default_organization_id":"org-fixture","organizations":[{"id":"org-other"}]}`, `{"token":"unused"}`},
		{`{"default_organization_id":"org-fixture","organizations":[{"id":"org-fixture"}]}`, `{}`},
		{`{"default_organization_id":"org-fixture","organizations":[{"id":"org-fixture"}]}`, `{"token":"\n "}`},
	} {
		server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			if r.Method == http.MethodGet {
				w.Write([]byte(fixture.user))
			} else {
				if strings.Contains(fixture.token, "unused") {
					t.Error("requested a token without a resolved default organization")
				}
				w.Write([]byte(fixture.token))
			}
		}))
		client, _ := newAuthClient(server.URL)
		if _, err := client.modelToken(context.Background(), credentials{123, "test-token"}); err == nil {
			t.Error("accepted incomplete model authorization")
		}
		server.Close()
	}
}

func TestModelAuthorizationErrorsDoNotExposeSecrets(t *testing.T) {
	for _, status := range []int{401, 402, 403, 500} {
		server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			if r.Method == http.MethodGet {
				w.Write([]byte(`{"default_organization_id":"private-org","organizations":[{"id":"private-org"}]}`))
				return
			}
			w.WriteHeader(status)
			w.Write([]byte(`{"token":"private-response","message":"private-organization-name"}`))
		}))
		client, _ := newAuthClient(server.URL)
		_, err := client.modelToken(context.Background(), credentials{123, "private-account"})
		if err == nil || strings.Contains(err.Error(), "private-") {
			t.Errorf("unsafe model authorization error: %v", err)
		}
		if status == 402 && !strings.Contains(err.Error(), "HTTP 402 Payment Required") {
			t.Error("payment denial lost its actionable status")
		}
		server.Close()
	}
}
