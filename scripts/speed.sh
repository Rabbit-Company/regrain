#!/usr/bin/env bash
# Times regrain diff for every scan mode on one pair of videos.
#
#   scripts/speed.sh <source> <denoised> [regrain binary] [extra diff options]
set -eu
source=${1:?usage: speed.sh <source> <denoised> [regrain binary] [extra diff options]}
denoised=${2:?usage: speed.sh <source> <denoised> [regrain binary] [extra diff options]}
regrain=${3:-regrain}
shift $(( $# < 3 ? $# : 3 ))
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
table="$work/grain.tbl"
for scan in full half quarter eighth; do
    summary=$("$regrain" diff "$source" "$denoised" -o "$table" -y --scan "$scan" --progress json "$@" | tail -1)
    seconds=$(echo "$summary" | grep -oE '"seconds":[0-9.]+' | cut -d: -f2)
    echo "$scan: $seconds s"
done
