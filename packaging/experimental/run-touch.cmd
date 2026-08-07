@echo off
cd /d "%~dp0Windows"
holodori-native-host.exe --mode touch --udp-port 42825 --metrics
pause
