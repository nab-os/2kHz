#!/usr/bin/env bash
#
# Build a .deb from a staging root, adding the Maintainer and Depends the
# Dioxus bundler leaves blank. Depends comes from dpkg-shlibdeps, not a fixed
# list, 24.04's time_t transition renamed libgtk-3-0 and friends to -t64.
#
# Version is set here too when given: dx stamps the .deb from Cargo.toml, which
# is not bumped per commit, so the package would claim 0.1.0 whatever the
# filename says.
#
# Usage: finish-deb.sh <staging-root> <output.deb> <maintainer> [version]

set -euo pipefail

ROOT=${1:?usage: finish-deb.sh <staging-root> <output.deb> <maintainer> [version]}
OUT=${2:?missing output path}
MAINTAINER=${3:?missing maintainer}
VERSION=${4:-}

[ -f "$ROOT/DEBIAN/control" ] || { echo "no DEBIAN/control under $ROOT" >&2; exit 1; }

# dpkg-shlibdeps refuses to run outside something shaped like a source tree.
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$WORK/debian"
printf 'Source: x\nPackage: x\nArchitecture: any\n' > "$WORK/debian/control"

BINS=()
while IFS= read -r -d '' f; do
  case "$(file -b --mime-type "$f")" in
    application/x-executable | application/x-pie-executable | application/x-sharedlib)
      BINS+=("$(readlink -f "$f")") ;;
  esac
done < <(find "$ROOT/usr" -type f -perm -u+x -print0)

[ ${#BINS[@]} -gt 0 ] || { echo "no ELF binaries under $ROOT/usr" >&2; exit 1; }

echo "  scanning: $(printf '%s ' "${BINS[@]##*/}")"
DEPENDS=$(cd "$WORK" && dpkg-shlibdeps -O --ignore-missing-info "${BINS[@]}" 2>/dev/null \
          | sed 's/^shlibs:Depends=//')
[ -n "$DEPENDS" ] || { echo "dpkg-shlibdeps produced no dependencies" >&2; exit 1; }
echo "  depends:  $DEPENDS"

CONTROL="$ROOT/DEBIAN/control" DEPENDS="$DEPENDS" MAINTAINER="$MAINTAINER" \
VERSION="$VERSION" python3 - <<'PY'
import os

path = os.environ["CONTROL"]
fields = {"Maintainer": os.environ["MAINTAINER"], "Depends": os.environ["DEPENDS"]}
if os.environ.get("VERSION"):
    fields["Version"] = os.environ["VERSION"]

out, seen = [], set()
for line in open(path).read().splitlines():
    # Continuation lines start with whitespace and belong to the field above,
    # so only a line starting at column 0 introduces a new field.
    key = line.split(":", 1)[0] if ":" in line and not line[:1].isspace() else None
    if key in fields:
        if key not in seen:          # replace the first occurrence in place,
            out.append(f"{key}: {fields[key]}")
            seen.add(key)
        continue                     # and drop any later duplicate
    out.append(line)

# Valid even if the file ends inside Description: a line at column 0 closes it.
for k, v in fields.items():
    if k not in seen:
        out.append(f"{k}: {v}")

open(path, "w").write("\n".join(out) + "\n")
PY

mkdir -p "$(dirname "$OUT")"
dpkg-deb --build --root-owner-group "$ROOT" "$OUT" >/dev/null
echo "  built:    $OUT ($(du -h "$OUT" | cut -f1))"
