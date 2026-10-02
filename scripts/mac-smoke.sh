#!/usr/bin/env bash
# Runtime smoke test of the built macOS app (CI jobs check-macos and build-installer-macos).
# Nobody on the team has a Mac, so this is the only place the real .app is started.
#
# Starts <productName>.app/Contents/MacOS/<CFBundleExecutable> directly with MIRA_LOG=debug,
# without user interaction and without needing `claude` (setup only logs "claude not found").
# Hard checks (any FEJL makes the script exit 1):
#   a) the process is alive after start
#   b) ~/Library/Logs/<bundle id>/mira-bots.log exists and has "pipe server listening on <path>"
#      for this pid (pipe/server.rs; log dir = tauri app_log_dir on macOS)
#   c) the socket exists at $TMPDIR/mira-bots-<uid>/mira-bots-<pid>.sock or the /tmp fallback
#      /tmp/mira-bots-<uid>/<pid>.sock (pipe/unix_socket.rs), file 0600, directory 0700, ours
#   d) no panic: the emergency file $TMPDIR/mira-bots-panic.log (lib.rs) gets no new content
#      and the log has no "panic:" line (start to exit)
#   e) quit: the quit Apple Event (what logout and Activity Monitor's Quit send) ends the app
#      within 10 s, exit 0. If TCC refuses the Apple Event (-1743 "Not authorized" in quit.txt),
#      that is only ADVARSEL and SIGTERM is the hard quit instead (exit 0 within 10 s required).
#   f) cleanup: the socket file is gone after exit (RunEvent::Exit -> cleanup_registered)
# Soft checks (reported, never fatal): windows of the process (CGWindowList through JXA),
# screenshots (need Screen Recording rights; may be black or wallpaper only), hook/mcp exe and
# claude found, ERROR lines in the log, socket directory removed.
#
# SIGTERM: the app handles SIGTERM/SIGINT/SIGHUP itself (platform::signals, review7 W5) and takes
# the same quit path (RunEvent::Exit -> kill_all + cleanup_registered, exit 0). The Apple Event
# is tried first because it is what macOS itself sends; SIGTERM is the deterministic fallback
# when TCC refuses Apple Events from osascript on the runner. When the Apple Event was delivered
# but the app did not quit, e fails even if SIGTERM then ends it (SIGKILL is the last resort).
#
# Output in smoke/ (MIRA_SMOKE_OUT): summary.txt, mira-bots.log, app-output.txt, windows.txt,
# screen.png, window-<id>.png, panic log if any.
set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "mac-smoke: kører kun på macOS (uname: $(uname -s))" >&2
  exit 2
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET="${MIRA_SMOKE_TARGET:-aarch64-apple-darwin}"
OUT="${MIRA_SMOKE_OUT:-$ROOT/smoke}"
START_TIMEOUT="${MIRA_SMOKE_START_TIMEOUT:-30}"
QUIT_TIMEOUT="${MIRA_SMOKE_QUIT_TIMEOUT:-10}"
# config.rs LOG_FILE_STEM; the file is <stem>.log, rotated to <stem>_<date>.log on the next start.
LOG_STEM="mira-bots"
# lib.rs EMERGENCY_LOG_FILE, in std::env::temp_dir().
PANIC_FILE="mira-bots-panic.log"

rm -rf "$OUT"
mkdir -p "$OUT"
SUMMARY="$OUT/summary.txt"
CHECKS="$OUT/.checks"
: >"$CHECKS"
HARD_FAILS=0
APP_PID=""
SUMMARY_DONE=0

# check <hård|blød> <OK|FEJL|ADVARSEL|INFO> <text>
check() {
  printf '%-8s [%s] %s\n' "$2" "$1" "$3" >>"$CHECKS"
  if [[ "$1" == "hård" && "$2" == "FEJL" ]]; then
    HARD_FAILS=$((HARD_FAILS + 1))
  fi
}

alive() {
  [[ -n "$APP_PID" ]] && kill -0 "$APP_PID" 2>/dev/null
}

# Polls `$1` (a command) every 0.25 s for up to `$2` seconds; true as soon as it succeeds.
wait_for() {
  local tries=$(($2 * 4))
  while ((tries > 0)); do
    if "$1"; then
      return 0
    fi
    sleep 0.25
    tries=$((tries - 1))
  done
  "$1"
}

