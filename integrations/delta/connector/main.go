package main

import (
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"os"
	"os/signal"
	"path/filepath"
	"time"

	"caos/w"
)

func writePrivateJSON(path string, value any) error {
	if path == "" {
		return errors.New("a private output file is required")
	}
	if err := os.MkdirAll(filepath.Dir(path), 0700); err != nil {
		return err
	}
	file, err := os.OpenFile(path, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0600)
	if err != nil {
		return err
	}
	defer file.Close()
	if err := json.NewEncoder(file).Encode(value); err != nil {
		return err
	}
	return file.Sync()
}

func readPrivateJSON(path string, value any) error {
	info, err := os.Lstat(path)
	if err != nil {
		return err
	}
	if !info.Mode().IsRegular() || info.Mode().Perm()&0077 != 0 {
		return errors.New("credential files must be regular files readable only by their owner")
	}
	file, err := os.Open(path)
	if err != nil {
		return err
	}
	defer file.Close()
	decoder := json.NewDecoder(file)
	if err := decoder.Decode(value); err != nil {
		return errors.New("invalid private state file")
	}
	return nil
}

func run(ctx context.Context, args []string) error {
	if len(args) == 0 {
		return errors.New("usage: caos-delta auth-start|auth-wait|auth-check|probe [options]")
	}
	flags := flag.NewFlagSet(args[0], flag.ContinueOnError)
	cloudURL := flags.String("cloud-url", defaultCloudURL, "Delta account service origin")
	requestFile := flags.String("request-file", "", "private pending device authorization file")
	credentialFile := flags.String("credential-file", "", "private credentials output file")
	remoteURL := flags.String("remote-url", defaultRemoteURL, "Delta WebSocket endpoint")
	actorFile := flags.String("actor-file", "", "stable actor identity file")
	registrationName := flags.String("registration-name", "", "temporarily register and clear this diagnostic machine name")
	if err := flags.Parse(args[1:]); err != nil {
		return err
	}
	if flags.NArg() != 0 {
		return errors.New("unexpected positional arguments")
	}
	client, err := newAuthClient(*cloudURL)
	if err != nil {
		return err
	}
	switch args[0] {
	case "auth-start":
		if *requestFile == "" {
			return errors.New("--request-file is required")
		}
		if _, err := os.Lstat(*requestFile); !os.IsNotExist(err) {
			return errors.New("request file already exists or cannot be inspected; resume it with auth-wait")
		}
		authorization, err := client.start(ctx)
		if err != nil {
			return err
		}
		if err := writePrivateJSON(*requestFile, authorization); err != nil {
			return err
		}
		// A verification URL is for the person approving the request. The
		// device secret and eventual account credentials are never printed.
		fmt.Println("Approve this device request in your browser:")
		fmt.Println(authorization.VerificationURL)
		fmt.Println("Then resume with auth-wait and the same --request-file.")
	case "auth-wait":
		if *credentialFile == "" || *requestFile == "" {
			return errors.New("--request-file and --credential-file are required")
		}
		if _, err := os.Lstat(*credentialFile); !os.IsNotExist(err) {
			return errors.New("credential output already exists or cannot be inspected")
		}
		var authorization deviceAuthorization
		if err := readPrivateJSON(*requestFile, &authorization); err != nil {
			return err
		}
		credentials, err := client.wait(ctx, authorization)
		if err != nil {
			return err
		}
		if err := writePrivateJSON(*credentialFile, credentials); err != nil {
			return err
		}
		fmt.Printf("Signed in as account %d; credentials saved privately.\n", credentials.UserID)
	case "auth-check":
		var account credentials
		if err := readPrivateJSON(*credentialFile, &account); err != nil {
			return err
		}
		if _, err := client.deltaToken(ctx, account); err != nil {
			return err
		}
		fmt.Println("Account credentials accepted; Delta service token issued.")
	case "probe":
		var account credentials
		if err := readPrivateJSON(*credentialFile, &account); err != nil {
			return err
		}
		var identity struct {
			Actor string `json:"actor"`
		}
		if _, err := os.Lstat(*actorFile); os.IsNotExist(err) {
			id, err := newUUID()
			if err != nil {
				return err
			}
			identity.Actor = id.String()
			if err := writePrivateJSON(*actorFile, identity); err != nil {
				return err
			}
		} else if err := readPrivateJSON(*actorFile, &identity); err != nil {
			return err
		}
		actor, err := parseUUID(identity.Actor)
		if err != nil {
			return err
		}
		token, err := client.deltaToken(ctx, account)
		if err != nil {
			return err
		}
		ctx, cancel := context.WithTimeout(ctx, 30*time.Second)
		defer cancel()
		var name *string
		if *registrationName != "" {
			name = registrationName
		}
		result, err := probeRemote(ctx, *remoteURL, token, actor, name)
		if err != nil {
			return err
		}
		fmt.Printf("Authenticated Delta protocol exchange succeeded (response tag %d).\n", result.ResponseTag)
		if result.RegistrationVerified {
			fmt.Println("Machine-runner registration and clearing were both accepted.")
		}
	default:
		return errors.New("unknown command")
	}
	return nil
}

func main() {
	if len(os.Args) == 1 && w.Exists("/cas/args") {
		w.Main(workerProbe)
		return
	}
	ctx, cancel := signal.NotifyContext(context.Background(), os.Interrupt)
	defer cancel()
	if err := run(ctx, os.Args[1:]); err != nil {
		fmt.Fprintln(os.Stderr, "caos-delta:", err)
		os.Exit(1)
	}
}
