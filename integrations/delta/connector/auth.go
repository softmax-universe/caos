package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"strconv"
	"strings"
	"time"
)

const defaultCloudURL = "https://cloud.zed.dev"

type deviceAuthorization struct {
	DeviceSecret    string    `json:"device_secret"`
	UserCode        string    `json:"user_code"`
	VerificationURL string    `json:"verification_url"`
	IntervalSeconds int       `json:"interval_seconds"`
	ExpiresAt       time.Time `json:"expires_at"`
}

type userID uint32

func (id *userID) UnmarshalJSON(data []byte) error {
	text := string(data)
	if strings.HasPrefix(text, `"`) {
		if err := json.Unmarshal(data, &text); err != nil {
			return errors.New("invalid user id")
		}
	}
	value, err := strconv.ParseUint(text, 10, 32)
	if err != nil || value == 0 {
		return errors.New("invalid user id")
	}
	*id = userID(value)
	return nil
}

type credentials struct {
	UserID      userID `json:"user_id"`
	AccessToken string `json:"access_token"`
}

type devicePoll struct {
	Status       string `json:"status"`
	LegacyUserID userID `json:"legacy_user_id"`
	AccessToken  string `json:"access_token"`
}

type authClient struct {
	baseURL string
	http    *http.Client
}

type authHTTPError struct{ Status int }

func (e *authHTTPError) Error() string {
	return fmt.Sprintf("authentication server returned HTTP %d", e.Status)
}

func newAuthClient(baseURL string) (*authClient, error) {
	u, err := url.Parse(baseURL)
	if err != nil || u.Host == "" || u.User != nil || u.RawQuery != "" || u.Fragment != "" {
		return nil, errors.New("invalid cloud origin")
	}
	loopback := u.Hostname() == "127.0.0.1" || u.Hostname() == "::1" || u.Hostname() == "localhost"
	if u.Scheme != "https" && !(u.Scheme == "http" && loopback) {
		return nil, errors.New("cloud origin must use HTTPS")
	}
	if u.Path != "" && u.Path != "/" {
		return nil, errors.New("cloud origin must not contain a path")
	}
	return &authClient{
		baseURL: strings.TrimSuffix(baseURL, "/"),
		http: &http.Client{
			Timeout: 30 * time.Second,
			CheckRedirect: func(_ *http.Request, _ []*http.Request) error {
				return errors.New("authentication redirects are not allowed")
			},
		},
	}, nil
}

func (c *authClient) request(ctx context.Context, method, path string, body any, account *credentials, result any) error {
	var input io.Reader
	if body != nil {
		data, err := json.Marshal(body)
		if err != nil {
			return err
		}
		input = bytes.NewReader(data)
	}
	request, err := http.NewRequestWithContext(ctx, method, c.baseURL+path, input)
	if err != nil {
		return errors.New("could not construct authentication request")
	}
	request.Header.Set("Content-Type", "application/json")
	request.Header.Set("User-Agent", "caos-delta-connector")
	if account != nil {
		if account.UserID == 0 || account.AccessToken == "" {
			return errors.New("valid account credentials are required")
		}
		request.Header.Set("Authorization", fmt.Sprintf("%d %s", account.UserID, account.AccessToken))
	}
	response, err := c.http.Do(request)
	if err != nil {
		// Do not include request/response bodies or credential-bearing URLs.
		return errors.New("authentication request failed")
	}
	defer response.Body.Close()
	if response.StatusCode != http.StatusOK {
		return &authHTTPError{Status: response.StatusCode}
	}
	data, err := io.ReadAll(io.LimitReader(response.Body, (1<<20)+1))
	if err != nil || len(data) > 1<<20 {
		return errors.New("invalid authentication response")
	}
	if err := json.Unmarshal(data, result); err != nil {
		return errors.New("invalid authentication response")
	}
	return nil
}

func (a deviceAuthorization) validate(now time.Time) error {
	u, err := url.Parse(a.VerificationURL)
	if err != nil || u.Scheme != "https" || u.Host != "dashboard.zed.dev" ||
		u.Path != "/delta/device" || u.User != nil || u.Fragment != "" {
		return errors.New("unexpected device verification URL")
	}
	if a.DeviceSecret == "" || a.UserCode == "" || a.IntervalSeconds < 1 ||
		a.IntervalSeconds > 300 || !a.ExpiresAt.After(now) ||
		a.ExpiresAt.After(now.Add(24*time.Hour)) {
		return errors.New("invalid or expired device authorization")
	}
	return nil
}

