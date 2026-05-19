#!/usr/bin/env bash
set -u

PID_FILE="${PID_FILE:-$HOME/.config/claude-proxy/claude-proxy.pid}"
CONFIG_LOG="${CONFIG_LOG:-$HOME/.config/claude-proxy/claude-proxy.log}"
TRACING_LOG="${TRACING_LOG:-$HOME/.config/claude-proxy/logs/claude-proxy.log}"
OUT_DIR="${OUT_DIR:-/tmp/claude-proxy-stop-diagnostics-$(date +%Y%m%d-%H%M%S)}"
STOP_CMD="${STOP_CMD:-claude-proxy server stop}"
STRACE_SECONDS="${STRACE_SECONDS:-8}"

RUN_STOP=1
RUN_STRACE=0

usage() {
    cat <<EOF
Usage: $0 [--no-stop] [--strace]

Collect claude-proxy stop diagnostics into a timestamped directory.

Options:
  --no-stop   Collect snapshots without running: $STOP_CMD
  --strace    Attach strace while running stop, if strace is available

Environment:
  OUT_DIR=/path/to/output
  PID_FILE=/path/to/claude-proxy.pid
  STOP_CMD='claude-proxy server stop'
EOF
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --no-stop)
            RUN_STOP=0
            ;;
        --strace)
            RUN_STRACE=1
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "Unknown argument: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
    shift
done

mkdir -p "$OUT_DIR"

run_capture() {
    local output="$1"
    shift
    {
        echo "\$ $*"
        echo
        "$@"
    } >"$output" 2>&1
}

append_section() {
    local output="$1"
    local title="$2"
    shift 2
    {
        echo
        echo "===== $title ====="
        echo "\$ $*"
        "$@"
    } >>"$output" 2>&1
}

current_pid() {
    if [ -r "$PID_FILE" ]; then
        tr -d '[:space:]' <"$PID_FILE"
    fi
}

collect_snapshot() {
    local label="$1"
    local pid="$2"
    local file="$OUT_DIR/${label}.txt"

    {
        echo "label=$label"
        echo "time=$(date --iso-8601=seconds)"
        echo "pid_file=$PID_FILE"
        echo "pid_from_start=$pid"
        echo "pid_file_current=$(current_pid || true)"
        echo "uname=$(uname -a)"
        echo
    } >"$file"

    if [ -z "$pid" ]; then
        echo "No PID available." >>"$file"
        return
    fi

    append_section "$file" "ps process" ps -p "$pid" -o pid,ppid,stat,wchan:40,lstart,etime,cmd
    append_section "$file" "ps threads" ps -L -p "$pid" -o pid,tid,stat,wchan:32,comm

    if [ -r "/proc/$pid/status" ]; then
        append_section "$file" "proc status" sed -n '1,120p' "/proc/$pid/status"
    else
        echo "No /proc/$pid/status; process may have exited." >>"$file"
    fi

    if [ -r "/proc/$pid/wchan" ]; then
        append_section "$file" "proc wchan" cat "/proc/$pid/wchan"
    fi

    if command -v lsof >/dev/null 2>&1; then
        append_section "$file" "lsof" lsof -nP -p "$pid"
    else
        echo "lsof not found." >>"$file"
    fi

    if command -v ss >/dev/null 2>&1; then
        append_section "$file" "ss relevant" bash -c "ss -tanp | rg 'pid=$pid|:8082|:10808' || true"
    else
        echo "ss not found." >>"$file"
    fi

    if command -v ls >/dev/null 2>&1 && [ -d "/proc/$pid/fd" ]; then
        append_section "$file" "fd links" bash -c "ls -l /proc/$pid/fd || true"
    fi
}

collect_logs() {
    local label="$1"
    local file="$OUT_DIR/${label}-logs.txt"
    {
        echo "time=$(date --iso-8601=seconds)"
        echo
        echo "===== recent lifecycle lines ====="
        rg -n "Received SIGTERM|Received SIGINT|Graceful shutdown|Server shut down|Retrying upstream|Request:|Logging initialized|Dual output" \
            "$CONFIG_LOG" "$TRACING_LOG" 2>/dev/null | tail -n 200 || true
        echo
        echo "===== tail $CONFIG_LOG ====="
        tail -n 300 "$CONFIG_LOG" 2>/dev/null || true
        echo
        echo "===== tail $TRACING_LOG ====="
        tail -n 300 "$TRACING_LOG" 2>/dev/null || true
    } >"$file" 2>&1
}

echo "Writing diagnostics to: $OUT_DIR"

PID_AT_START="$(current_pid || true)"
echo "PID at start: ${PID_AT_START:-<none>}" | tee "$OUT_DIR/summary.txt"

run_capture "$OUT_DIR/environment.txt" env
collect_snapshot "before-stop" "$PID_AT_START"
collect_logs "before-stop"

TAIL_PID=""
{
    echo "time=$(date --iso-8601=seconds)"
    echo "Following logs while stop runs..."
    tail -F "$CONFIG_LOG" "$TRACING_LOG" 2>&1
} >"$OUT_DIR/live-logs-during-stop.txt" &
TAIL_PID="$!"
sleep 0.2

STRACE_PID=""
if [ "$RUN_STRACE" -eq 1 ] && [ -n "$PID_AT_START" ]; then
    if command -v strace >/dev/null 2>&1; then
        timeout "$STRACE_SECONDS" strace -ff -tt -p "$PID_AT_START" \
            -e trace=network,signal,desc,futex,poll,epoll_wait \
            -o "$OUT_DIR/strace" >"$OUT_DIR/strace-launch.txt" 2>&1 &
        STRACE_PID="$!"
        sleep 0.5
    else
        echo "strace not found." >"$OUT_DIR/strace-launch.txt"
    fi
fi

if [ "$RUN_STOP" -eq 1 ]; then
    {
        echo "time_before=$(date --iso-8601=seconds)"
        echo "\$ $STOP_CMD"
        bash -lc "$STOP_CMD"
        status="$?"
        echo "exit_status=$status"
        echo "time_after=$(date --iso-8601=seconds)"
        exit "$status"
    } >"$OUT_DIR/stop-command.txt" 2>&1
    STOP_STATUS="$?"
else
    echo "--no-stop selected; stop command was not run." >"$OUT_DIR/stop-command.txt"
    STOP_STATUS=0
fi

sleep 1
collect_snapshot "after-stop-1s" "$PID_AT_START"
collect_logs "after-stop-1s"

sleep 6
collect_snapshot "after-stop-7s" "$PID_AT_START"
collect_logs "after-stop-7s"

if [ -n "$STRACE_PID" ]; then
    wait "$STRACE_PID" 2>/dev/null || true
fi

if [ -n "$TAIL_PID" ]; then
    kill "$TAIL_PID" 2>/dev/null || true
    wait "$TAIL_PID" 2>/dev/null || true
fi

{
    echo "Output directory: $OUT_DIR"
    echo "PID at start: ${PID_AT_START:-<none>}"
    echo "Stop status: $STOP_STATUS"
    echo "PID file after run: $(current_pid || true)"
    echo
    echo "Files:"
    find "$OUT_DIR" -maxdepth 1 -type f -printf '%f\n' | sort
} >>"$OUT_DIR/summary.txt"

tar -C "$(dirname "$OUT_DIR")" -czf "$OUT_DIR.tar.gz" "$(basename "$OUT_DIR")" 2>"$OUT_DIR/tar.txt" || true

echo
echo "Done."
echo "Directory: $OUT_DIR"
echo "Archive:   $OUT_DIR.tar.gz"
echo "Summary:"
cat "$OUT_DIR/summary.txt"
