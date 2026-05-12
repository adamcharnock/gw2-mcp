#!/usr/bin/env bash
# Reject the commit if data/festivals.yaml has a `last_updated` older
# than the configured threshold. Surfaced via lefthook pre-commit;
# fixed by running the `refresh-festivals` Claude skill, which rewrites
# the file and updates last_updated.
#
# Why: the festival schedule data is approximate and inferred from the
# wiki. If we let it rot, the runtime tool starts misleading the user
# about when Halloween / Wintersday / etc. are running. 30 days is a
# generous cap — most festivals last ~3 weeks, so monthly refresh
# guarantees we never serve a window that's an entire iteration stale.

set -euo pipefail

YAML="data/festivals.yaml"
MAX_AGE_DAYS=${FESTIVALS_MAX_AGE_DAYS:-30}

if [ ! -f "$YAML" ]; then
    echo "ERROR: $YAML is missing." >&2
    exit 1
fi

# `last_updated: "YYYY-MM-DD"` — extract literally so we don't depend on
# yq being installed.
last_updated=$(grep -E '^last_updated:' "$YAML" \
    | head -n 1 \
    | sed -E 's/^last_updated:[[:space:]]*"?([0-9]{4}-[0-9]{2}-[0-9]{2})"?.*/\1/')

if [ -z "$last_updated" ]; then
    echo "ERROR: could not parse last_updated from $YAML" >&2
    exit 1
fi

# Portable epoch-seconds conversion (GNU date vs BSD/macOS date).
if last_epoch=$(date -d "$last_updated" +%s 2>/dev/null); then
    :
elif last_epoch=$(date -j -f "%Y-%m-%d" "$last_updated" +%s 2>/dev/null); then
    :
else
    echo "ERROR: could not parse date '$last_updated' from $YAML" >&2
    exit 1
fi

now_epoch=$(date +%s)
age_days=$(( (now_epoch - last_epoch) / 86400 ))

if [ "$age_days" -gt "$MAX_AGE_DAYS" ]; then
    cat >&2 <<EOF
ERROR: $YAML is $age_days days old (last_updated: $last_updated).
Max allowed age: $MAX_AGE_DAYS days.

Run the refresh-festivals Claude skill to bring the data up to date:
    .claude/skills/refresh-festivals/SKILL.md

(Or set FESTIVALS_MAX_AGE_DAYS=<n> to override the threshold for a
single commit — discouraged.)
EOF
    exit 1
fi

exit 0