# Octal permission bits / owner uid (BSD stat).
perm() { stat -f %Lp "$1"; }
owner() { stat -f %u "$1"; }

file_size() {
  if [[ -f "$1" ]]; then
    wc -c <"$1" | tr -d ' '
  else
    echo 0
  fi
}

write_summary() {
  [[ "$SUMMARY_DONE" == 1 ]] && return 0
  SUMMARY_DONE=1
  {
    echo "mira-bots macOS-røgtest ($(date '+%Y-%m-%d %H:%M:%S'))"
    echo "macOS: $(sw_vers -productVersion 2>/dev/null || echo ukendt), $(uname -m)"
    echo "app: ${APP:-?}"
    echo "bundle id: ${BUNDLE_ID:-?}, binær: ${EXE:-?}, pid: ${APP_PID:-?}"
    echo "log: ${LOG:-?}"
    echo "socket: ${SOCK:-?}"
    echo
    cat "$CHECKS"
    echo
    if ((HARD_FAILS == 0)); then
      echo "Resultat: OK (alle hårde krav opfyldt)"
    else
      echo "Resultat: FEJL ($HARD_FAILS hårde krav fejlede)"
    fi
  } >"$SUMMARY"
  rm -f "$CHECKS"
  cat "$SUMMARY"
}

on_exit() {
  local code=$?
  # Never leave the app running on the runner.
  if alive; then
    kill -KILL "$APP_PID" 2>/dev/null || true
  fi
  if [[ "$SUMMARY_DONE" != 1 ]]; then
    check hård FEJL "scriptet stoppede uventet (exit $code)"
    write_summary
  fi
}
trap on_exit EXIT

# --- The bundle -----------------------------------------------------------------------------
CONF="$ROOT/src-tauri/tauri.conf.json"
PRODUCT="$(plutil -extract productName raw -o - "$CONF")"
CONF_ID="$(plutil -extract identifier raw -o - "$CONF")"
BUNDLE_DIR="$ROOT/target/$TARGET/release/bundle/macos"
APP="$BUNDLE_DIR/$PRODUCT.app"
if [[ ! -d "$APP" ]]; then
  # shellcheck disable=SC2012 # listing for the report only
  check hård FEJL "bundlen $APP findes ikke (indhold: $(ls "$BUNDLE_DIR" 2>/dev/null | tr '\n' ' '))"
  write_summary
  exit 1
fi
PLIST="$APP/Contents/Info.plist"
EXE="$(plutil -extract CFBundleExecutable raw -o - "$PLIST")"
BUNDLE_ID="$(plutil -extract CFBundleIdentifier raw -o - "$PLIST")"
BIN="$APP/Contents/MacOS/$EXE"
if [[ "$BUNDLE_ID" == "$CONF_ID" ]]; then
  check blød INFO "bundle id $BUNDLE_ID = identifier i tauri.conf.json"
else
  check blød ADVARSEL "bundle id $BUNDLE_ID afviger fra tauri.conf.json ($CONF_ID)"
fi
if [[ ! -x "$BIN" ]]; then
  check hård FEJL "binæren $BIN findes ikke eller er ikke eksekverbar"
  write_summary
  exit 1
fi

LOG_DIR="$HOME/Library/Logs/$BUNDLE_ID"
LOG="$LOG_DIR/$LOG_STEM.log"
UID_NUM="$(id -u)"
# std::env::temp_dir(): $TMPDIR, else the per-user temp dir (newer Rust) or /tmp.
TMP_BASE="${TMPDIR:-}"
if [[ -z "$TMP_BASE" ]]; then
  TMP_BASE="$(getconf DARWIN_USER_TEMP_DIR 2>/dev/null || true)"
fi
TMP_BASE="${TMP_BASE:-/tmp}"
TMP_BASE="${TMP_BASE%/}"
PANIC_LOGS="$TMP_BASE/$PANIC_FILE"
if [[ "$TMP_BASE" != "/tmp" ]]; then
  PANIC_LOGS="$PANIC_LOGS /tmp/$PANIC_FILE"
fi
PANIC_BEFORE=""
for p in $PANIC_LOGS; do
  PANIC_BEFORE="$PANIC_BEFORE $(file_size "$p")"
