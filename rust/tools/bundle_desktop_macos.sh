#!/bin/sh
# Assembles the T3 Code desktop app for macOS: "T3 Code.app" holding the UI program, the server program it
# starts (beside it, where the app looks), the icon and an Info.plist. Nothing is signed or notarised, nothing
# is installed, and nothing is downloaded: it uses the two programs you built and the macOS tools sips,
# iconutil and plutil.
#
#   CARGO_TARGET_DIR=<ui target> cargo build -p t3-ui --no-default-features --features desktop
#   CARGO_TARGET_DIR=<server target> cargo build -p t3-server
#   rust/tools/bundle_desktop_macos.sh --ui <ui target>/debug/t3-ui --server <server target>/debug/t3-server \
#       --out <directory for the app> [--version 0.1.0] [--force]
#
# --out must be an absolute path to a directory that is empty or does not exist yet (or --force: its
# "T3 Code.app" is replaced). Run the result with:  "<out>/T3 Code.app/Contents/MacOS/t3-ui"  or  open "<out>/T3 Code.app"
set -eu

ui=""; server=""; out=""; version="0.1.0"; force=0
while [ $# -gt 0 ]; do
  case "$1" in
    --ui) ui="$2"; shift 2 ;;
    --server) server="$2"; shift 2 ;;
    --out) out="$2"; shift 2 ;;
    --version) version="$2"; shift 2 ;;
    --force) force=1; shift ;;
    -h|--help) sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

[ "$(uname -s)" = Darwin ] || { echo "This script builds the macOS app and needs macOS." >&2; exit 1; }
[ -n "$ui" ] && [ -n "$server" ] && [ -n "$out" ] || { echo "--ui, --server and --out are required (see --help)." >&2; exit 2; }
[ -x "$ui" ] || { echo "The UI program is not an executable file: $ui" >&2; exit 1; }
[ -x "$server" ] || { echo "The server program is not an executable file: $server" >&2; exit 1; }
case "$out" in /*) ;; *) echo "--out must be an absolute path." >&2; exit 2 ;; esac
case "$version" in *[!0-9A-Za-z.+-]*|"") echo "--version may hold only letters, digits and . + -" >&2; exit 2 ;; esac

repo="$(cd "$(dirname "$0")/../.." && pwd)"
icon_source="$repo/assets/prod/black-macos-1024.png"
[ -f "$icon_source" ] || { echo "The icon source is missing: $icon_source" >&2; exit 1; }

app="$out/T3 Code.app"
if [ -e "$app" ]; then
  [ "$force" = 1 ] || { echo "$app exists; use --force to replace it." >&2; exit 1; }
  rm -rf "$app"
fi
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$ui" "$app/Contents/MacOS/t3-ui"
cp "$server" "$app/Contents/MacOS/t3-server"

# The icon, the way the original's build makes it (scripts/build-desktop-artifact.ts): the 1024 px source at
# each size of an iconset, then iconutil.
scratch="$(mktemp -d "${TMPDIR:-/tmp}/t3code-rust-port-test-icon-XXXXXX")"
trap 'rm -rf "$scratch"' EXIT
iconset="$scratch/icon.iconset"
mkdir -p "$iconset"
for size in 16 32 128 256 512; do
  sips -z "$size" "$size" "$icon_source" --out "$iconset/icon_${size}x${size}.png" >/dev/null
  double=$((size * 2))
  sips -z "$double" "$double" "$icon_source" --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o "$app/Contents/Resources/icon.icns"

cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key><string>en</string>
  <key>CFBundleExecutable</key><string>t3-ui</string>
  <key>CFBundleIconFile</key><string>icon</string>
  <key>CFBundleIdentifier</key><string>com.t3tools.t3code.rustport</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>CFBundleName</key><string>T3 Code</string>
  <key>CFBundleDisplayName</key><string>T3 Code</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
</dict>
</plist>
PLIST
plutil -lint "$app/Contents/Info.plist" >/dev/null

echo "Built $app"
echo "Not signed or notarised. Run it with: \"$app/Contents/MacOS/t3-ui\""
