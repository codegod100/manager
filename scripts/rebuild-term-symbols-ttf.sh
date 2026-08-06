#!/usr/bin/env bash
# Rebuild assets/term-symbols.ttf for the egui_term fallback family.
#
# Sources:
#   - DejaVu Sans → full Braille Patterns block (U+2800–U+28FF) for
#     agent ink spinner frames (“Running … tok”)
#   - Noto Sans Symbols → ⌕ (U+2315) for “Find …” tool rows
#
# unitsPerEm differ, so ⌕ is outline-copied + scaled into the DejaVu subset
# (fontTools merge rejects the mismatch).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/assets/term-symbols.ttf"

find_dejavu() {
  if [[ -n "${DEJAVU_SANS:-}" && -f "$DEJAVU_SANS" ]]; then
    echo "$DEJAVU_SANS"
    return
  fi
  local candidates=(
    /nix/store/*-dejavu-fonts-*/share/fonts/truetype/DejaVuSans.ttf
    /run/current-system/sw/share/fonts/truetype/DejaVuSans.ttf
    /usr/share/fonts/truetype/dejavu/DejaVuSans.ttf
    /usr/share/fonts/TTF/DejaVuSans.ttf
  )
  # shellcheck disable=SC2068
  for f in ${candidates[@]}; do
    if [[ -f "$f" ]]; then
      echo "$f"
      return
    fi
  done
  if command -v fc-list >/dev/null 2>&1; then
    fc-list "DejaVu Sans:style=Book" file 2>/dev/null | head -1 | cut -d: -f1
    return
  fi
  return 1
}

find_noto_symbols() {
  if [[ -n "${NOTO_SANS_SYMBOLS:-}" && -f "$NOTO_SANS_SYMBOLS" ]]; then
    echo "$NOTO_SANS_SYMBOLS"
    return
  fi
  local candidates=(
    /nix/store/*-noto-fonts-*/share/fonts/noto/NotoSansSymbols.ttf
    /run/current-system/sw/share/fonts/noto/NotoSansSymbols.ttf
    /usr/share/fonts/noto/NotoSansSymbols.ttf
    /usr/share/fonts/truetype/noto/NotoSansSymbols-Regular.ttf
  )
  # shellcheck disable=SC2068
  for f in ${candidates[@]}; do
    if [[ -f "$f" ]]; then
      echo "$f"
      return
    fi
  done
  if command -v fc-list >/dev/null 2>&1; then
    fc-list "Noto Sans Symbols:style=Regular" file 2>/dev/null | head -1 | cut -d: -f1
    return
  fi
  return 1
}

DEJAVU="$(find_dejavu || true)"
NOTO="$(find_noto_symbols || true)"
if [[ -z "$DEJAVU" || ! -f "$DEJAVU" ]]; then
  echo "error: DejaVu Sans not found; set DEJAVU_SANS=/path/to/DejaVuSans.ttf" >&2
  exit 1
fi
if [[ -z "$NOTO" || ! -f "$NOTO" ]]; then
  echo "error: Noto Sans Symbols not found; set NOTO_SANS_SYMBOLS=/path/to/NotoSansSymbols.ttf" >&2
  exit 1
fi

if ! command -v python3 >/dev/null 2>&1; then
  echo "error: python3 required" >&2
  exit 1
fi

echo "dejavu: $DEJAVU"
echo "noto:   $NOTO"
echo "output: $OUT"

DEJAVU="$DEJAVU" NOTO="$NOTO" OUT="$OUT" python3 <<'PY'
import os
from fontTools.ttLib import TTFont
from fontTools.subset import Subsetter, Options
from fontTools.pens.ttGlyphPen import TTGlyphPen
from fontTools.pens.transformPen import TransformPen

dejavu = os.environ["DEJAVU"]
noto = os.environ["NOTO"]
out = os.environ["OUT"]

opts = Options()
opts.layout_features = []
opts.glyph_names = True
opts.notdef_glyph = True
opts.notdef_outline = True
opts.recommended_glyphs = True
opts.name_IDs = ["*"]
opts.name_legacy = True
opts.name_languages = ["*"]
opts.legacy_cmap = True
opts.symbol_cmap = True

base = TTFont(dejavu)
sub = Subsetter(options=opts)
sub.populate(unicodes=set(range(0x2800, 0x2900)))
sub.subset(base)
for tag in ("MATH", "SVG ", "COLR", "CPAL", "CBDT", "CBLC", "sbix"):
    if tag in base:
        del base[tag]

donor = TTFont(noto)
donor_cmap = donor.getBestCmap()
if 0x2315 not in donor_cmap:
    raise SystemExit("Noto Sans Symbols missing U+2315 ⌕")
donor_name = donor_cmap[0x2315]
scale = base["head"].unitsPerEm / donor["head"].unitsPerEm

pen = TTGlyphPen(None)
donor["glyf"][donor_name].draw(TransformPen(pen, (scale, 0, 0, scale, 0, 0)), donor["glyf"])
new_name = "uni2315"
base["glyf"][new_name] = pen.glyph()
src_w = donor["hmtx"][donor_name][0]
base["hmtx"][new_name] = (int(round(src_w * scale)), 0)

order = base.getGlyphOrder()
if new_name not in order:
    order.append(new_name)
    base.setGlyphOrder(order)

for table in base["cmap"].tables:
    if table.isUnicode():
        table.cmap[0x2315] = new_name

base.save(out)
cmap = base.getBestCmap() or {}
assert 0x2315 in cmap
assert all(cp in cmap for cp in range(0x2800, 0x2900))
print(f"wrote {os.path.getsize(out)} bytes (braille=256 + ⌕)")
PY
