#!/bin/sh
# Runs inside the image as user `computer`. Fails when any agreed tool is missing.
set -u
failed=0
check() {
  if ! "$@" >/dev/null 2>&1; then
    echo "missing or broken: $*" >&2
    failed=1
  fi
}
for tool in python3 pip3 uv uvx git gh curl jq aws node npm rg ruby gem fish gcc make \
  ps less file zip xz nano convert fluxbox sudo xsel pgrep; do
  check "$tool" --version
done
check ssh -V
check unzip -v
check xclip -version
check xdpyinfo -version
check xterm -version
check chromium --version
check soffice --version
check xdg-open --version
check Xvnc -help
check python3 -m venv --help
check sudo -n true
[ "$(getent passwd computer | cut -d: -f7)" = /bin/bash ] || { echo "default shell is not bash" >&2; failed=1; }
case "$(bash -lc 'echo $PATH')" in
  /home/computer/.local/bin:*) ;;
  *) echo "~/.local/bin is not first on PATH" >&2; failed=1 ;;
esac
[ "$(fc-match -f '%{family}' 'Noto Color Emoji')" = "Noto Color Emoji" ] || { echo "emoji font missing" >&2; failed=1; }
fc-list ':lang=ja' family | grep -q CJK || { echo "CJK font missing" >&2; failed=1; }
office_dir=$(mktemp -d)
printf 'name,count
widgets,3
' > "$office_dir/sample.csv"
soffice -env:UserInstallation="file://$office_dir/profile" --headless --convert-to pdf --outdir "$office_dir" "$office_dir/sample.csv" >/dev/null 2>&1
head -c 4 "$office_dir/sample.pdf" 2>/dev/null | grep -q '%PDF' || { echo "soffice cannot convert a document to PDF" >&2; failed=1; }
rm -rf "$office_dir"
exit "$failed"
