#!/usr/bin/env bash
#
# Validates and normalizes a single release version for the release workflow. It strips an optional
# leading v, prints the normalized X.Y.Z[-prerelease] to stdout, and exits nonzero with a concise
# stderr message for anything else. release.yml and scripts/tests/release-version.test.sh both call
# this one helper so the accepted syntax lives in exactly one place.
#
# Build metadata (+build) is rejected on purpose: Homebrew's Version comparison ranks +build tokens
# ABOVE the plain version, which disagrees with SemVer's rule that build metadata is ignored for
# precedence. A version fed to a Homebrew formula must therefore carry a -prerelease suffix only, and
# the release error contract promises exactly that.

set -euo pipefail

die() {
    printf 'refusing release version %s: expected vX.Y.Z or X.Y.Z with an optional -prerelease suffix (no +build metadata)\n' "$1" >&2
    exit 1
}

if [ "$#" -ne 1 ]; then
    printf 'usage: %s <version>\n' "$0" >&2
    exit 2
fi

raw="$1"
version="${raw#v}"

reviewer_re='^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$'
printf '%s' "$version" | grep -Eq "$reviewer_re" || die "'$raw'"

# The floor regex still accepts a leading, trailing, or doubled dot in the prerelease (1.2.3-. or
# 1.2.3-a..b), but SemVer forbids empty dot-separated identifiers. Wrapping the prerelease in dots
# turns any empty identifier into a "..", so a single test rejects all three shapes at once.
case "$version" in
    *-*)
        prerelease="${version#*-}"
        case ".$prerelease." in
            *..*) die "'$raw'" ;;
        esac
        ;;
esac

printf '%s\n' "$version"
