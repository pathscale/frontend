#!/bin/sh
# bounded.sh SECONDS CMD...: run CMD, kill it after SECONDS. Output to stdout.
limit=$1; shift
"$@" &
p=$!
i=0
while kill -0 $p 2>/dev/null; do
  if [ $i -ge $limit ]; then kill -9 $p; echo "WATCHDOG-KILLED after ${limit}s: $*"; exit 124; fi
  sleep 1; i=$((i+1))
done
wait $p
