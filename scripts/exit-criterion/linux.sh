#!/usr/bin/env bash
# The Linux half of Phase 8's exit criterion (ADR 0042): a real shoald, on a Linux runner,
# pairs as a new device with the Windows runner's, which holds the user key, joins the partial
# folder it offers as a full replica, and fills it with the files the Windows half checks. The
# two halves find each other through iroh's relays and then talk directly over the LAN; each
# pairing code goes to the Windows half as a commit status on this run's commit.
#
# Needs TOKEN (this run's token, which may write statuses), GITHUB_SERVER_URL,
# GITHUB_REPOSITORY, GITHUB_SHA and GITHUB_RUN_ID, as a workflow run has, and shoald on PATH.
set -euo pipefail
run=$GITHUB_RUN_ID
work=${RUNNER_TEMP:-$(mktemp -d)}/exit-criterion
data=$work/data
folder=$work/folder
rm -rf "$work"
mkdir -p "$work"

post() { # post <name> <value>: a status named exit-criterion/<run>/<name>, carrying value
  local body
  body=$(printf '{"state":"success","context":"exit-criterion/%s/%s","description":"%s","target_url":"https://mailbox.invalid/%s"}' \
    "$run" "$1" "$1" "$2")
  curl -sSf -o /dev/null -X POST -H "Authorization: token $TOKEN" -H 'Content-Type: application/json' \
    "$GITHUB_SERVER_URL/api/v1/repos/$GITHUB_REPOSITORY/statuses/$GITHUB_SHA" -d "$body"
}

# Pair, with a new code each time one expires unused, while the Windows half may still be queued.
deadline=$((SECONDS + 75 * 60))
paired=
while [ -z "$paired" ] && [ "$SECONDS" -lt "$deadline" ]; do
  rm -f "$work/pair.out"
  shoald pair --data-dir "$data" --name "Linux runner" > "$work/pair.out" 2> "$work/pair.err" &
  pid=$!
  until grep -q '^code: ' "$work/pair.out" 2>/dev/null || ! kill -0 "$pid" 2>/dev/null; do sleep 1; done
  if code=$(sed -n 's/^code: //p' "$work/pair.out") && [ -n "$code" ]; then
    echo "pairing code posted"
    post code "$code"
  fi
  if wait "$pid" && grep -q '^user: ' "$work/pair.out"; then
    paired=1
  else
    echo "pairing ended unpaired: $(tail -n 1 "$work/pair.err")"
  fi
done
[ -n "$paired" ] || { echo "never paired"; exit 1; }
cat "$work/pair.out"
user=$(sed -n 's/^user: //p' "$work/pair.out")
folder_id=$(sed -n 's/^folder: \([0-9a-f]*\) .*/\1/p' "$work/pair.out" | head -n 1)
[ -n "$folder_id" ] || { echo "the Windows device offered no folder"; exit 1; }

# The files the Windows half expects, made the same way there.
mkdir -p "$folder/docs"
printf 'hello from Linux, run %s\n' "$run" > "$folder/hello.txt"
python3 -c 'import sys; sys.stdout.buffer.write(bytes((i * 7) % 251 for i in range(3 << 20)))' \
  > "$folder/docs/big.bin"
cat > "$work/shoald.toml" <<TOML
data_dir = '$data'

[[folders]]
id = "$folder_id"
path = '$folder'
members = ["$user"]
retention_days = 30
scan_interval_secs = 5
TOML
shoald config check "$work/shoald.toml"
shoald run --config "$work/shoald.toml" 2> "$work/daemon.log" &
daemon=$!
trap 'kill -INT "$daemon" 2>/dev/null; wait "$daemon" 2>/dev/null || true' EXIT

# Done when the Windows half's file arrives.
until grep -qs "run $run" "$folder/done.txt"; do
  if [ "$SECONDS" -ge "$deadline" ] || ! kill -0 "$daemon" 2>/dev/null; then
    echo "no word from the Windows half"
    tail -n 80 "$work/daemon.log"
    exit 1
  fi
  sleep 2
done
echo "the Windows half's file arrived"
post finished ok
