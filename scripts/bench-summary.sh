#!/usr/bin/env sh
# Build the CI job summary table from bench output.
# Usage: ./scripts/bench-summary.sh <bench-output-file>
# Appends Markdown to $GITHUB_STEP_SUMMARY (or stdout when unset).
set -eu

out_file="$1"

get() {
    # get <key> <line-prefix> - keys are unique, so match on the key itself.
    grep -a "$2" "$out_file" | grep "$1=" | sed -n "s/.*$1=\\([^ ]*\\).*/\\1/p" | tr -d '\r' | head -1
}

entries=$(get entries FERROW-MEASURE)
seed=$(get seed FERROW-MEASURE)
generate_ms=$(get generate_ms FERROW-MEASURE)
freeze_ms=$(get freeze_ms FERROW-MEASURE)
bytes_per_entry=$(get bytes_per_entry FERROW-MEASURE)
file_bytes=$(get file_bytes FERROW-MEASURE)
load_ms=$(get load_ms_median FERROW-MEASURE)
load_read_ms=$(get load_read_ms FERROW-MEASURE)
load_checksum_ms=$(get load_checksum_ms FERROW-MEASURE)
load_decode_ms=$(get load_decode_ms FERROW-MEASURE)
load_structure_ms=$(get load_structure_ms FERROW-MEASURE)
load_derived_ms=$(get load_derived_ms FERROW-MEASURE)
load_unattributed_ms=$(get load_unattributed_ms FERROW-MEASURE)

emit() {
    echo "## Ferrow M1 benchmark"
    echo
    echo "Synthetic fixture: ${entries} entries, seed ${seed} (ubuntu-latest)."
    echo
    echo "| Measure | Value |"
    echo "|---|---|"
    echo "| Fixture generate | ${generate_ms} ms |"
    echo "| Fixture freeze | ${freeze_ms} ms |"
    echo "| Memory | ${bytes_per_entry} bytes/entry |"
    echo "| Index file size | ${file_bytes} bytes |"
    echo "| Load saved index (median of 3) | ${load_ms} ms |"
    echo "| Load: read | ${load_read_ms} ms |"
    echo "| Load: checksum | ${load_checksum_ms} ms |"
    echo "| Load: decode | ${load_decode_ms} ms |"
    echo "| Load: structure validation | ${load_structure_ms} ms |"
    echo "| Load: derived-data validation | ${load_derived_ms} ms |"
    echo "| Load: unattributed | ${load_unattributed_ms} ms |"
    echo
    echo "Query latency, median of 5 (one-shot measures; divan detail below):"
    echo
    echo "| Query | First page (limit 100) | Full count | Hits |"
    echo "|---|---|---|---|"
    sed -n 's/^FERROW-QUERY query="\(.*\)" first_page_ms=\([^ ]*\) page_hits=[^ ]* full_count_ms=\([^ ]*\) total_hits=\([^ ]*\).*/| `\1` | \2 ms | \3 ms | \4 |/p' "$out_file"
    echo
    echo "<details><summary>divan output</summary>"
    echo
    echo '```'
    grep -a -v '^FERROW-' "$out_file" || true
    echo '```'
    echo
    echo "</details>"
}

if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
    emit >>"$GITHUB_STEP_SUMMARY"
else
    emit
fi
