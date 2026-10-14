#!/bin/sh
# Measure syro and print the numbers: RSS, footprint, cold start, process count.

set -eu

bin=${1:?usage: measure.sh <binary> [label]}
label=${2:-$(basename "$bin")}
scratch=$(mktemp -d "/tmp/syro-measure.XXXXXX")
settle=12
trap 'rm -rf "$scratch"' EXIT INT TERM

# A repo with five worktrees, and a state.json naming it. Five because that is
# the scenario in section 1, and more than that only makes the sidebar longer.
scene_tree() {
    dir=$1
    mkdir -p "$dir/repo" "$dir/home/.config/syro"
    cd "$dir/repo"
    git init -q -b main .
    git -c user.email=m@easure -c user.name=measure commit -q --allow-empty -m init
    for n in 1 2 3 4 5; do
        git worktree add -q -b "wt-$n" "$dir/wt-$n"
    done
    cd / >/dev/null
    cat > "$dir/home/.config/syro/state.json" <<EOF
{"repos":[{"id":"measure","name":"repo","path":"$dir/repo"}]}
EOF
}

# Echoes "<rss-ku8>|<footprint>|<cold>|<children>"; keeps the app's log in
# $home/log so a scene that dies can say why.
run_scene() {
    home=$1
    mkdir -p "$home/.config/syro"
    log="$home/log"
    start=$(perl -MTime::HiRes=time -e 'print time')
    HOME="$home" RUST_LOG=info "$bin" >"$log" 2>&1 &
    pid=$!

    cold="-"
    i=0
    while [ "$i" -lt 200 ]; do
        if [ -n "$(pgrep -P "$pid" 2>/dev/null | head -1)" ]; then
            cold=$(perl -MTime::HiRes=time -e "printf '%.0f ms', (time() - $start) * 1000")
            break
        fi
        sleep 0.1
        i=$((i + 1))
    done

    sleep "$settle"
    rss=$(ps -o rss= -p "$pid" 2>/dev/null | tr -d ' ')
    if [ -z "$rss" ]; then
        echo "measure.sh: the app died in scene $home; its log:" >&2
        cat "$log" >&2
        exit 1
    fi
    footprint -p "$pid" > "$home/footprint" 2>&1 || true
    # footprint prints its total on the banner line: "<name> [<pid>]: 64-bit Footprint: 63 MB (16384
        # bytes per page)".
    fp=$(sed -n 's/.*Footprint: \([0-9.]* [KMG]B\).*/\1/p' "$home/footprint" | head -1)
    [ -n "$fp" ] || fp=-
    children=$(pgrep -P "$pid" 2>/dev/null | wc -l | tr -d ' ')
    kill "$pid" 2>/dev/null || true
    sleep 1
    printf '%s|%s|%s|%s\n' "$rss" "$fp" "$cold" "$children"
}

field() { echo "$1" | cut -d'|' -f"$2"; }

echo "=== $label ==="
echo "binary: $bin ($(du -h "$bin" | cut -f1))"
echo "machine: $(sysctl -n machdep.cpu.brand_string), $(sysctl -n hw.memsize | awk '{printf "%.0f", $1/1073741824}') GB, macOS $(sw_vers -productVersion), $(sysctl -n hw.ncpu) cores"
echo

mkdir -p "$scratch/empty/home/.config/syro"
a=$(run_scene "$scratch/empty/home")
echo "empty:    $(field "$a" 1) KB RSS · footprint $(field "$a" 2) · $(field "$a" 4) child processes"

scene_tree "$scratch/term"
b=$(run_scene "$scratch/term/home")
echo "worktree: $(field "$b" 1) KB RSS · footprint $(field "$b" 2) · cold $(field "$b" 3) (proxy) · $(field "$b" 4) child process"

echo
printf '%-9s %9s %11s %11s %8s\n' scene RSS_KB footprint cold_proxy children
printf '%-9s %9s %11s %11s %8s\n' empty "$(field "$a" 1)" "$(field "$a" 2)" - "$(field "$a" 4)"
printf '%-9s %9s %11s %11s %8s\n' worktree "$(field "$b" 1)" "$(field "$b" 2)" "$(field "$b" 3)" "$(field "$b" 4)"
