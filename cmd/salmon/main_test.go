package main

import (
	"bytes"
	"io/ioutil"
	"os"
	"os/user"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/dimonomid/salmon"
	"github.com/dimonomid/salmon/internal/setup"
	"github.com/dimonomid/salmon/logs"
)

func TestConfigInitCreatesConfigWithoutOverwritingIt(t *testing.T) {
	path := filepath.Join(t.TempDir(), "etc", "salmon.yml")
	command := newRootCommand()
	output := &bytes.Buffer{}
	command.SetOut(output)
	command.SetErr(output)
	command.SetArgs([]string{"setup", "create-config", "--config", path})
	if err := command.Execute(); err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(output.String(), "Created configuration") {
		t.Fatalf("unexpected command output: %q", output.String())
	}

	data, err := ioutil.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if got, want := string(data), string(mustSetupAsset("assets/setup/salmon.yml")); got != want {
		t.Fatalf("config contents = %q, want %q", got, want)
	}
	if _, err := loadConfig(path); err != nil {
		t.Fatalf("generated config is invalid: %v", err)
	}

	command = newRootCommand()
	command.SetOut(output)
	command.SetErr(output)
	command.SetArgs([]string{"setup", "create-config", "--config", path})
	if err := command.Execute(); err != nil {
		t.Fatal(err)
	}
	data, err = ioutil.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if got, want := string(data), string(mustSetupAsset("assets/setup/salmon.yml")); got != want {
		t.Fatalf("config was overwritten: got %q, want %q", got, want)
	}
}

func TestRunnableCommandsRejectPositionalArguments(t *testing.T) {
	for _, args := range [][]string{
		{"unexpected"},
		{"setup", "unexpected"},
		{"setup", "create-config", "unexpected"},
		{"setup", "create-user", "unexpected"},
		{"setup", "install-service", "unexpected"},
	} {
		command := newRootCommand()
		command.SetArgs(args)
		if err := command.Execute(); err == nil {
			t.Errorf("command %q accepted an unexpected positional argument", args)
		}
	}
}

func TestSetupOperationsDoNotPolluteTopLevelCommands(t *testing.T) {
	command := newRootCommand()
	commands := command.Commands()
	if len(commands) != 1 || commands[0].Name() != "setup" {
		t.Fatalf("top-level commands = %v, want only setup", commands)
	}
	if !strings.Contains(commands[0].Long, "Perform the complete setup") {
		t.Fatalf("setup long help = %q, want complete-setup description", commands[0].Long)
	}
	if flag := commands[0].PersistentFlags().Lookup("reinstall"); flag == nil || flag.DefValue != "false" {
		t.Fatalf("setup --reinstall flag = %#v, want default false", flag)
	}

	setupCommands := map[string]bool{}
	for _, subcommand := range commands[0].Commands() {
		setupCommands[subcommand.Name()] = true
	}
	for _, want := range []string{"create-config", "create-user", "install-service"} {
		if !setupCommands[want] {
			t.Errorf("setup subcommands = %v, missing %q", setupCommands, want)
		}
	}
	for _, subcommand := range commands[0].Commands() {
		if subcommand.Name() == "install-service" && !strings.Contains(subcommand.Short, "Install the executable and systemd service") {
			t.Errorf("install-service description = %q, want executable and service installation", subcommand.Short)
		}
	}
}

func TestSalmonSetupRootErrorProvidesSudoGuidance(t *testing.T) {
	if err := salmonSetupRootError(0, "bin/salmon setup"); err != nil {
		t.Fatalf("root setup error = %v, want nil", err)
	}
	err := salmonSetupRootError(1000, "bin/salmon setup --reinstall")
	if err == nil {
		t.Fatal("non-root setup was accepted")
	}
	for _, want := range []string{
		"requires root privileges",
		"Hint: Rerun it with sudo:",
		"sudo bin/salmon setup --reinstall",
	} {
		if !strings.Contains(err.Error(), want) {
			t.Fatalf("non-root setup error = %q, want %q", err, want)
		}
	}
}

