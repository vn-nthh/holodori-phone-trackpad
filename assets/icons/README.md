# App icons

`android.png` and `pc.png` are the original supplied Doritrack logos.
Android uses density-specific launcher icons plus an adaptive icon with a dark
background. The adaptive XML insets the foreground by 15% on each side to keep
the phone inside circular masks.
The PC launcher uses a multi-size Windows ICO and a PNG for other desktop targets.

To regenerate with the launcher dependencies installed, run from the repository
root in PowerShell:

```powershell
$Tauri = '.\tauri-launcher\node_modules\.bin\tauri.cmd'
& $Tauri icon assets/icons/pc.png --output build/logo-assets/pc
& $Tauri icon assets/icons/android.json --output build/logo-assets/phone
Copy-Item build/logo-assets/pc/icon.ico tauri-launcher/src-tauri/icons/icon.ico
Copy-Item build/logo-assets/pc/icon.png tauri-launcher/src-tauri/icons/icon.png
Get-ChildItem build/logo-assets/phone/android -Directory |
    Where-Object Name -ne 'mipmap-anydpi-v26' |
    Copy-Item -Destination android-app/app/src/main/res -Recurse -Force
& $Tauri icon assets/icons/android.png --output build/logo-assets/phone/hdpi --png 72
& $Tauri icon build/logo-assets/phone/android/mipmap-xxxhdpi/ic_launcher_round.png --output build/logo-assets/phone/hdpi-round --png 72
Copy-Item build/logo-assets/phone/hdpi/72x72.png android-app/app/src/main/res/mipmap-hdpi/ic_launcher.png
Copy-Item build/logo-assets/phone/hdpi-round/72x72.png android-app/app/src/main/res/mipmap-hdpi/ic_launcher_round.png
```

Keep the generated Android resources and the two desktop icons in version
control so ordinary app builds do not need an icon-generation step.
Preserve the hand-authored adaptive XML when regenerating. The explicit 72px
outputs correct this CLI version's 49px legacy hdpi icons.
