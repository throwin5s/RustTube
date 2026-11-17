#!/usr/bin/env bash
# Measure cold click -> first decoded frame with a fresh RustTube process per run.
#
# The in-memory stream cache and the app's hover prefetch are per-process, so
# each run starts the built binary again. The fixed search query makes the
# autoplay target deterministic (dQw4w9WgXcQ).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

RUNS=5
MODE=direct
BUILD=1
QUERY="Rick Astley Never Gonna Give You Up"
VIDEO_ID="dQw4w9WgXcQ"

usage() {
    cat <<'EOF'
Usage: scripts/cold-start.sh [options]

Options:
  -n, --runs N     number of fresh-process runs (default: 5)
      --fallback   force the yt-dlp fallback (current fast client)
      --legacy     force the pre-InnerTube yt-dlp client path, for the old
                   ~3.2 s before-style measurement
      --no-build   use the existing target/debug/rusttube binary
  -h, --help       show this help
EOF
}

while (($#)); do
    case "$1" in
        -n|--runs)
            RUNS="${2:?--runs needs a number}"
            shift 2
            ;;
        --fallback)
            MODE=fallback
            shift
            ;;
        --legacy)
            MODE=legacy
            shift
            ;;
        --no-build)
            BUILD=0
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "Unknown option: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

case "$RUNS" in
    ''|*[!0-9]*|0)
        echo "--runs must be a positive integer" >&2
        exit 2
        ;;
esac

if ((BUILD)); then
    cargo build --quiet
fi

BIN="$ROOT/target/debug/rusttube"
if [[ ! -x "$BIN" ]]; then
    echo "Missing $BIN; run without --no-build first." >&2
    exit 1
fi

EXTRA_ENV=()
case "$MODE" in
    fallback)
        EXTRA_ENV+=(RUSTTUBE_DISABLE_INNERTUBE=1)
        ;;
    legacy)
        EXTRA_ENV+=(RUSTTUBE_DISABLE_INNERTUBE=1 RUSTTUBE_LEGACY_YTDLP=1)
        ;;
esac

LOGS="$(mktemp -d "${TMPDIR:-/tmp}/rusttube-cold-start.XXXXXX")"
TIMES="$LOGS/times.txt"
: >"$TIMES"

cleanup() {
    if [[ -n "${APP_PID:-}" ]] && kill -0 "$APP_PID" 2>/dev/null; then
        kill "$APP_PID" 2>/dev/null || true
        wait "$APP_PID" 2>/dev/null || true
    fi
}
trap cleanup EXIT

echo "RustTube cold start: mode=$MODE runs=$RUNS video=$VIDEO_ID"
echo "Each run launches a fresh process; click -> first frame is read from the app log."
echo

for ((run = 1; run <= RUNS; run++)); do
    log="$LOGS/run-$run.log"
    rm -f "$log"

    if ((${#EXTRA_ENV[@]})); then
        env RUSTTUBE_TIMING=1 \
            RUSTTUBE_AUTOPLAY=1 \
            "RUSTTUBE_SHOT_QUERY=$QUERY" \
            "${EXTRA_ENV[@]}" \
            "$BIN" >"$log" 2>&1 &
    else
        env RUSTTUBE_TIMING=1 \
            RUSTTUBE_AUTOPLAY=1 \
            "RUSTTUBE_SHOT_QUERY=$QUERY" \
            "$BIN" >"$log" 2>&1 &
    fi
    APP_PID=$!

    found=0
    for ((tick = 0; tick < 450; tick++)); do
        if grep -q "click → first frame for $VIDEO_ID" "$log" 2>/dev/null; then
            found=1
            break
        fi
        if ! kill -0 "$APP_PID" 2>/dev/null; then
            break
        fi
        sleep 0.1
    done

    if ((!found)); then
        echo "Run $run did not reach first frame. Log:" >&2
        cat "$log" >&2
        exit 1
    fi

    line="$(grep "click → first frame for $VIDEO_ID" "$log" | tail -1)"
    value="$(sed -E 's/.*: ([0-9]+(\.[0-9]+)?)s$/\1/' <<<"$line")"
    if [[ ! "$value" =~ ^[0-9]+(\.[0-9]+)?$ ]]; then
        echo "Could not parse timing from: $line" >&2
        cat "$log" >&2
        exit 1
    fi
    printf '%s\n' "$value" >>"$TIMES"
    printf 'run %d/%d: %ss\n' "$run" "$RUNS" "$value"

    kill "$APP_PID" 2>/dev/null || true
    wait "$APP_PID" 2>/dev/null || true
    unset APP_PID
done

echo
echo "Runs (seconds): $(paste -sd' ' "$TIMES")"
python3 - "$TIMES" "$MODE" <<'PY'
import statistics
import sys

path, mode = sys.argv[1], sys.argv[2]
values = [float(line) for line in open(path) if line.strip()]
print(f"{mode}: mean {statistics.mean(values):.2f}s, "
      f"median {statistics.median(values):.2f}s, "
      f"min {min(values):.2f}s, max {max(values):.2f}s")
PY
echo "Logs: $LOGS"