func TestSalmonStartHintRestartsAfterReinstall(t *testing.T) {
	for _, test := range []struct {
		name        string
		reinstalled bool
		want        string
	}{
		{name: "initial setup", want: "sudo systemctl start salmon.service"},
		{name: "reinstall", reinstalled: true, want: "sudo systemctl restart salmon.service"},
	} {
		t.Run(test.name, func(t *testing.T) {
			output := &bytes.Buffer{}
			if err := printSalmonStartHint(output, test.reinstalled); err != nil {
				t.Fatal(err)
			}
			if !strings.Contains(output.String(), test.want) {
				t.Fatalf("start hint = %q, want %q", output.String(), test.want)
			}
		})
	}
}

func TestCreateSalmonUserInstallsSysusersConfigurationAndCreatesAccount(t *testing.T) {
	path := filepath.Join(t.TempDir(), "sysusers.d", "salmon.conf")
	output := &bytes.Buffer{}
	var calls [][]string
	run := func(name string, args ...string) error {
		if !strings.Contains(output.String(), "Created systemd sysusers configuration") {
			t.Fatal("sysusers command ran before file creation was reported")
		}
		calls = append(calls, append([]string{name}, args...))
		return nil
	}

	if err := createSalmonUserAt(output, path, run); err != nil {
		t.Fatal(err)
	}
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if got, want := string(data), string(mustSetupAsset("assets/setup/salmon.sysusers")); got != want {
		t.Fatalf("sysusers configuration = %q, want %q", got, want)
	}
	if got, want := len(calls), 1; got != want {
		t.Fatalf("command count = %d, want %d", got, want)
	}
	if got, want := strings.Join(calls[0], " "), "systemd-sysusers "+path; got != want {
		t.Fatalf("command = %q, want %q", got, want)
	}
	if !strings.Contains(output.String(), "Created systemd sysusers configuration") {
		t.Fatalf("unexpected command output: %q", output.String())
	}
}

