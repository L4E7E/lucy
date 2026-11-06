#!/bin/sh
# Read-only report. Run in the host session while the desired popup is open.
set -eu
for lucy_pid in $(pgrep -x lucy-shell || true); do
    printf '\nPID %s: ' "$lucy_pid"
    tr '\000' ' ' < "/proc/$lucy_pid/cmdline"
    printf '\n'
    if [ -r "/proc/$lucy_pid/smaps_rollup" ]; then
        awk '/^(Rss|Pss|Private_Clean|Private_Dirty|Shared_Clean|Shared_Dirty|Swap):/ { printf "%-16s %8.2f MiB\n", $1, $2/1024 }' "/proc/$lucy_pid/smaps_rollup"
        printf 'Largest mappings (RSS KiB, PSS KiB, mapping):\n'
        awk '
          /^[0-9a-f]+-[0-9a-f]+ / { name=$0 }
          /^Rss:/ { rss=$2 }
          /^Pss:/ { if(rss>0) print rss, $2, name }
        ' "/proc/$lucy_pid/smaps" | sort -nr | head -12
    else
        printf 'Cannot read process memory; run this script in the host user session.\n'
    fi
done
