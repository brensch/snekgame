#!/usr/bin/env bash
# badge.sh LABEL PERCENT > badge.svg — a flat shields-style badge, coloured
# by the percentage, with no outside service involved.
set -euo pipefail
label=$1
value=$(printf '%.0f%%' "$2")
pct=${2%.*}
if   [ "$pct" -ge 80 ]; then colour='#4c1'
elif [ "$pct" -ge 60 ]; then colour='#97ca00'
elif [ "$pct" -ge 40 ]; then colour='#dfb317'
else colour='#e05d44'; fi
# Verdana 11px is ~7px a character; pad each side by 6.
lw=$(( ${#label} * 7 + 12 ))
vw=$(( ${#value} * 7 + 12 ))
w=$(( lw + vw ))
cat <<SVG
<svg xmlns="http://www.w3.org/2000/svg" width="$w" height="20" role="img" aria-label="$label: $value">
  <title>$label: $value</title>
  <linearGradient id="s" x2="0" y2="100%"><stop offset="0" stop-color="#bbb" stop-opacity=".1"/><stop offset="1" stop-opacity=".1"/></linearGradient>
  <clipPath id="r"><rect width="$w" height="20" rx="3" fill="#fff"/></clipPath>
  <g clip-path="url(#r)">
    <rect width="$lw" height="20" fill="#555"/>
    <rect x="$lw" width="$vw" height="20" fill="$colour"/>
    <rect width="$w" height="20" fill="url(#s)"/>
  </g>
  <g fill="#fff" text-anchor="middle" font-family="Verdana,Geneva,DejaVu Sans,sans-serif" font-size="11">
    <text x="$(( lw / 2 ))" y="15" fill="#010101" fill-opacity=".3">$label</text>
    <text x="$(( lw / 2 ))" y="14">$label</text>
    <text x="$(( lw + vw / 2 ))" y="15" fill="#010101" fill-opacity=".3">$value</text>
    <text x="$(( lw + vw / 2 ))" y="14">$value</text>
  </g>
</svg>
SVG