func TestInstallSalmonExecutableCopiesFromNonSystemDirectory(t *testing.T) {
	directory := t.TempDir()
	source := filepath.Join(directory, "Downloads", "salmon")
	destination := filepath.Join(directory, "usr", "local", "bin", "salmon")
	if err := os.MkdirAll(filepath.Dir(source), 0755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(source, []byte("salmon binary"), 0755); err != nil {
		t.Fatal(err)
	}
	output := &bytes.Buffer{}

	executable, preserved, err := installSalmonExecutableAt(output, source, destination, false)
	if err != nil {
		t.Fatal(err)
	}
	if executable != destination {
		t.Fatalf("service executable = %q, want %q", executable, destination)
	}
	if preserved {
		t.Fatal("new executable was reported as preserved")
	}
	if !strings.Contains(output.String(), "Installed executable at "+destination) {
		t.Fatalf("output = %q, want installation report", output.String())
	}
	data, err := os.ReadFile(destination)
	if err != nil {
		t.Fatal(err)
	}
	if got, want := string(data), "salmon binary"; got != want {
		t.Fatalf("installed executable = %q, want %q", got, want)
	}
}

func TestInstallSalmonExecutableUsesSystemLocationInPlace(t *testing.T) {
	for _, source := range []string{
		"/bin/salmon",
		"/usr/bin/salmon",
		"/usr/local/bin/salmon",
		"/opt/salmon/bin/salmon",
		"/nix/store/hash-salmon/bin/salmon",
		"/snap/salmon/current/bin/salmon",
	} {
		t.Run(source, func(t *testing.T) {
			output := &bytes.Buffer{}
			executable, preserved, err := installSalmonExecutableAt(output, source, filepath.Join(t.TempDir(), "salmon"), false)
			if err != nil {
				t.Fatal(err)
			}
			if executable != source {
				t.Fatalf("service executable = %q, want %q", executable, source)
			}
			if preserved {
				t.Fatal("system executable was reported as preserved installation")
			}
			if output.Len() != 0 {
				t.Fatalf("output = %q, want none", output.String())
			}
		})
	}
}

func TestRequireSalmonServiceAccountChecksUserAndGroup(t *testing.T) {
	lookupUser := func(name string) (*user.User, error) {
		if name != salmonUserName {
			t.Fatalf("user name = %q, want %q", name, salmonUserName)
		}
		return &user.User{Username: name}, nil
	}
	lookupGroup := func(name string) (*user.Group, error) {
		if name != salmonGroupName {
			t.Fatalf("group name = %q, want %q", name, salmonGroupName)
		}
		return &user.Group{Name: name}, nil
	}
	if err := requireSalmonServiceAccountWith(lookupUser, lookupGroup); err != nil {
		t.Fatal(err)
	}

	err := requireSalmonServiceAccountWith(
		func(string) (*user.User, error) { return nil, user.UnknownUserError(salmonUserName) },
		lookupGroup,
	)
	if err == nil || !strings.Contains(err.Error(), "sudo salmon setup create-user") {
		t.Fatalf("missing-user error = %v, want user-create guidance", err)
	}

	err = requireSalmonServiceAccountWith(
		lookupUser,
		func(string) (*user.Group, error) { return nil, user.UnknownGroupError(salmonGroupName) },
	)
	if err == nil || !strings.Contains(err.Error(), "sudo salmon setup create-user") {
		t.Fatalf("missing-group error = %v, want user-create guidance", err)
	}
}

func TestLogLevelFlagDefaultsToInfoAndRejectsInvalidValues(t *testing.T) {
	command := newRootCommand()
	flag := command.Flags().Lookup("log-level")
	if flag == nil || flag.DefValue != "info" {
		t.Fatalf("log-level flag = %#v, want default info", flag)
	}
	command.SetOut(&bytes.Buffer{})
	command.SetErr(&bytes.Buffer{})
	command.SetArgs([]string{"--log-level", "verbose"})
	if err := command.Execute(); err == nil || !strings.Contains(err.Error(), "invalid log level") {
		t.Fatalf("error = %v, want invalid-log-level error", err)
	}
}

func TestVersionFlagPrintsBuildInformation(t *testing.T) {
	command := newRootCommand()
	output := &bytes.Buffer{}
	command.SetOut(output)
	command.SetArgs([]string{"--version"})
	if err := command.Execute(); err != nil {
		t.Fatal(err)
	}
	for _, want := range []string{"Salmon dev\n", "Commit: none\n", "Build time: unknown\n", "Built by: unknown\n", "GOOS: ", "CGO: "} {
		if !strings.Contains(output.String(), want) {
			t.Fatalf("version output %q does not contain %q", output.String(), want)
		}
	}
}

func TestSalmonServiceTemplateIncludesExecutableAndConfig(t *testing.T) {
	unit, err := setup.RenderSystemdUnitTemplate("salmon.service.tpl", string(mustSetupAsset("assets/setup/salmon.service.tpl")), struct {
		Executable     string
		ConfigFilename string
	}{"/usr/local/bin/salmon", "/etc/salmon.yml"})
	if err != nil {
		t.Fatal(err)
	}
	for _, want := range []string{"User=salmon", "Group=salmon", "ExecStart=\"/usr/local/bin/salmon\" --config \"/etc/salmon.yml\"", "WantedBy=multi-user.target"} {
		if !strings.Contains(unit, want) {
			t.Fatalf("unit %q does not contain %q", unit, want)
		}
	}
}

func TestSalmonServiceTemplateEscapesSystemdSpecifiers(t *testing.T) {
	unit, err := setup.RenderSystemdUnitTemplate("salmon.service.tpl", string(mustSetupAsset("assets/setup/salmon.service.tpl")), struct {
		Executable     string
		ConfigFilename string
	}{"/tmp/sal%mon", "/tmp/a%b.yml"})
	if err != nil {
		t.Fatal(err)
	}
	if want := "ExecStart=\"/tmp/sal%%mon\" --config \"/tmp/a%%b.yml\""; !strings.Contains(unit, want) {
		t.Fatalf("unit %q does not contain %q", unit, want)
	}
}

func TestRunSalmonSuggestsSetupWhenConfigIsMissing(t *testing.T) {
	path := filepath.Join(t.TempDir(), "missing.yml")
	err := runSalmon(path, logs.Info)
	if err == nil || strings.Contains(err.Error(), "setup") {
		t.Fatalf("runSalmon() error = %v, want no setup guidance for custom config", err)
	}
}

func TestSalmonConfigReadErrorSuggestsSetupForDefaultConfig(t *testing.T) {
	err := salmonConfigReadError(defaultSalmonConfig, os.ErrNotExist)
	for _, want := range []string{
		"Hint: Run the following command to create the default configuration and install the service:\n\n    sudo " + os.Args[0] + " setup",
		"To create only the default configuration without installing the service, run:\n\n    sudo " + os.Args[0] + " setup create-config",
	} {
		if !strings.Contains(err.Error(), want) {
			t.Fatalf("salmonConfigReadError() = %v, want guidance %q", err, want)
		}
	}
}

func TestLoadConfigRejectsUnknownFields(t *testing.T) {
	path := filepath.Join(t.TempDir(), "salmon.yml")
	if err := os.WriteFile(path, []byte("core:\n  collectorz: []\n"), 0600); err != nil {
		t.Fatal(err)
	}
	if _, err := loadConfig(path); err == nil || !strings.Contains(err.Error(), "collectorz") {
		t.Fatalf("loadConfig error = %v, want unknown-field error", err)
	}
}

func TestLoadConfigUsesSystemdRuleFields(t *testing.T) {
	path := filepath.Join(t.TempDir(), "salmon.yml")
	data := []byte(`core:
  collectors:
    - id: services
      systemd:
        unitRules:
          - names: [one.service, two.service]
            conditions:
              - {subStateContains: auto-restart, result: warning, resolve: {after: 5s, states: [active, inactive, not-sent-by-systemd]}}
`)
	if err := os.WriteFile(path, data, 0600); err != nil {
		t.Fatal(err)
	}

	cfg, err := loadConfig(path)
	if err != nil {
		t.Fatal(err)
	}
	got := cfg.Core.Collectors[0].Systemd.UnitRules[0].Names
	if strings.Join(got, ",") != "one.service,two.service" {
		t.Fatalf("rule names = %#v", got)
	}
	condition := cfg.Core.Collectors[0].Systemd.UnitRules[0].Conditions[0]
	if condition.SubStateContains != "auto-restart" || condition.Resolve == nil || condition.Resolve.After != 5*time.Second || len(condition.Resolve.States) != 3 || condition.Resolve.States[0] != "active" || condition.Resolve.States[1] != "inactive" || condition.Resolve.States[2] != "not-sent-by-systemd" || condition.Result != salmon.ItemStateWarning {
		t.Fatalf("rule condition = %#v", condition)
	}
}

func TestLoadConfigUsesWebserverTLSOptions(t *testing.T) {
	path := filepath.Join(t.TempDir(), "salmon.yml")
	data := []byte(`core:
  messengers:
    - webserver:
        listenAddress: 127.0.0.1:41990
        tls:
          certFile: /etc/salmon/tls/fullchain.pem
          keyFile: /etc/salmon/tls/privkey.pem
        auth:
          - id: my-laptop
            bearerTokenHash: "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
`)
	if err := os.WriteFile(path, data, 0600); err != nil {
		t.Fatal(err)
	}
	cfg, err := loadConfig(path)
	if err != nil {
		t.Fatal(err)
	}
	tlsConfig := cfg.Core.Messengers[0].Webserver.TLS
	if tlsConfig == nil || tlsConfig.CertFile != "/etc/salmon/tls/fullchain.pem" || tlsConfig.KeyFile != "/etc/salmon/tls/privkey.pem" {
		t.Fatalf("TLS config = %#v", tlsConfig)
	}
	auth := cfg.Core.Messengers[0].Webserver.Auth
	if len(auth) != 1 || auth[0].ID != "my-laptop" || auth[0].BearerTokenHash == "" {
		t.Fatalf("auth config = %#v", auth)
	}
}

func TestLoadConfigRejectsOldNestedBearerTokenAuth(t *testing.T) {
	path := filepath.Join(t.TempDir(), "salmon.yml")
	data := []byte(`core:
  messengers:
    - webserver:
        listenAddress: 127.0.0.1:41990
        auth:
          bearerTokens:
            - id: my-laptop
              tokenHash: "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
`)
	if err := os.WriteFile(path, data, 0600); err != nil {
		t.Fatal(err)
	}
	if _, err := loadConfig(path); err == nil {
		t.Fatal("old nested bearer-token authentication was accepted")
	}
}
