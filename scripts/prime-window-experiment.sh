#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: scripts/prime-window-experiment.sh --slot SLOT [options]

Run a controlled quota-window experiment on one cx slot.

The script separates three signals:
  1. whether Codex exec returned token usage,
  2. whether /wham/usage reports used_percent > 1%,
  3. whether reset_at stops sliding as wall-clock time advances.

Options:
  --slot SLOT             Slot to test. Required.
  --manager-dir DIR       cx profile-manager dir. Default: $CX_PROFILE_MANAGER_DIR or ~/.codex/profile-manager.
  --cx-bin PATH           cx binary. Default: cx.
  --codex-bin PATH        real Codex binary. Default: codex.
  --prompt TEXT           Prompt to send. Default: Reply exactly: hi
  --max-requests N        Maximum requests to send. Default: 3.
  --pre-polls N           Polls before the first request. Default: 3.
  --post-polls N          Polls after each request. Default: 12.
  --poll-interval SEC     Seconds between usage polls. Default: 5.
  --out FILE              JSONL output path. Default: .work/prime-experiments/<slot>-<timestamp>.jsonl.
  --allow-active          Continue even if the slot already looks active during baseline.
  --no-stop-on-active     Continue all requests even after active is detected.
  -h, --help              Show this help.

Interpretation:
  - Idle/default usually looks like used=1.0 and reset_at ~= now+18000.
    Across polls, resetAtShift should be close to elapsedSincePrevious.
  - Active usually has used>1.0, or reset_at stops sliding.
    Across polls, resetAtShift should be close to 0.
USAGE
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"
}

json_number_or_null() {
  local value="${1:-}"
  if [[ "$value" =~ ^-?[0-9]+([.][0-9]+)?$ ]]; then
    printf '%s' "$value"
  else
    printf 'null'
  fi
}

json_string_or_null() {
  local value="${1:-}"
  if [[ -n "$value" ]]; then
    jq -cn --arg value "$value" '$value'
  else
    printf 'null'
  fi
}

SLOT=""
MANAGER_DIR="${CX_PROFILE_MANAGER_DIR:-"$HOME/.codex/profile-manager"}"
CX_BIN="${CX_BIN:-cx}"
CODEX_BIN="${CODEX_BIN:-codex}"
PROMPT="Reply exactly: hi"
MAX_REQUESTS=3
PRE_POLLS=3
POST_POLLS=12
POLL_INTERVAL=5
OUT=""
ALLOW_ACTIVE=0
STOP_ON_ACTIVE=1

