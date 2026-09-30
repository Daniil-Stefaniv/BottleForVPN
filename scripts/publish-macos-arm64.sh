#!/bin/sh
set -eu

project_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
output_dir="$project_root/artifacts/macos-arm64"
helper_output_dir="$project_root/artifacts/macos-arm64-helper"
release_dir="$project_root/artifacts/release"
app_dir="$project_root/artifacts/staging/macos/BottleForApps.app"
pkg_scripts="$project_root/artifacts/staging/pkgscripts"

dotnet publish "$project_root/src/BottleForApps/BottleForApps.csproj" \
  --configuration Release \
  --runtime osx-arm64 \
  --self-contained true \
  -p:NuGetAudit=false \
  -p:PublishSingleFile=true \
  -p:DebugType=None \
  --output "$output_dir"

dotnet publish "$project_root/src/BottleRouteHelper/BottleRouteHelper.csproj" \
  --configuration Release \
  --runtime osx-arm64 \
  --self-contained true \
  -p:NuGetAudit=false \
  -p:PublishSingleFile=true \
  -p:DebugType=None \
  --output "$helper_output_dir"

app_version=$(sed -n 's:.*<Version>\(.*\)</Version>.*:\1:p' "$project_root/src/BottleForApps/BottleForApps.csproj" | head -1)
[ -n "$app_version" ] || app_version="0.0.0"

rm -rf "$app_dir" "$pkg_scripts"
mkdir -p "$app_dir/Contents/MacOS/Extras" "$app_dir/Contents/Resources" "$release_dir" "$pkg_scripts"
cp "$project_root/installer/macos/Info.plist" "$app_dir/Contents/Info.plist"
perl -0pi -e "s|(<key>CFBundleShortVersionString</key>\s*<string>)[^<]*(</string>)|\${1}$app_version\${2}|; s|(<key>CFBundleVersion</key>\s*<string>)[^<]*(</string>)|\${1}$app_version\${2}|" "$app_dir/Contents/Info.plist"
cp "$project_root/installer/icons/BottleForApps.icns" "$app_dir/Contents/Resources/BottleForApps.icns"
cp -R "$output_dir/." "$app_dir/Contents/MacOS/"
cp "$helper_output_dir/bottle-route-helper" "$app_dir/Contents/MacOS/Extras/"
chmod +x "$app_dir/Contents/MacOS/bottle" "$app_dir/Contents/MacOS/Extras/bottle-route-helper"
xattr -cr "$app_dir"
codesign --deep --force --sign - "$app_dir"

cp "$project_root/installer/macos/preinstall" "$pkg_scripts/"
cp "$project_root/installer/macos/postinstall" "$pkg_scripts/"
cp "$project_root/installer/macos/local.bottleforapps.route-helper.plist" "$pkg_scripts/"
cp "$helper_output_dir/bottle-route-helper" "$pkg_scripts/"
chmod +x "$pkg_scripts/preinstall" "$pkg_scripts/postinstall" "$pkg_scripts/bottle-route-helper"

pkgbuild --component "$app_dir" \
  --scripts "$pkg_scripts" \
  --install-location /Applications \
  "$release_dir/BottleForApps-macOS-arm64.pkg"
printf 'Installer package: %s\n' "$release_dir/BottleForApps-macOS-arm64.pkg"