done

# --- Start ----------------------------------------------------------------------------------
echo "mac-smoke: starter $BIN"
MIRA_LOG=debug "$BIN" >"$OUT/app-output.txt" 2>&1 &
APP_PID=$!
SOCK=""

# The listening line for this pid (the old log of a previous start names another pid).
listen_path() {
  [[ -f "$LOG" ]] || return 1
  grep -F "pipe server listening on " "$LOG" 2>/dev/null |
    sed 's/.*pipe server listening on //' |
    grep -E "/mira-bots-$UID_NUM/(mira-bots-)?$APP_PID\.sock\$" | tail -n 1
}

ready() {
  alive || return 0 # stop waiting; the checks below report the dead process
  [[ -n "$(listen_path || true)" ]] || return 1
  local p
  p="$(listen_path)"
  [[ -S "$p" ]]
}
wait_for ready "$START_TIMEOUT" || true

# a) process
if alive; then
  check hård OK "a) processen kører (pid $APP_PID)"
else
  code=0
  wait "$APP_PID" || code=$?
  check hård FEJL "a) processen døde under opstart (exit $code); se app-output.txt og mira-bots.log"
fi

# b) log file and listening line
if [[ ! -f "$LOG" ]]; then
  # shellcheck disable=SC2012 # listing for the report only
  check hård FEJL "b) logfilen $LOG findes ikke (indhold af ~/Library/Logs: $(ls "$HOME/Library/Logs" 2>/dev/null | tr '\n' ' '))"
else
  SOCK="$(listen_path || true)"
  if [[ -n "$SOCK" ]]; then
    check hård OK "b) logfilen findes og har \"pipe server listening on $SOCK\""
  else
    check hård FEJL "b) logfilen findes, men har ingen \"pipe server listening on …$APP_PID.sock\" (se mira-bots.log)"
  fi
fi

# c) socket file, directory, permissions
if [[ -z "$SOCK" ]]; then
  check hård FEJL "c) ingen socket-sti at tjekke (b fejlede)"
else
  sock_dir="$(dirname "$SOCK")"
  expected_primary="$TMP_BASE/mira-bots-$UID_NUM/mira-bots-$APP_PID.sock"
  expected_fallback="/tmp/mira-bots-$UID_NUM/$APP_PID.sock"
  expected_tmp="/tmp/mira-bots-$UID_NUM/mira-bots-$APP_PID.sock"
  if [[ "$SOCK" == "$expected_fallback" ]]; then
    check blød INFO "socket under reservestien /tmp (stien under \$TMPDIR ville være over 100 tegn)"
  fi
  if [[ "$SOCK" != "$expected_primary" && "$SOCK" != "$expected_fallback" && "$SOCK" != "$expected_tmp" ]]; then
    check hård FEJL "c) socket-stien $SOCK følger ikke skemaet ($expected_primary eller $expected_fallback)"
  elif [[ ! -S "$SOCK" ]]; then
    check hård FEJL "c) $SOCK er ikke en socket"
  else
    fmode="$(perm "$SOCK")"
    dmode="$(perm "$sock_dir")"
    fown="$(owner "$SOCK")"
    down="$(owner "$sock_dir")"
    if [[ "$fmode" == "600" && "$dmode" == "700" && "$fown" == "$UID_NUM" && "$down" == "$UID_NUM" ]]; then
      check hård OK "c) socket $SOCK findes, fil 0$fmode, mappe 0$dmode, ejer uid $UID_NUM"
    else
      check hård FEJL "c) socket-rettigheder: fil 0$fmode (uid $fown), mappe 0$dmode (uid $down); krævet 0600/0700, uid $UID_NUM"
    fi
  fi
fi

# Soft: what setup logged (no claude needed to start).
if [[ -f "$LOG" ]]; then
  for pat in "hook exe: " "mcp exe: " "claude: " "log file: "; do
    line="$(grep -F "$pat" "$LOG" | tail -n 1 | sed -E 's/^(\[[^]]*\])+ ?//' || true)"
    if [[ -n "$line" ]]; then
      check blød INFO "log: $line"
    fi
  done
  if grep -qF "claude not found" "$LOG"; then
    check blød INFO "log: \"claude not found\" (forventet uden claude; appen starter alligevel)"
  fi
  for pat in "mira-hook not found" "mira-mcp not found" "island window not found" "could not place the island"; do
    if grep -qF "$pat" "$LOG"; then
      check blød ADVARSEL "log: \"$pat\""
    fi
  done
