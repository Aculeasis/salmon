package main

import (
	"fmt"
	"os"
	"strings"

	"github.com/spf13/cobra"

	"github.com/dimonomid/salmon/internal/setup"
	"github.com/dimonomid/salmon/logs"
	"github.com/dimonomid/salmon/version"
)

// newRootCommand constructs the Salmon command-line interface.
func newRootCommand() *cobra.Command {
	var configFilename string
	var logLevel string
	var reinstall bool
	root := &cobra.Command{
		Use:          "salmon",
		Short:        "Monitor system health and publish its status",
		Version:      version.FullDescription("Salmon"),
		SilenceUsage: true,
		Args:         cobra.NoArgs,
		RunE: func(_ *cobra.Command, _ []string) error {
			minLogLevel, err := logs.ParseLogLevel(logLevel)
			if err != nil {
				return err
			}
			return runSalmon(configFilename, minLogLevel)
		},
	}
	root.SetVersionTemplate("{{.Version}}")
	root.PersistentFlags().StringVar(&configFilename, "config", defaultSalmonConfig, "Config filename")
	root.Flags().StringVar(&logLevel, "log-level", "info", "Minimum log level (debug, info, warning, or error)")

	setupCommand := &cobra.Command{
		Use:   "setup",
		Short: "Perform the complete setup",
		Long:  "Perform the complete setup by installing the executable when needed, creating the default configuration and service account, then installing the systemd service. Run a setup subcommand to perform only one of these operations.",
		Args:  cobra.NoArgs,
		RunE: func(cmd *cobra.Command, _ []string) error {
			if err := requireRootForSalmonSetup(cmd, "", configFilename, reinstall); err != nil {
				return err
			}
			if err := initializeSalmonConfig(cmd.OutOrStdout(), configFilename); err != nil {
				return err
			}
			if err := createSalmonUser(cmd.OutOrStdout()); err != nil {
				return err
			}
			if err := installSalmonService(cmd.OutOrStdout(), configFilename, reinstall); err != nil {
				return err
			}
			return printSalmonStartHint(cmd.OutOrStdout(), reinstall)
		},
	}
	setupCommand.PersistentFlags().BoolVar(&reinstall, "reinstall", false, "Replace the installed executable and systemd service")
	setupCommand.AddCommand(
		&cobra.Command{
			Use:   "create-config",
			Short: "Create the default configuration if it does not exist",
			Args:  cobra.NoArgs,
			RunE: func(cmd *cobra.Command, _ []string) error {
				if configFilename == defaultSalmonConfig {
					if err := requireRootForSalmonSetup(cmd, "create-config", configFilename, reinstall); err != nil {
						return err
					}
				}
				return initializeSalmonConfig(cmd.OutOrStdout(), configFilename)
			},
		},
		&cobra.Command{
			Use:   "create-user",
			Short: "Create the system user and group used by the Salmon service",
			Args:  cobra.NoArgs,
			RunE: func(cmd *cobra.Command, _ []string) error {
				if err := requireRootForSalmonSetup(cmd, "create-user", configFilename, reinstall); err != nil {
					return err
				}
				return createSalmonUser(cmd.OutOrStdout())
			},
		},
		&cobra.Command{
			Use:   "install-service",
			Short: "Install the executable and systemd service, then enable it",
			Args:  cobra.NoArgs,
			RunE: func(cmd *cobra.Command, _ []string) error {
				if err := requireRootForSalmonSetup(cmd, "install-service", configFilename, reinstall); err != nil {
					return err
				}
				return installSalmonService(cmd.OutOrStdout(), configFilename, reinstall)
			},
		},
	)

	root.AddCommand(setupCommand)
	return root
}

func requireRootForSalmonSetup(cmd *cobra.Command, operation, configFilename string, reinstall bool) error {
	arguments := []string{setup.ShellArgument(os.Args[0]), "setup"}
	if operation != "" {
		arguments = append(arguments, operation)
	}
	if reinstall {
		arguments = append(arguments, "--reinstall")
	}
	if flag := cmd.Flag("config"); flag != nil && flag.Changed {
		arguments = append(arguments, "--config", setup.ShellArgument(configFilename))
	}
	return salmonSetupRootError(os.Geteuid(), strings.Join(arguments, " "))
}

func salmonSetupRootError(effectiveUserID int, invocation string) error {
	if effectiveUserID == 0 {
		return nil
	}
	return fmt.Errorf("this setup operation requires root privileges\n\nHint: Rerun it with sudo:\n\n    sudo %s", invocation)
}
