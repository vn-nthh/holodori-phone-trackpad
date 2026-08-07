@echo off
cd /d "%~dp0Windows"
holodori-native-host.exe --mode touch --metrics
pause