fi

# Soft: windows of the process. Bounds/ids need no Screen Recording right (window titles do).
WINDOW_IDS=""
if alive; then
  if osascript -l JavaScript -e '
ObjC.import("CoreGraphics");
function run(argv) {
  var pid = parseInt(argv[0], 10);
  var list = ObjC.deepUnwrap(ObjC.castRefToObject($.CGWindowListCopyWindowInfo(0, 0))) || [];
  var out = [];
  list.forEach(function (w) {
    if (w.kCGWindowOwnerPID !== pid) return;
    var b = w.kCGWindowBounds || {};
    out.push([w.kCGWindowNumber, w.kCGWindowLayer, w.kCGWindowIsOnscreen ? 1 : 0,
      b.X, b.Y, b.Width, b.Height].join(" "));
  });
  return out.join("\n");
}' "$APP_PID" >"$OUT/windows.txt" 2>&1; then
    on_screen="$(awk '$3 == 1' "$OUT/windows.txt" | wc -l | tr -d ' ')"
    total="$(grep -cE '^[0-9]+ ' "$OUT/windows.txt" || true)"
    if ((on_screen > 0)); then
      check blød OK "vinduer: $total i alt, $on_screen på skærmen (id lag synlig x y b h i windows.txt)"
    else
      check blød ADVARSEL "vinduer: $total i alt, ingen på skærmen (windows.txt)"
    fi
    WINDOW_IDS="$(awk '$3 == 1 {print $1}' "$OUT/windows.txt" | head -n 3)"
  else
    check blød ADVARSEL "vinduer: CGWindowList via osascript fejlede (windows.txt)"
  fi
fi

# Soft: screenshots (without Screen Recording rights macOS gives a black/wallpaper-only image
# or refuses).
sleep 1
if screencapture -x -t png "$OUT/screen.png" >/dev/null 2>&1 && [[ -s "$OUT/screen.png" ]]; then
  check blød OK "screenshot: screen.png ($(file_size "$OUT/screen.png") bytes; kan være sort uden Screen Recording-rettighed)"
else
  check blød ADVARSEL "screenshot: screencapture fejlede eller gav en tom fil"
fi
for wid in $WINDOW_IDS; do
  if screencapture -x -o -t png -l"$wid" "$OUT/window-$wid.png" >/dev/null 2>&1 && [[ -s "$OUT/window-$wid.png" ]]; then
    check blød OK "screenshot af vindue $wid: window-$wid.png"
  else
    check blød ADVARSEL "screenshot af vindue $wid fejlede"
  fi
done

# --- Quit -----------------------------------------------------------------------------------
not_alive() { ! alive; }
# TCC (Automation) refused the Apple Event: osascript reports -1743 / "Not authorized to send
# Apple events". Then the event never reached the app and says nothing about it.
tcc_refused() { grep -qiE -- '-1743|not authori[sz]ed' "$OUT/quit.txt" 2>/dev/null; }
EXIT_CODE=""
QUIT_VIA=""
if alive; then
  # The quit Apple Event to exactly this pid (NSRunningApplication.terminate()); the bundle id
  # target is the fallback. Both run in the background so a hanging osascript cannot block.
  quit_how="Apple Event (NSRunningApplication.terminate())"
  osascript -l JavaScript -e '
