[Desktop Entry]
Type=Application
Name=Salmon Watch Legacy
Comment=Show Salmon status in the desktop tray
Icon=salmon-watch-legacy
Exec={{ desktopExecArgument .Executable }} --config {{ desktopExecArgument .ConfigFilename }}
Terminal=false
