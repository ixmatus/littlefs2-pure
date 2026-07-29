#!/usr/bin/env bash
#
# Verify the recorded sha256 values in docs/references/ against the files
# actually committed in this tree.
#
# This is the deterministic half of the registry rot check: it touches no
# network, so it runs on every push. The nondeterministic half, probing
# whether canonical and archived URLs still resolve, lives in
# tools/probe_reference_links.sh and runs on a schedule instead, because a
# network flake must never be able to fail an unrelated build.
#
# The schema this reads is documented in docs/references/README.md. Three
# frontmatter keys matter here:
#
#   vendor_status  vendored-at-path <path> | pointer-only | legally-cannot
#                  | paper-copy-owned
#   sha256         none, or one or more hashes, optionally named and
#                  optionally followed by a parenthetical note
#   provenance     primary | secondary | internal
#
# What each verdict means:
#
#   ok        A recorded hash was checked against the file on disk and
#             matched. This is the only verdict that proves anything.
#   skipped   The entry is pointer-only, so there is nothing in the tree to
#             hash. A hash recorded for a pointer-only source (the retrieved
#             PDF of a paper, say) is reported but cannot be verified here:
#             the artifact is deliberately not vendored.
#   exempt    The entry has internal provenance and points at one of this
#             repository's own living documents. Those change under normal
#             work, so a content hash would fail on every unrelated edit;
#             git history is their integrity mechanism, not this script.
#             Recorded hashes are still verified when present, which is why
#             the conformance vector corpus (also internal provenance) is
#             checked rather than exempted.
#   FAILED    A hash mismatched, a recorded file is missing, or an external
#             source was vendored into the tree without recording a hash.
#             Any failure exits nonzero.
#
# Usage: tools/check_references.sh [registry-dir]

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REGISTRY_DIR="${1:-$REPO_ROOT/docs/references}"

COMPANIONS="README.md INDEX.md GLOSSARY.md VERIFICATION-MAP.md"

ok_count=0
skipped_count=0
exempt_count=0
fail_count=0
failures=()

# Hash a file, printing the bare lowercase hex digest.
hash_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    else
        shasum -a 256 "$1" | cut -d' ' -f1
    fi
}

# Print the frontmatter block of an entry: the lines between the opening
# `---` and the next one.
frontmatter() {
    awk 'NR==1 && $0!="---" { exit 1 }
         NR==1 { next }
         $0=="---" { exit }
         { print }' "$1"
}

# Print the value of a top level `key:` line from stdin.
value_of() {
    awk -v key="$1" '
        index($0, key ":") == 1 {
            sub(/^[^:]*:[ \t]*/, "")
            print
            exit
        }'
}

# Strip leading and trailing whitespace. Done in bash rather than with sed
# because a `[ \t]` bracket expression means "space, backslash, or t" to BSD
# sed, which silently eats a leading `t` from paths like `tools/...`.
trim() {
    local v="$1"
    v="${v#"${v%%[![:space:]]*}"}"
    v="${v%"${v##*[![:space:]]}"}"
    printf '%s' "$v"
}

report() {
    printf '  %-9s %s\n' "$1" "$2"
}

fail() {
    report "FAILED" "$1"
    failures+=("$2: $1")
    fail_count=$((fail_count + 1))
}

echo "Registry hash verification: $REGISTRY_DIR"
echo

entry_count=0
for entry in "$REGISTRY_DIR"/*.md; do
    name="$(basename "$entry")"
    case " $COMPANIONS " in
        *" $name "*) continue ;;
    esac
    entry_count=$((entry_count + 1))

    block="$(frontmatter "$entry")"
    vendor_status="$(printf '%s\n' "$block" | value_of vendor_status)"
    provenance="$(printf '%s\n' "$block" | value_of provenance)"
    raw_sha="$(printf '%s\n' "$block" | value_of sha256)"

    echo "$name"

    # Normalize the sha256 value: drop surrounding quotes, then drop every
    # parenthetical note so the remaining commas separate hash entries.
    sha="${raw_sha%\"}"
    sha="${sha#\"}"
    sha="$(printf '%s' "$sha" | sed 's/([^)]*)//g')"

    has_hashes=0
    if [ -n "$sha" ] && [ "$sha" != "none" ]; then
        has_hashes=1
    fi

    # The statuses that vendor nothing have nothing in the tree to hash.
    case "$vendor_status" in
        vendored-at-path*) ;;
        *)
            if [ "$has_hashes" -eq 1 ]; then
                report "skipped" "$vendor_status; hash recorded for a source that is not vendored, not verifiable here"
            else
                report "skipped" "$vendor_status; nothing vendored in this tree"
            fi
            skipped_count=$((skipped_count + 1))
            echo
            continue
            ;;
    esac

    target="$(trim "${vendor_status#vendored-at-path }")"
    target_path="$REPO_ROOT/$target"

    if [ ! -e "$target_path" ]; then
        fail "vendored path \`$target\` does not exist" "$name"
        echo
        continue
    fi

    if [ "$has_hashes" -eq 0 ]; then
        if [ "$provenance" = "internal" ]; then
            report "exempt" "internal provenance; \`$target\` is a living document in this repository, versioned by git"
            exempt_count=$((exempt_count + 1))
        else
            fail "\`$target\` is vendored with provenance \`$provenance\` but records no sha256" "$name"
        fi
        echo
        continue
    fi

    # Split the normalized value on commas. Each item is either a bare
    # digest (a single vendored file) or `<filename> <digest>`.
    old_ifs="$IFS"
    IFS=','
    for item in $sha; do
        IFS="$old_ifs"
        item="$(trim "$item")"
        [ -z "$item" ] && continue

        file_name=""
        digest=""
        case "$item" in
            *' '*)
                file_name="${item%% *}"
                digest="${item##* }"
                ;;
            *)
                digest="$item"
                ;;
        esac

        if ! printf '%s' "$digest" | grep -Eq '^[0-9a-f]{64}$'; then
            fail "\`$item\` is not a recognizable sha256 record" "$name"
            IFS=','
            continue
        fi

        if [ -n "$file_name" ]; then
            check_path="$target_path/$file_name"
            label="$target/$file_name"
        elif [ -d "$target_path" ]; then
            fail "\`$target\` is a directory but the hash record names no file" "$name"
            IFS=','
            continue
        else
            check_path="$target_path"
            label="$target"
        fi

        if [ ! -f "$check_path" ]; then
            fail "recorded file \`$label\` is missing from the tree" "$name"
            IFS=','
            continue
        fi

        actual="$(hash_file "$check_path")"
        if [ "$actual" != "$digest" ]; then
            fail "\`$label\` hash mismatch: recorded $digest, found $actual" "$name"
        else
            report "ok" "$label"
            ok_count=$((ok_count + 1))
        fi
        IFS=','
    done
    IFS="$old_ifs"
    echo
done

echo "-----------------------------------------------------------------------"
printf 'entries %d | hashes verified %d | skipped %d | exempt %d | failures %d\n' \
    "$entry_count" "$ok_count" "$skipped_count" "$exempt_count" "$fail_count"

if [ "$entry_count" -eq 0 ]; then
    echo "no entries found; the registry path is wrong or the registry is empty" >&2
    exit 1
fi

if [ "$fail_count" -gt 0 ]; then
    echo
    echo "Failures:" >&2
    for f in "${failures[@]}"; do
        echo "  $f" >&2
    done
    exit 1
fi

echo "All recorded hashes match."
