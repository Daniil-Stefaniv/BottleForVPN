#!/bin/sh
set -eu

project_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
artifacts="$project_root/artifacts"
release="$artifacts/release"
mac_publish="$artifacts/macos-arm64"
mac_helper_publish="$artifacts/macos-arm64-helper"
win_publish="$artifacts/win-x64"
win_helper_publish="$artifacts/win-x64-helper"
stage="$artifacts/staging"

rm -rf "$release" "$stage" "$mac_publish" "$mac_helper_publish" "$win_publish" "$win_helper_publish"
mkdir -p "$release" "$stage"

dotnet publish "$project_root/src/BottleForApps/BottleForApps.csproj" \
  --configuration Release --runtime osx-arm64 --self-contained true \
  -p:NuGetAudit=false -p:PublishSingleFile=true -p:DebugType=None --output "$mac_publish"

dotnet publish "$project_root/src/BottleRouteHelper/BottleRouteHelper.csproj" \
  --configuration Release --runtime osx-arm64 --self-contained true \
  -p:NuGetAudit=false -p:PublishSingleFile=true -p:DebugType=None --output "$mac_helper_publish"

dotnet publish "$project_root/src/BottleForApps/BottleForApps.csproj" \
  --configuration Release --runtime win-x64 --self-contained true \
  -p:NuGetAudit=false -p:PublishSingleFile=true -p:DebugType=None --output "$win_publish"

dotnet publish "$project_root/src/BottleRouteHelper/BottleRouteHelper.csproj" \
  --configuration Release --runtime win-x64 --self-contained true \
  -p:NuGetAudit=false -p:PublishSingleFile=true -p:DebugType=None --output "$win_helper_publish"

# macOS: .app с приложением, иконкой и помощником в Extras.
app_version=$(sed -n 's:.*<Version>\(.*\)</Version>.*:\1:p' "$project_root/src/BottleForApps/BottleForApps.csproj" | head -1)
[ -n "$app_version" ] || app_version="0.0.0"

mac_app="$stage/macos/BottleForApps.app"
mkdir -p "$mac_app/Contents/MacOS/Extras" "$mac_app/Contents/Resources"
cp "$project_root/installer/macos/Info.plist" "$mac_app/Contents/Info.plist"
perl -0pi -e "s|(<key>CFBundleShortVersionString</key>\s*<string>)[^<]*(</string>)|\${1}$app_version\${2}|; s|(<key>CFBundleVersion</key>\s*<string>)[^<]*(</string>)|\${1}$app_version\${2}|" "$mac_app/Contents/Info.plist"
cp "$project_root/installer/icons/BottleForApps.icns" "$mac_app/Contents/Resources/BottleForApps.icns"
cp -R "$mac_publish/." "$mac_app/Contents/MacOS/"
cp "$mac_helper_publish/bottle-route-helper" "$mac_app/Contents/MacOS/Extras/"
chmod +x "$mac_app/Contents/MacOS/bottle" "$mac_app/Contents/MacOS/Extras/bottle-route-helper"
xattr -cr "$mac_app"
codesign --deep --force --sign - "$mac_app"

# macOS: ресурсы для скриптов pkg (postinstall ставит службу маршрутов).
pkg_scripts="$stage/pkgscripts"
mkdir -p "$pkg_scripts"
cp "$project_root/installer/macos/preinstall" "$pkg_scripts/"
cp "$project_root/installer/macos/postinstall" "$pkg_scripts/"
cp "$project_root/installer/macos/local.bottleforapps.route-helper.plist" "$pkg_scripts/"
cp "$mac_helper_publish/bottle-route-helper" "$pkg_scripts/"
chmod +x "$pkg_scripts/preinstall" "$pkg_scripts/postinstall" "$pkg_scripts/bottle-route-helper"

pkgbuild --component "$mac_app" \
  --scripts "$pkg_scripts" \
  --install-location /Applications \
  "$release/BottleForApps-macOS-arm64.pkg"

# macOS: DMG с перетаскиванием в Applications.
dmg_src="$stage/dmg"
mkdir -p "$dmg_src"
cp -R "$mac_app" "$dmg_src/"
ln -s /Applications "$dmg_src/Applications"
hdiutil create -volname "BottleForApps $app_version" \
  -srcfolder "$dmg_src" -ov -format UDZO \
  "$release/BottleForApps-macOS-arm64.dmg" >/dev/null

# Windows: приложение и служба-помощник в одном каталоге app.
win_stage="$stage/windows/BottleForApps-Windows-x64"
mkdir -p "$win_stage/app"
cp -R "$win_publish/." "$win_stage/app/"
cp "$win_helper_publish/bottle-route-helper.exe" "$win_stage/app/"
cp "$project_root/installer/windows/Install-BottleForApps.ps1" "$win_stage/"
cp "$project_root/installer/windows/Uninstall-BottleForApps.ps1" "$win_stage/"
cp "$project_root/installer/windows/Install-BottleForApps.bat" "$win_stage/"
cp "$project_root/installer/README-RU.txt" "$win_stage/"
(cd "$stage/windows" && zip -qr "$release/BottleForApps-Windows-x64.zip" "BottleForApps-Windows-x64")

cp "$project_root/installer/README-RU.txt" "$release/README-RU.txt"
(cd "$release" && shasum -a 256 BottleForApps-macOS-arm64.pkg BottleForApps-macOS-arm64.dmg BottleForApps-Windows-x64.zip > SHA256SUMS.txt)
(cd "$release" && zip -qr "$artifacts/BottleForApps-installers.zip" .)

printf 'Release created: %s\n' "$artifacts/BottleForApps-installers.zip"
