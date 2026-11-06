#!/usr/bin/env bash
set -euo pipefail
bin="$PWD/target/debug"
receipt="$RUNNER_TEMP/ferrow-service-pid.txt"
cleanup() {
  if systemctl cat ferrowd.service >/dev/null 2>&1; then
    timeout 25s sudo systemctl stop ferrowd.service
    if [[ -s "$receipt" ]]; then
      read -r recorded start < "$receipt"
      if [[ -r "/proc/$recorded/stat" ]] && [[ $(awk '{print $22}' "/proc/$recorded/stat") == "$start" ]]; then
        echo "recorded service PID $recorded still alive" >&2; return 1
      fi
    fi
    timeout 25s sudo "$bin/ferrow" service uninstall
    test ! -e /etc/systemd/system/ferrowd.service
  fi
}
if [[ ${1:-} == cleanup ]]; then cleanup; exit; fi
trap 'result=$?; trap - EXIT; cleanup || exit 1; exit "$result"' EXIT
tree="$RUNNER_TEMP/svc-tree"
mkdir -p "$tree"
echo x > "$tree/ci-needle.txt"
timeout 25s sudo "$bin/ferrow" service install
timeout 10s sudo "$bin/ferrow" index "$tree" --out /var/lib/ferrow/index.bin
timeout 25s sudo systemctl start ferrowd.service
service_pid=$(systemctl show ferrowd.service -p MainPID --value)
test "$service_pid" -gt 0
echo "$service_pid $(awk '{print $22}' "/proc/$service_pid/stat")" > "$receipt"
if [[ ${1:-} == fail-after-start ]]; then exit 42; fi
ready=''
for _attempt in {1..30}; do
  if timeout 5s sudo "$bin/ferrow" status | grep -q 'mode: service'; then ready=1; break; fi
  sleep 0.1
done
test -n "$ready"
timeout 5s sudo "$bin/ferrow" search ci-needle | grep -q ci-needle.txt
# A non-root caller must receive a clear refusal and no privileged result.
if timeout 5s env XDG_DATA_HOME="$RUNNER_TEMP/empty-home" "$bin/ferrow" search ci-needle > "$RUNNER_TEMP/ordinary.out" 2> "$RUNNER_TEMP/ordinary.err"; then
  test ! -s "$RUNNER_TEMP/ordinary.out"
fi
grep -q Unauthorized "$RUNNER_TEMP/ordinary.err"
read -r u1 s1 < <(awk '{print $14, $15}' "/proc/$service_pid/stat")
sleep 3
read -r u2 s2 < <(awk '{print $14, $15}' "/proc/$service_pid/stat")
test "$((u2+s2-u1-s1))" -le 10
