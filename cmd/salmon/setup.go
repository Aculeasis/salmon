package main

import (
	"fmt"
	"io"
	"os"
	"os/exec"
	"os/user"
	"path/filepath"
	"strings"

	"github.com/dimonomid/salmon/internal/setup"
)

const (
	defaultSalmonConfig  = "/etc/salmon.yml"
	salmonExecutablePath = "/usr/local/bin/salmon"
	salmonUserName       = "salmon"
	salmonGroupName      = "salmon"
	salmonSysusersPath   = "/usr/local/lib/sysusers.d/salmon.conf"
	salmonUnitPath       = "/etc/systemd/system/salmon.service"
)

// initializeSalmonConfig creates the configuration when absent and reports the
// result to output.
func initializeSalmonConfig(output io.Writer, configFilename string) error {
	created, err := setup.EnsureFile(configFilename, string(mustSetupAsset("assets/setup/salmon.yml")))
	if err != nil {
		return err
	}
	return setup.ReportEnsureResult(output, "configuration", configFilename, created)
}

// createSalmonUser installs Salmon's sysusers configuration and asks systemd
// to create the service user and group when they do not already exist.
func createSalmonUser(output io.Writer) error {
	return createSalmonUserAt(output, salmonSysusersPath, runCommand)
}

func createSalmonUserAt(output io.Writer, sysusersPath string, run setup.CmdRunner) error {
	created, err := setup.EnsureFile(sysusersPath, string(mustSetupAsset("assets/setup/salmon.sysusers")))
	if err != nil {
		return err
	}
	if err := setup.ReportEnsureResult(output, "systemd sysusers configuration", sysusersPath, created); err != nil {
		return err
	}
	if err := run("systemd-sysusers", sysusersPath); err != nil {
		return fmt.Errorf("create Salmon service user and group: %w", err)
	}
	return nil
}

// requireSalmonServiceAccount prevents installing a unit that systemd cannot
// start because its configured user or group is missing.
func requireSalmonServiceAccount() error {
	return requireSalmonServiceAccountWith(user.Lookup, user.LookupGroup)
}

func requireSalmonServiceAccountWith(
	lookupUser func(string) (*user.User, error),
	lookupGroup func(string) (*user.Group, error),
) error {
	if _, err := lookupUser(salmonUserName); err != nil {
		return fmt.Errorf("service user %q does not exist; run `sudo salmon setup create-user`: %w", salmonUserName, err)
	}
	if _, err := lookupGroup(salmonGroupName); err != nil {
		return fmt.Errorf("service group %q does not exist; run `sudo salmon setup create-user`: %w", salmonGroupName, err)
	}
	return nil
}

// installSalmonService installs or explicitly reinstalls the executable and
// systemd unit, enables the service, and reports each result.
func installSalmonService(output io.Writer, configFilename string, reinstall bool) error {
	if err := requireSalmonServiceAccount(); err != nil {
		return err
	}
	absoluteConfigFilename, err := filepath.Abs(configFilename)
	if err != nil {
		return fmt.Errorf("resolve config path: %w", err)
	}
	if _, err := loadConfig(absoluteConfigFilename); err != nil {
		return fmt.Errorf("validate config at %s: %w", absoluteConfigFilename, err)
	}
	executable, err := setup.ExecutablePath()
	if err != nil {
		return err
	}
	executable, executablePreserved, err := installSalmonExecutable(output, executable, reinstall)
	if err != nil {
		return err
	}
	unit, err := setup.RenderSystemdUnitTemplate("salmon.service.tpl", string(mustSetupAsset("assets/setup/salmon.service.tpl")), struct {
		Executable     string
		ConfigFilename string
	}{executable, absoluteConfigFilename})
	if err != nil {
		return err
	}
	installService := setup.InstallSystemdService
	if reinstall {
		installService = setup.ReinstallSystemdService
	}
	created, err := installService(salmonUnitPath, "salmon.service", unit, runCommand)
	if err != nil {
		return err
	}
	if !created && reinstall {
		_, err := fmt.Fprintf(output, "Updated systemd service at %s\n", salmonUnitPath)
		return err
	}
	if err := setup.ReportEnsureResult(output, "systemd service", salmonUnitPath, created); err != nil {
		return err
	}
	if executablePreserved || !created {
		return printSalmonReinstallHint(output)
	}
	return nil
}

// installSalmonExecutable copies an executable launched from a user or other
// non-system directory to a stable, system-wide location for the service.
func installSalmonExecutable(output io.Writer, source string, reinstall bool) (string, bool, error) {
	return installSalmonExecutableAt(output, source, salmonExecutablePath, reinstall)
}

func installSalmonExecutableAt(output io.Writer, source, destination string, reinstall bool) (string, bool, error) {
	if isSystemExecutablePath(source) {
		return source, false, nil
	}
	installed, err := setup.InstallExecutable(source, destination, reinstall)
	if err != nil {
		return "", false, err
	}
	if installed {
		action := "Installed"
		if reinstall {
			action = "Reinstalled"
		}
		if _, err := fmt.Fprintf(output, "%s executable at %s\n", action, destination); err != nil {
			return "", false, err
		}
	} else {
		if err := setup.ReportEnsureResult(output, "executable", destination, false); err != nil {
			return "", false, err
		}
	}
	return destination, !installed, nil
}

func printSalmonReinstallHint(output io.Writer) error {
	_, err := fmt.Fprintln(output, "\nRun this setup command again with --reinstall to replace installed files. The configuration will not be overwritten.")
	return err
}

func isSystemExecutablePath(path string) bool {
	path = filepath.Clean(path)
	for _, directory := range []string{"/bin", "/sbin", "/usr/bin", "/usr/sbin", "/usr/local/bin", "/usr/local/sbin"} {
		if filepath.Dir(path) == directory {
			return true
		}
	}
	for _, directory := range []string{"/nix/store", "/opt", "/snap"} {
		if path == directory || strings.HasPrefix(path, directory+string(os.PathSeparator)) {
			return true
		}
	}
	return false
}

// restartSalmonService starts the installed service or restarts it to pick up
// an explicitly reinstalled executable or unit.
func restartSalmonService(output io.Writer) error {
	return restartSalmonServiceWith(output, runCommand)
}

func restartSalmonServiceWith(output io.Writer, run setup.CmdRunner) error {
	if err := run("systemctl", "restart", "salmon.service"); err != nil {
		return fmt.Errorf("start Salmon service: %w\n\nView its status and recent logs with:\n\n    sudo systemctl status salmon.service\n    sudo journalctl --no-pager -u salmon.service -n 50", err)
	}
	_, err := fmt.Fprintln(output, "\nSalmon is configured, installed, and running.")
	return err
}

// runCommand executes a command, forwarding its standard output and error to
// Salmon's own streams.
func runCommand(name string, args ...string) error {
	command := exec.Command(name, args...)
	command.Stdout = os.Stdout
	command.Stderr = os.Stderr
	return command.Run()
}