ObjC.import("AppKit");
function run(argv) {
  var app = $.NSRunningApplication.runningApplicationWithProcessIdentifier(parseInt(argv[0], 10));
  if (!app || app.isNil()) return "ingen NSRunningApplication for pid " + argv[0];
  return app.terminate() ? "terminate() sendt" : "terminate() afvist";
}' "$APP_PID" >"$OUT/quit.txt" 2>&1 &
  osa_pid=$!
  if ! wait_for not_alive 4; then
    quit_how="Apple Event (tell application id \"$BUNDLE_ID\" to quit)"
    osascript -e "tell application id \"$BUNDLE_ID\" to quit" >>"$OUT/quit.txt" 2>&1 &
    osa_pid="$osa_pid $!"
    wait_for not_alive $((QUIT_TIMEOUT - 4)) || true
  fi
  for p in $osa_pid; do
    kill "$p" 2>/dev/null || true
  done
  QUIT_VIA="apple"
  if alive; then
    if tcc_refused; then
      QUIT_VIA="sigterm"
      quit_how="SIGTERM (Apple Event afvist af TCC; se quit.txt)"
    else
      QUIT_VIA="failed"
      quit_how="SIGTERM til oprydning (Apple Event virkede ikke inden $QUIT_TIMEOUT s; se quit.txt)"
    fi
    kill -TERM "$APP_PID" 2>/dev/null || true
    if ! wait_for not_alive "$QUIT_TIMEOUT"; then
      kill -KILL "$APP_PID" 2>/dev/null || true
      QUIT_VIA="failed"
      quit_how="SIGKILL (hverken Apple Event eller SIGTERM afsluttede inden $QUIT_TIMEOUT s)"
      wait_for not_alive 5 || true
    fi
  fi
  EXIT_CODE=0
  wait "$APP_PID" 2>/dev/null || EXIT_CODE=$?
  if [[ "$QUIT_VIA" == sigterm ]]; then
    check blød ADVARSEL "e) Apple Event afvist af TCC (-1743/Not authorized i quit.txt); SIGTERM brugt som afslutning"
  fi
  if [[ "$QUIT_VIA" != failed && "$EXIT_CODE" == 0 ]]; then
    check hård OK "e) afsluttet via $quit_how inden $QUIT_TIMEOUT s, exit 0"
  else
    check hård FEJL "e) afslutning: $quit_how, exit $EXIT_CODE (krævet: Apple Event, eller SIGTERM når TCC afviser Apple Events; exit 0)"
  fi
else
  check hård FEJL "e) afslutning ikke testet (processen kørte ikke)"
fi

# f) cleanup
if [[ -n "$SOCK" ]]; then
  if [[ -e "$SOCK" ]]; then
    check hård FEJL "f) socket-filen $SOCK ligger der stadig efter afslutning (${quit_how:-ingen afslutning})"
  else
    check hård OK "f) socket-filen er fjernet efter afslutning (${quit_how:-ingen afslutning})"
  fi
  if [[ -e "$(dirname "$SOCK")" ]]; then
    check blød INFO "socket-mappen $(dirname "$SOCK") findes stadig (fjernes kun når den er tom)"
  else
    check blød OK "socket-mappen er fjernet"
  fi
else
  check hård FEJL "f) oprydning kan ikke tjekkes (ingen socket-sti)"
fi

# d) panic (checked last so it also covers the shutdown)
panic_new=""
i=0
for p in $PANIC_LOGS; do
  i=$((i + 1))
  before="$(echo "$PANIC_BEFORE" | awk -v n="$i" '{print $n}')"
  if [[ "$(file_size "$p")" != "$before" ]]; then
    panic_new="$panic_new $p"
    cp "$p" "$OUT/" 2>/dev/null || true
  fi
done
log_panics=0
if [[ -f "$LOG" ]]; then
  log_panics="$(grep -c "panic:" "$LOG" || true)"
fi
if [[ -z "$panic_new" && "$log_panics" == 0 ]]; then
  check hård OK "d) ingen panic (nødloggen $TMP_BASE/$PANIC_FILE uændret, ingen \"panic:\" i loggen)"
else
  check hård FEJL "d) panic: ny tekst i [${panic_new# }], $log_panics \"panic:\"-linjer i loggen"
fi

# Soft: ERROR lines.
if [[ -f "$LOG" ]]; then
  cp "$LOG" "$OUT/mira-bots.log"
  errors="$(grep -c "\]\[ERROR\]" "$LOG" || true)"
  if [[ "$errors" == 0 ]]; then
    check blød OK "ingen ERROR-linjer i loggen"
  else
    check blød ADVARSEL "$errors ERROR-linjer i loggen:"
    grep "\]\[ERROR\]" "$LOG" | head -n 5 | sed 's/^/           /' >>"$CHECKS"
  fi
fi

write_summary
if ((HARD_FAILS > 0)); then
  if [[ -f "$LOG" ]]; then
    echo "--- sidste 40 linjer af $LOG ---"
    tail -n 40 "$LOG"
  fi
  exit 1
fi
