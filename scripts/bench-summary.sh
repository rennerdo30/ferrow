#!/usr/bin/env sh
# Build the CI job summary table from bench output.
# Usage: ./scripts/bench-summary.sh <bench-output-file>
# Appends Markdown to $GITHUB_STEP_SUMMARY (or stdout when unset).
set -eu

if [ "$#" -ne 1 ] || [ ! -f "$1" ] || [ ! -s "$1" ]; then
    echo 'Missing or empty fixture profiling input' >&2
    exit 1
fi
out_file="$1"

get() {
    # get <key> <line-prefix> - keys are unique, so match on the key itself.
    value=$(grep -a "$2" "$out_file" | grep "$1=" | sed -n "s/.*$1=\\([^ ]*\\).*/\\1/p" | tr -d '\r' | head -1)
    if [ -z "$value" ]; then
        echo "Missing fixture profiling measurement: $1" >&2
        return 1
    fi
    printf '%s\n' "$value"
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
load_buffer_ms=$(get load_buffer_ms FERROW-MEASURE)
load_cleanup_ms=$(get load_cleanup_ms FERROW-MEASURE)
load_fold_ms=$(get load_fold_ms FERROW-MEASURE)
load_sort_ms=$(get load_sort_ms FERROW-MEASURE)
load_unattributed_ms=$(get load_unattributed_ms FERROW-MEASURE)

# Validate before opening the summary for append. A failed producer must not
# publish a plausible-looking table of empty measurements.
for prefix in 'FERROW-QUERY query=' 'FERROW-QUERY-PARTS query='; do
    if ! grep -aq "^$prefix" "$out_file"; then
        echo "Missing fixture profiling rows: $prefix" >&2
        exit 1
    fi
done

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
    echo "| Load: folded-data validation | ${load_fold_ms} ms |"
    echo "| Load: sort validation | ${load_sort_ms} ms |"
    echo "| Load: payload buffer | ${load_buffer_ms} ms |"
    echo "| Load: payload cleanup | ${load_cleanup_ms} ms |"
    echo "| Load: unattributed | ${load_unattributed_ms} ms |"
    echo
    echo "Query latency, median of 5 (one-shot measures; divan detail below):"
    echo
    echo "| Query | First page (limit 100) | Full count | Hits |"
    echo "|---|---|---|---|"
    sed 's/|/\\|/g' "$out_file" | sed -n 's/^FERROW-QUERY query="\(.*\)" first_page_ms=\([^ ]*\) page_hits=[^ ]* full_count_ms=\([^ ]*\) total_hits=\([^ ]*\).*/| `\1` | \2 ms | \3 ms | \4 |/p'
    echo
    echo "Additive query spans from the median-total sample (milliseconds):"
    echo
    echo '| Query | Operation | Total | Prepare | Scan | Collect | Unattributed |'
    echo '|---|---|---|---|---|---|---|'
    sed 's/|/\\|/g' "$out_file" | sed -n 's/^FERROW-QUERY-PARTS query="\(.*\)" operation=\([^ ]*\) total_ms=\([^ ]*\) prepare_ms=\([^ ]*\) scan_ms=\([^ ]*\) collect_ms=\([^ ]*\) unattributed_ms=\([^ ]*\).*/| `\1` | \2 | \3 | \4 | \5 | \6 | \7 |/p'
    echo
    echo '| Service IPC query | First page (limit 100) | Full count | Hits |'
    echo '|---|---|---|---|'
    sed 's/|/\\|/g' "$out_file" | sed -n 's/^FERROW-SERVICE query="\(.*\)" first_page_ms=\([^ ]*\) page_hits=[^ ]* full_count_ms=\([^ ]*\) total_hits=\([^ ]*\).*/| `\1` | \2 ms | \3 ms | \4 |/p'
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
