#!/usr/bin/env bash
# Build SoulProfile.driver (CI test fixture; see ProfileDevice.cpp) with plain clang++ —
# no Xcode project or CMake needed. Usage: build.sh <out-dir>
#
# Installing it (CI only — it adds fake output devices to the machine):
#   sudo cp -R <out-dir>/SoulProfile.driver /Library/Audio/Plug-Ins/HAL/
#   sudo killall -9 coreaudiod
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
out="${1:?usage: build.sh <out-dir>}"
mkdir -p "$out"
out="$(cd "$out" && pwd)"

# libASPL v3.1.2 (MIT). Pinned by commit; its generated sources are checked in upstream.
LIBASPL_REV=47f688ed6bb637ab8b7f4b36864734b2b1f69b6b
src="$out/libASPL"
if [ ! -d "$src/.git" ]; then
  git clone --quiet https://github.com/gavv/libASPL.git "$src"
fi
git -C "$src" checkout --quiet "$LIBASPL_REV"

bundle="$out/SoulProfile.driver"
rm -rf "$bundle"
mkdir -p "$bundle/Contents/MacOS"
cp "$here/Info.plist" "$bundle/Contents/Info.plist"

# Universal, so the same script serves Apple-silicon and Intel runners.
clang++ -std=c++17 -O2 -Wall -Wextra -Wno-unused-parameter -Wno-invalid-offsetof -Wno-reorder-init-list \
  -arch arm64 -arch x86_64 -mmacosx-version-min=11.0 \
  -bundle -fvisibility=hidden \
  -I "$src/include" \
  "$src"/src/*.cpp "$here/ProfileDevice.cpp" \
  -framework CoreFoundation -framework CoreAudio \
  -o "$bundle/Contents/MacOS/SoulProfile"

# The entry point must be exported despite -fvisibility=hidden.
nm -gU "$bundle/Contents/MacOS/SoulProfile" | grep -q "_SoulProfileEntryPoint" \
  || { echo "entry point not exported" >&2; exit 1; }

# Ad-hoc signature: hosted runners load ad-hoc-signed HAL plug-ins (see the
# restricted-ladder job).
codesign --force --sign - "$bundle"
echo "$bundle"
