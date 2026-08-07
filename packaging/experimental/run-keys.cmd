@echo off
cd /d "%~dp0Windows"
holodori-native-host.exe --mode keys --lanes s,d,f,j,k,l --metrics
pause