func (c *authClient) start(ctx context.Context) (deviceAuthorization, error) {
	var authorization deviceAuthorization
	err := c.request(ctx, http.MethodPost, "/frontend/auth/delta/device/start", struct{}{}, nil, &authorization)
	if err == nil {
		err = authorization.validate(time.Now())
	}
	return authorization, err
}

func (c *authClient) poll(ctx context.Context, secret string) (devicePoll, error) {
	var result devicePoll
	if secret == "" {
		return result, errors.New("device secret is required")
	}
	err := c.request(ctx, http.MethodPost, "/frontend/auth/delta/device/poll",
		map[string]string{"device_secret": secret}, nil, &result)
	if err != nil {
		return result, err
	}
	switch result.Status {
	case "pending", "slow_down", "expired", "consumed":
	case "approved":
		if result.LegacyUserID == 0 || result.AccessToken == "" {
			return result, errors.New("approved authorization has no valid credentials")
		}
	default:
		return result, errors.New("unknown device authorization status")
	}
	return result, nil
}

func (c *authClient) deltaToken(ctx context.Context, account credentials) (string, error) {
	var result struct {
		Token string `json:"token"`
	}
	if err := c.request(ctx, http.MethodPost, "/client/delta_tokens", struct{}{}, &account, &result); err != nil {
		return "", err
	}
	if result.Token == "" {
		return "", errors.New("Delta token response has no token")
	}
	return result.Token, nil
}

// Check the connector's own model authorization, not the native application's
// keys. This obtains a short-lived token but performs no inference. A successful
// Delta service-token exchange alone does not establish permission to call a model.
func (c *authClient) modelToken(ctx context.Context, account credentials) (string, error) {
	var user struct {
		DefaultOrganization string `json:"default_organization_id"`
		Organizations       []struct {
			ID string `json:"id"`
		} `json:"organizations"`
	}
	if err := c.request(ctx, http.MethodGet, "/client/users/me", nil, &account, &user); err != nil {
		return "", fmt.Errorf("could not read connector model organization: %w", err)
	}
	found := false
	for _, organization := range user.Organizations {
		if organization.ID != "" && organization.ID == user.DefaultOrganization {
			found = true
		}
	}
	if !found {
		return "", errors.New("connector account has no resolved default model organization")
	}
	var result struct {
		Token string `json:"token"`
	}
	body := map[string]string{"organization_id": user.DefaultOrganization}
	if err := c.request(ctx, http.MethodPost, "/client/llm_tokens", body, &account, &result); err != nil {
		var status *authHTTPError
		if errors.As(err, &status) && status.Status == http.StatusPaymentRequired {
			return "", errors.New("connector model authorization denied (HTTP 402 Payment Required); no inference was started")
		}
		return "", fmt.Errorf("could not authorize connector model access: %w", err)
	}
	if strings.TrimSpace(result.Token) == "" {
		return "", errors.New("model authorization response has no token")
	}
	return result.Token, nil
}

func (c *authClient) wait(ctx context.Context, authorization deviceAuthorization) (credentials, error) {
	if err := authorization.validate(time.Now()); err != nil {
		return credentials{}, err
	}
	ctx, cancel := context.WithDeadline(ctx, authorization.ExpiresAt)
	defer cancel()
	interval := time.Duration(authorization.IntervalSeconds) * time.Second
	for {
		// Wait before the first poll too: a resumed request may have just polled.
		timer := time.NewTimer(interval)
		select {
		case <-ctx.Done():
			timer.Stop()
			return credentials{}, ctx.Err()
		case <-timer.C:
		}
		result, err := c.poll(ctx, authorization.DeviceSecret)
		if err != nil {
			return credentials{}, err
		}
		switch result.Status {
		case "approved":
			return credentials{UserID: result.LegacyUserID, AccessToken: result.AccessToken}, nil
		case "expired", "consumed":
			return credentials{}, fmt.Errorf("device authorization is %s", result.Status)
		case "slow_down":
			interval += 5 * time.Second
		}
	}
}
