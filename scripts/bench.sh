#!/usr/bin/env bash
# Runs the performance-budget scenarios (`gpui-chat --bench <name>`, see crates/app/src/bench.rs)
# offline in a throwaway HOME, prints a table against the budgets of docs/ARCHITECTURE.md and
# exits non-zero if one is missed.
#
#   scripts/bench.sh                      # build release, run everything
#   scripts/bench.sh --only memory,idle   # some scenarios
#   scripts/bench.sh --bin path/to/gpui-chat --no-build
#   scripts/bench.sh --launch-runs 9      # launch is the median of n runs (default 5)
#
# Every scenario opens a real window for a few seconds (macOS, needs a logged-in session).
# Numbers depend on the machine; the table says which one ran them.
set -euo pipefail

cd "$(dirname "$0")/.."

only="scroll,burst,idle,memory,images,launch"
bin="target/release/gpui-chat"
build=1
launch_runs=5
while [ $# -gt 0 ]; do
  case "$1" in
    --only) only="$2"; shift 2 ;;
    --only=*) only="${1#*=}"; shift ;;
    --bin) bin="$2"; shift 2 ;;
    --bin=*) bin="${1#*=}"; shift ;;
    --no-build) build=0; shift ;;
    --launch-runs) launch_runs="$2"; shift 2 ;;
    -h|--help) sed -n '2,13p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

if [ "$build" = 1 ]; then
  cargo build --release -p gpui-chat >&2
fi

home="$(mktemp -d "${TMPDIR:-/tmp}/gpui-chat-bench.XXXXXX")"
trap 'rm -rf "$home"' EXIT
results="$home/results.jsonl"
: > "$results"

run() {
  local name="$1" t0 line
  t0="$(python3 -c 'import time; print(time.time_ns())')"
  # The isolated HOME keeps the user's settings, state and caches out of it (and untouched).
  if ! line="$(HOME="$home" GPUI_CHAT_BENCH_T0_NS="$t0" RUST_LOG=warn \
      "$bin" --bench "$name" 2>>"$home/stderr.log" | grep '^BENCH ' | tail -n 1)"; then
    line=""
  fi
  if [ -z "$line" ]; then
    echo "{\"bench\":\"$name\",\"error\":\"no result (see stderr)\"}" >> "$results"
    tail -n 20 "$home/stderr.log" >&2 || true
  else
    echo "${line#BENCH }" >> "$results"
  fi
}

IFS=',' read -r -a names <<< "$only"
for name in "${names[@]}"; do
  if [ "$name" = launch ]; then
    # One unmeasured run first so the binary is paged in (a cold first launch after a build is
    # dominated by the loader and signature checks, not by the app).
    run launch
    : > "$home/launch.jsonl"
    for _ in $(seq "$launch_runs"); do
      run launch
      tail -n 1 "$results" >> "$home/launch.jsonl"
    done
  else
    run "$name"
  fi
done

machine="$(sysctl -n machdep.cpu.brand_string 2>/dev/null || uname -m), $(($(sysctl -n hw.memsize 2>/dev/null || echo 0) / 1073741824)) GB, macOS $(sw_vers -productVersion 2>/dev/null || echo ?)"

python3 - "$results" "$home/launch.jsonl" "$machine" <<'PY'
import json, statistics, sys

results_path, launch_path, machine = sys.argv[1], sys.argv[2], sys.argv[3]
results = {}
for line in open(results_path):
    line = line.strip()
    if line:
        r = json.loads(line)
        results[r["bench"]] = r  # last one wins (launch: overwritten by the median below)
try:
    launches = [json.loads(l) for l in open(launch_path) if l.strip()]
except FileNotFoundError:
    launches = []
if launches:
    key = "exec_to_first_draw_ms" if all("exec_to_first_draw_ms" in l for l in launches) else "main_to_first_draw_ms"
    values = [l[key] for l in launches if l.get(key) is not None]
    results["launch"] = {"bench": "launch", "median_ms": statistics.median(values), "runs": len(values),
                         "min_ms": min(values), "max_ms": max(values), "key": key}

# (scenario, label, value key, budget, unit, extra columns)
rows = [
    ("scroll", "Frame time while scrolling, 1 pane at 100 msg/s (apply+draw p99)", "frame_total_p99_ms", 8.3, "ms",
     ["draw_p99_ms", "input_p99_ms", "frames", "dropped"]),
    ("burst", "Burst ingest 1 000 msg/s: input → drawn p99", "input_p99_ms", 16.7, "ms",
     ["frame_total_p99_ms", "dropped", "ingest_per_s"]),
    ("idle", "Idle CPU, nothing animated", "cpu_percent", 1.0, "%", ["draws"]),
    ("memory", "Memory, 10 channels × 1 000 messages (physical footprint)", "footprint_mb", 300.0, "MB",
     ["rss_mb", "image_bytes_mb", "images", "graveyard"]),
    ("images", "Image budget holds under a flood of distinct animated emotes (footprint)", "footprint_mb", 400.0, "MB",
     ["image_bytes_mb", "image_budget_mb", "rss_mb", "graveyard"]),
    ("launch", "Launch to first window (median)", "median_ms", 300.0, "ms", ["min_ms", "max_ms", "runs"]),
]
failed = False
print(f"\nMachine: {machine}\n")
print("| Budget | Measured | Target | Result | Details |")
print("|---|---|---|---|---|")
for name, label, key, budget, unit, extra in rows:
    r = results.get(name)
    if r is None:
        continue
    if "error" in r:
        print(f"| {label} | error: {r['error']} | < {budget:g} {unit} | ❌ | |")
        failed = True
        continue
    value = r.get(key)
    ok = value is not None and value < budget
    if name == "burst" and r.get("dropped", 0) > 0:
        ok = False
    if name == "images" and r.get("image_bytes_mb", 0) > r.get("image_budget_mb", 0) + 0.01:
        ok = False
    failed |= not ok
    details = ", ".join(f"{k} {r[k]:g}" for k in extra if isinstance(r.get(k), (int, float)))
    shown = "n/a" if value is None else f"{value:.2f} {unit}"
    print(f"| {label} | {shown} | < {budget:g} {unit} | {'✅' if ok else '❌'} | {details} |")
print()
for name in sorted(results):
    print(f"BENCH {json.dumps(results[name], separators=(',', ':'))}")
sys.exit(1 if failed else 0)
PY