while [[ $# -gt 0 ]]; do
  case "$1" in
    --slot)
      [[ $# -ge 2 ]] || die "--slot requires a value"
      SLOT="$2"
      shift
      ;;
    --manager-dir)
      [[ $# -ge 2 ]] || die "--manager-dir requires a value"
      MANAGER_DIR="$2"
      shift
      ;;
    --cx-bin)
      [[ $# -ge 2 ]] || die "--cx-bin requires a value"
      CX_BIN="$2"
      shift
      ;;
    --codex-bin)
      [[ $# -ge 2 ]] || die "--codex-bin requires a value"
      CODEX_BIN="$2"
      shift
      ;;
    --prompt)
      [[ $# -ge 2 ]] || die "--prompt requires a value"
      PROMPT="$2"
      shift
      ;;
    --max-requests)
      [[ $# -ge 2 ]] || die "--max-requests requires a value"
      MAX_REQUESTS="$2"
      shift
      ;;
    --pre-polls)
      [[ $# -ge 2 ]] || die "--pre-polls requires a value"
      PRE_POLLS="$2"
      shift
      ;;
    --post-polls)
      [[ $# -ge 2 ]] || die "--post-polls requires a value"
      POST_POLLS="$2"
      shift
      ;;
    --poll-interval)
      [[ $# -ge 2 ]] || die "--poll-interval requires a value"
      POLL_INTERVAL="$2"
      shift
      ;;
    --out)
      [[ $# -ge 2 ]] || die "--out requires a value"
      OUT="$2"
      shift
      ;;
    --allow-active)
      ALLOW_ACTIVE=1
      ;;
    --no-stop-on-active)
      STOP_ON_ACTIVE=0
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      die "unknown option: $1"
      ;;
  esac
  shift
done

[[ -n "$SLOT" ]] || die "--slot is required"
[[ "$MAX_REQUESTS" =~ ^[0-9]+$ && "$MAX_REQUESTS" -ge 1 ]] || die "--max-requests must be >= 1"
[[ "$PRE_POLLS" =~ ^[0-9]+$ ]] || die "--pre-polls must be an integer"
[[ "$POST_POLLS" =~ ^[0-9]+$ && "$POST_POLLS" -ge 1 ]] || die "--post-polls must be >= 1"
[[ "$POLL_INTERVAL" =~ ^[0-9]+$ && "$POLL_INTERVAL" -ge 1 ]] || die "--poll-interval must be >= 1"

need_cmd jq
need_cmd awk
need_cmd date
need_cmd mktemp

if [[ "$CX_BIN" != */* ]]; then
  need_cmd "$CX_BIN"
fi
if [[ "$CODEX_BIN" != */* ]]; then
  need_cmd "$CODEX_BIN"
fi

SLOT_HOME="${MANAGER_DIR}/slots/${SLOT}/home"
SLOT_SQLITE_HOME="${SLOT_HOME}/sqlite"
[[ -d "$SLOT_HOME" ]] || die "slot home not found: $SLOT_HOME"

if [[ -z "$OUT" ]]; then
  mkdir -p .work/prime-experiments
  OUT=".work/prime-experiments/${SLOT}-$(date -u +%Y%m%dT%H%M%SZ).jsonl"
else
  mkdir -p "$(dirname "$OUT")"
fi

PREVIOUS_NOW=""
PREVIOUS_RESET_AT=""
FIRST_ACTIVE=""
FIRST_STABLE=""
LAST_USED=""
LAST_RESET_AT=""
LAST_TTL=""

poll_usage_raw() {
  "$CX_BIN" status \
    --manager-dir "$MANAGER_DIR" \
    --no-cache \
    --json \
    --no-progress \
    "$SLOT"
}

is_active_by_usage() {
  local used="$1"
  local ttl="$2"
  [[ -n "$used" && -n "$ttl" ]] || return 1
  awk -v used="$used" -v ttl="$ttl" 'BEGIN { exit !(used > 1.0 && ttl > 300) }'
}

is_reset_stable() {
  local shift="$1"
  [[ -n "$shift" ]] || return 1
  awk -v shift="$shift" 'BEGIN { if (shift < 0) shift = -shift; exit !(shift <= 2) }'
}

record_poll() {
  local phase="$1"
  local request_index="$2"
  local poll_index="$3"
  local raw now result status used weekly reset_at weekly_reset_at score ttl reset_shift elapsed shift_minus_elapsed

  raw="$(poll_usage_raw)"
  now="$(date +%s)"
  result="$(jq -c '.results[0] // {}' <<<"$raw")"
  status="$(jq -r '.status // empty' <<<"$result")"
  used="$(jq -r '.fiveHourUsedPercent // empty' <<<"$result")"
  weekly="$(jq -r '.weeklyUsedPercent // empty' <<<"$result")"
  reset_at="$(jq -r '.fiveHourRefreshAt // empty' <<<"$result")"
  weekly_reset_at="$(jq -r '.weeklyRefreshAt // empty' <<<"$result")"
  score="$(jq -r '.score // empty' <<<"$result")"

  ttl=""
  if [[ "$reset_at" =~ ^[0-9]+$ ]]; then
    ttl=$((reset_at - now))
  fi

  reset_shift=""
  elapsed=""
  shift_minus_elapsed=""
  if [[ "$reset_at" =~ ^[0-9]+$ && "$PREVIOUS_RESET_AT" =~ ^[0-9]+$ ]]; then
    reset_shift=$((reset_at - PREVIOUS_RESET_AT))
  fi
  if [[ "$PREVIOUS_NOW" =~ ^[0-9]+$ ]]; then
    elapsed=$((now - PREVIOUS_NOW))
  fi
  if [[ -n "$reset_shift" && -n "$elapsed" ]]; then
    shift_minus_elapsed=$((reset_shift - elapsed))
  fi

  local active=false
  if is_active_by_usage "$used" "$ttl"; then
    active=true
    if [[ -z "$FIRST_ACTIVE" ]]; then
      FIRST_ACTIVE="request=${request_index} poll=${poll_index} used=${used} ttl=${ttl}s"
    fi
  fi

  local stable=false
  if is_reset_stable "$reset_shift"; then
    stable=true
    if [[ -z "$FIRST_STABLE" && "$phase" != "baseline" ]]; then
      FIRST_STABLE="request=${request_index} poll=${poll_index} resetAtShift=${reset_shift}s"
    fi
  fi

  jq -cn \
    --arg event "usage_poll" \
    --arg slot "$SLOT" \
    --arg phase "$phase" \
    --arg status "$status" \
    --argjson timestamp "$now" \
    --argjson requestIndex "$(json_number_or_null "$request_index")" \
    --argjson pollIndex "$(json_number_or_null "$poll_index")" \
    --argjson fiveHourUsedPercent "$(json_number_or_null "$used")" \
    --argjson weeklyUsedPercent "$(json_number_or_null "$weekly")" \
    --argjson score "$(json_number_or_null "$score")" \
    --argjson fiveHourRefreshAt "$(json_number_or_null "$reset_at")" \
    --argjson weeklyRefreshAt "$(json_number_or_null "$weekly_reset_at")" \
    --argjson ttlSeconds "$(json_number_or_null "$ttl")" \
    --argjson resetAtShift "$(json_number_or_null "$reset_shift")" \
    --argjson elapsedSincePrevious "$(json_number_or_null "$elapsed")" \
    --argjson shiftMinusElapsed "$(json_number_or_null "$shift_minus_elapsed")" \
    --argjson activeByUsage "$active" \
    --argjson resetStable "$stable" \
    '$ARGS.named' >>"$OUT"

  printf '%-10s req=%s poll=%s status=%s used=%s ttl=%ss reset_at=%s shift=%ss elapsed=%ss active=%s stable=%s\n' \
    "$phase" "$request_index" "$poll_index" "${status:-?}" "${used:-?}" "${ttl:-?}" "${reset_at:-?}" \
    "${reset_shift:-?}" "${elapsed:-?}" "$active" "$stable"

  PREVIOUS_NOW="$now"
  PREVIOUS_RESET_AT="$reset_at"
  LAST_USED="$used"
  LAST_RESET_AT="$reset_at"
  LAST_TTL="$ttl"
}

record_request() {
  local request_index="$1"
  local started finished elapsed tmp exit_code tokens

  tmp="$(mktemp)"
  started="$(date +%s)"
  set +e
  CODEX_HOME="$SLOT_HOME" \
    CODEX_SQLITE_HOME="$SLOT_SQLITE_HOME" \
    "$CODEX_BIN" \
    --ask-for-approval never \
    exec \
    --skip-git-repo-check \
    --ephemeral \
    --ignore-rules \
    -s read-only \
    --color never \
    "$PROMPT" >"$tmp" 2>&1 </dev/null
  exit_code=$?
  set -e
  finished="$(date +%s)"
  elapsed=$((finished - started))
  tokens="$(awk 'tolower($0) == "tokens used" { getline; gsub(/[^0-9]/, ""); print; exit }' "$tmp")"
  rm -f "$tmp"

  jq -cn \
    --arg event "codex_request" \
    --arg slot "$SLOT" \
    --argjson timestamp "$finished" \
    --argjson requestIndex "$request_index" \
    --argjson exitCode "$exit_code" \
    --argjson elapsedSeconds "$elapsed" \
    --argjson tokensUsed "$(json_number_or_null "$tokens")" \
    --argjson promptBytes "${#PROMPT}" \
    '$ARGS.named' >>"$OUT"

  printf 'request    req=%s exit=%s elapsed=%ss tokens=%s\n' \
    "$request_index" "$exit_code" "$elapsed" "${tokens:-?}"
  [[ "$exit_code" -eq 0 ]] || die "codex request failed at request ${request_index}"
}

jq -cn \
  --arg event "experiment_started" \
  --arg slot "$SLOT" \
  --arg managerDir "$MANAGER_DIR" \
  --arg cxBin "$CX_BIN" \
  --arg codexBin "$CODEX_BIN" \
  --argjson timestamp "$(date +%s)" \
  --argjson maxRequests "$MAX_REQUESTS" \
  --argjson prePolls "$PRE_POLLS" \
  --argjson postPolls "$POST_POLLS" \
  --argjson pollIntervalSeconds "$POLL_INTERVAL" \
  --argjson promptBytes "${#PROMPT}" \
  '$ARGS.named' >"$OUT"

printf 'writing JSONL: %s\n' "$OUT"
printf 'baseline polls: %s every %ss\n' "$PRE_POLLS" "$POLL_INTERVAL"

for ((poll = 1; poll <= PRE_POLLS; poll++)); do
  record_poll "baseline" 0 "$poll"
  if [[ "$poll" -lt "$PRE_POLLS" ]]; then
    sleep "$POLL_INTERVAL"
  fi
done

if [[ "$ALLOW_ACTIVE" -eq 0 ]] && is_active_by_usage "$LAST_USED" "$LAST_TTL"; then
  die "slot already looks active; rerun later or pass --allow-active"
fi

for ((request = 1; request <= MAX_REQUESTS; request++)); do
  record_request "$request"
  for ((poll = 1; poll <= POST_POLLS; poll++)); do
    record_poll "after_request" "$request" "$poll"
    if [[ "$STOP_ON_ACTIVE" -eq 1 ]] && is_active_by_usage "$LAST_USED" "$LAST_TTL"; then
      break
    fi
    if [[ "$poll" -lt "$POST_POLLS" ]]; then
      sleep "$POLL_INTERVAL"
    fi
  done
  if [[ "$STOP_ON_ACTIVE" -eq 1 ]] && is_active_by_usage "$LAST_USED" "$LAST_TTL"; then
    break
  fi
done

jq -cn \
  --arg event "experiment_finished" \
  --arg slot "$SLOT" \
  --arg firstActive "$FIRST_ACTIVE" \
  --arg firstStable "$FIRST_STABLE" \
  --argjson timestamp "$(date +%s)" \
  '$ARGS.named' >>"$OUT"

printf '\nsummary\n'
printf 'jsonl: %s\n' "$OUT"
printf 'first active by used>1%%: %s\n' "${FIRST_ACTIVE:-none}"
printf 'first reset_at stable poll: %s\n' "${FIRST_STABLE:-none}"
printf 'rule of thumb: idle reset_at slides by roughly elapsed seconds; active reset_at shift stays near 0.\n'
