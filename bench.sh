#!/usr/bin/env bash
# agnt-rs benchmark harness
set -u
BIN=./target/release/agnt-rs
MODELS=("$@")
[ ${#MODELS[@]} -eq 0 ] && MODELS=(gemma4:e2b gemma4:e4b gemma4:26b qwen3:8b)

TESTS=(
  "basic|Say only the word: ok"
  "tool_time|Use the get_time tool, then report only the number it returned."
  "tool_add|Use the add tool to compute 7 plus 13. Report only the number."
  "parallel|Use the add tool twice in one turn: once for 7+13, once for 100+200. Then report both sums."
)

run_one() {
  local model="$1" prompt="$2"
  local t0 t1 out
  t0=$(date +%s%N)
  out=$(printf '%s\n' "$prompt" | AGNT_MODEL="$model" "$BIN" --no-db --no-stream 2>&1)
  t1=$(date +%s%N)
  local ms=$(( (t1 - t0) / 1000000 ))
  # Strip harness chatter: drop banner + empty + prompt lines.
  local body
  body=$(printf '%s\n' "$out" \
    | sed -n '/^agnt-rs —/,$p' \
    | grep -v '^agnt-rs —' \
    | grep -v '^(empty line' \
    | sed '/^> *$/d' \
    | sed 's/^> //' \
    | awk 'NF { printed=1 } printed')
  # Collapse to a single line for the table (keep tool log visible).
  local oneline
  oneline=$(printf '%s' "$body" | tr '\n' ' ' | sed 's/  */ /g' | cut -c1-100)
  printf "  %-10s %6dms  %s\n" "$1" "$ms" "$oneline" >/dev/null  # placeholder
  echo "$ms|$oneline"
}

for model in "${MODELS[@]}"; do
  echo
  echo "=== $model ==="
  # Warmup (load into VRAM). Short prompt, not timed.
  printf 'hi\n' | AGNT_MODEL="$model" "$BIN" --no-db --no-stream >/dev/null 2>&1
  for t in "${TESTS[@]}"; do
    label="${t%%|*}"
    prompt="${t#*|}"
    IFS='|' read -r ms body < <(run_one "$model" "$prompt")
    printf "  %-12s %7sms  %s\n" "$label" "$ms" "$body"
  done
done
