#!/usr/bin/env bash
#
# Probe the canonical and archived URLs recorded in docs/references/ and
# report which ones still resolve.
#
# This is the nondeterministic half of the registry rot check. It talks to
# the network, so its result depends on things no commit controls: a
# publisher rate limiting, a CDN having a bad afternoon, the Wayback Machine
# being slow. That is exactly why it runs on a schedule rather than on every
# push. Wiring a network probe into the per push gate would let an unrelated
# outage fail an unrelated build, and a gate that fails for reasons the
# author cannot fix is a gate people learn to ignore.
#
# What counts as a failure is drawn narrowly, for the same reason:
#
#   404 or 410     The document is gone. This is link rot, the thing being
#                  looked for, and it fails the run.
#   DNS failure    The host no longer resolves. Also rot, also fails.
#   Anything else  Reported, never fatal. A 403 usually means a publisher
#                  dislikes automated clients rather than that the paper
#                  vanished; a 429 means the probe was too eager; a 5xx or a
#                  timeout means try again next week.
#
# Probes are sequential with a delay between them, and a longer delay after
# web.archive.org, which is a donation funded service this repository leans
# on heavily. Hammering it would be rude and would earn a rate limit.
#
# Usage:
#   tools/probe_reference_links.sh [options]
#
#   --limit N        Probe at most N URLs. Useful for a local smoke test.
#   --kind KIND      canonical, archived, or all. Default all.
#   --delay SECONDS  Delay between probes. Default 2.
#   --timeout SEC    Per request timeout. Default 30.
#   --registry DIR   Registry directory. Default docs/references.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REGISTRY_DIR="$REPO_ROOT/docs/references"
LIMIT=0
KIND="all"
DELAY=2
TIMEOUT=30

while [ $# -gt 0 ]; do
    case "$1" in
        --limit) LIMIT="$2"; shift 2 ;;
        --kind) KIND="$2"; shift 2 ;;
        --delay) DELAY="$2"; shift 2 ;;
        --timeout) TIMEOUT="$2"; shift 2 ;;
        --registry) REGISTRY_DIR="$2"; shift 2 ;;
        -h|--help) sed -n '2,40p' "${BASH_SOURCE[0]}"; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
done

case "$KIND" in
    canonical|archived|all) ;;
    *) echo "--kind must be canonical, archived, or all" >&2; exit 2 ;;
esac

COMPANIONS="README.md INDEX.md GLOSSARY.md VERIFICATION-MAP.md"

probed=0
alive=0
warned=0
dead=0
deadlist=()

trim() {
    local v="$1"
    v="${v#"${v%%[![:space:]]*}"}"
    v="${v%"${v##*[![:space:]]}"}"
    printf '%s' "$v"
}

frontmatter() {
    awk 'NR==1 && $0!="---" { exit 1 }
         NR==1 { next }
         $0=="---" { exit }
         { print }' "$1"
}

value_of() {
    awk -v key="$1" '
        index($0, key ":") == 1 {
            sub(/^[^:]*:[ \t]*/, "")
            print
            exit
        }'
}

# Probe one URL. Prints "<http-code> <method>"; an unreachable host prints
# "000 <method>". Tries HEAD first, because it is the cheapest question that
# answers "is this still there", and falls back to a one byte ranged GET for
# the servers that refuse HEAD.
probe() {
    local url="$1" code
    code="$(curl -sS -o /dev/null -w '%{http_code}' \
        --max-time "$TIMEOUT" -L -I \
        -A 'littlefs2-pure reference rot check (+https://github.com/ixmatus/littlefs2-pure)' \
        "$url" 2>/dev/null)"

    case "$code" in
        000|403|405|501)
            local get_code
            get_code="$(curl -sS -o /dev/null -w '%{http_code}' \
                --max-time "$TIMEOUT" -L -r 0-0 \
                -A 'littlefs2-pure reference rot check (+https://github.com/ixmatus/littlefs2-pure)' \
                "$url" 2>/dev/null)"
            printf '%s ranged-GET' "$get_code"
            return
            ;;
    esac
    printf '%s HEAD' "$code"
}

echo "Reference link rot probe: $REGISTRY_DIR"
echo "kind=$KIND limit=${LIMIT:-none} delay=${DELAY}s timeout=${TIMEOUT}s"
echo

for entry in "$REGISTRY_DIR"/*.md; do
    name="$(basename "$entry")"
    case " $COMPANIONS " in
        *" $name "*) continue ;;
    esac

    block="$(frontmatter "$entry")"

    for key in canonical archived; do
        if [ "$KIND" != "all" ] && [ "$KIND" != "$key" ]; then
            continue
        fi

        url="$(trim "$(printf '%s\n' "$block" | value_of "$key")")"

        # Registry values are prose as often as they are URLs: an internal
        # artifact records a repository path, an unarchivable source records
        # `none` with a reason. Only http(s) values are probeable.
        case "$url" in
            http://*|https://*) ;;
            *) continue ;;
        esac
        # Strip a trailing parenthetical note if one follows the URL.
        url="${url%% *}"

        if [ "$LIMIT" -gt 0 ] && [ "$probed" -ge "$LIMIT" ]; then
            break 2
        fi

        result="$(probe "$url")"
        code="${result%% *}"
        method="${result##* }"
        probed=$((probed + 1))

        case "$code" in
            404|410)
                verdict="DEAD"
                dead=$((dead + 1))
                deadlist+=("$name [$key] HTTP $code  $url")
                ;;
            000)
                verdict="DEAD"
                dead=$((dead + 1))
                deadlist+=("$name [$key] unreachable (DNS or connection failure)  $url")
                ;;
            2*|3*)
                verdict="ok"
                alive=$((alive + 1))
                ;;
            *)
                verdict="warn"
                warned=$((warned + 1))
                ;;
        esac

        printf '%-6s %-3s %-11s %s\n    %s\n' "$verdict" "$code" "$method" "$name [$key]" "$url"

        # Be a good citizen, and gentler still with the archive.
        case "$url" in
            *web.archive.org*) sleep "$((DELAY * 2))" ;;
            *) sleep "$DELAY" ;;
        esac
    done
done

echo
echo "-----------------------------------------------------------------------"
printf 'probed %d | alive %d | warnings %d | dead %d\n' "$probed" "$alive" "$warned" "$dead"

if [ "$dead" -gt 0 ]; then
    echo
    echo "Dead references:" >&2
    for d in "${deadlist[@]}"; do
        echo "  $d" >&2
    done
    echo >&2
    echo "A dead canonical URL needs the archived copy promoted or a new pointer recorded." >&2
    echo "A dead archived URL needs a fresh Wayback save in the same slice that notices it." >&2
    exit 1
fi

echo "No hard link rot detected."
