#!/usr/bin/env bash
# Runs export, remove and apply over every AV1 stream below a folder and checks that the
# pictures without grain stay identical and that dav1d decodes every output.
#
#   scripts/conformance.sh <folder> [regrain binary]
#
# The libaom test vectors are listed in libaom's test/test_vectors.cc and can be
# downloaded from https://storage.googleapis.com/aom-test-data/<name>.
set -u
folder=${1:?usage: conformance.sh <folder> [regrain binary]}
regrain=${2:-regrain}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

cat > "$work/grain.tbl" <<'TABLE'
filmgrn1
E 0 2000000 1 100 1
	p 2 7 0 10 0 1 128 192 256 128 192 256
	sY 3  0 20 100 60 255 25
	sCb 2 0 20 255 30
	sCr 1 128 25
	cY 2 4 6 4 2 4 10 24 10 4 6 26
	cCb 0 0 4 0 0 0 6 16 6 0 4 18 20
	cCr 0 0 0 0 0 0 0 0 0 0 0 0 0
E 2000000 9223372036854775807 1 200 1
	p 0 6 0 11 0 1 128 192 256 128 192 256
	sY 1  128 40
	sCb 0
	sCr 0
	cY
	cCb 0
	cCr 0
TABLE

picture() {
    ffmpeg -v error -nostdin -c:v libdav1d -filmgrain 0 -i "$1" -map 0:V:0 -f md5 - 2>/dev/null
}

passed=0
failed=0
while IFS= read -r input; do
    name=${input#"$folder"/}
    extension=${name##*.}
    reasons=()
    reference=$(picture "$input")
    if [ -z "$reference" ]; then
        echo "SKIP $name: ffmpeg cannot decode the original"
        continue
    fi
    "$regrain" -q export "$input" -o "$work/exported.tbl" -y || reasons+=("export failed")
    "$regrain" -q remove "$input" -o "$work/removed.$extension" -y --verify || reasons+=("remove failed")
    [ "$(picture "$work/removed.$extension")" = "$reference" ] || reasons+=("remove changed the picture")
    "$regrain" -q apply "$input" -o "$work/grain.$extension" -t "$work/grain.tbl" -y --verify || reasons+=("apply failed")
    [ "$(picture "$work/grain.$extension")" = "$reference" ] || reasons+=("apply changed the picture")
    if [ -s "$work/exported.tbl" ] && grep -q '^E' "$work/exported.tbl"; then
        "$regrain" -q apply "$work/removed.$extension" -o "$work/again.$extension" -t "$work/exported.tbl" -y --verify || reasons+=("applying the exported table failed")
    fi
    if [ ${#reasons[@]} -eq 0 ]; then
        passed=$((passed + 1))
    else
        failed=$((failed + 1))
        echo "FAIL $name: ${reasons[*]}"
    fi
done < <(find "$folder" -type f \( -name '*.ivf' -o -name '*.mkv' -o -name '*.webm' -o -name '*.obu' \) | sort)
echo "$passed passed, $failed failed"
[ "$failed" -eq 0 ]
