#!/usr/bin/env bash
#
# Fetch the yt-dlp/ejs benchmark bundle from lucid-softworks/lumen#12: the ejs core solver
# (meriyah + astring) with a YouTube player and two n-challenges embedded, printing the solved
# results as JSON. ~3 MB of JavaScript.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"

URL="https://github.com/user-attachments/files/29786020/code.js"
if [ ! -f code.js ]; then
  echo "Downloading ejs benchmark bundle (issue #12 reproducer) ..."
  curl -fsSL "$URL" -o code.js
fi
echo "$(wc -c < code.js | tr -d ' ') bytes in code.js"
