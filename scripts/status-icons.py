# /// script
# dependencies = ["shapely"]
# ///
# The menu bar icons as one big, centred "w" with each state's mark laid over its top-right
# corner: the mark is drawn on top and the "w" is cut away around it (a gap), as app badges do.
# The marks are as big as the corner holds, to read at 18 points. From wavo-gui.
#   uv run scripts/status-icons.py assets/status-icon/idle.svg assets/status-icon
import re
import sys

from shapely import affinity
from shapely.geometry import Point, Polygon, box
from shapely.ops import unary_union

SRC, OUT = sys.argv[1], sys.argv[2]
GAP = 1.0  # the cut around a mark, in units of the 18-unit box

d = re.findall(r'd="([^"]+)"', open(SRC).read())[0]
toks = re.findall(r"[MLCZ]|-?\d+\.?\d*", d)


def bez(p0, p1, p2, p3, t):
  u = 1 - t
  return tuple(u**3 * p0[k] + 3 * u * u * t * p1[k] + 3 * u * t * t * p2[k] + t**3 * p3[k] for k in (0, 1))


# The source "w" as rings of points (each M starts one).
rings, ring, cur, i = [], [], None, 0
while i < len(toks):
  c = toks[i]
  i += 1
  if c == "M":
    if ring:
      rings.append(ring)
    cur = (float(toks[i]), float(toks[i + 1]))
    ring = [cur]
    i += 2
  elif c == "L":
    cur = (float(toks[i]), float(toks[i + 1]))
    ring.append(cur)
    i += 2
  elif c == "C":
    v = [float(t) for t in toks[i : i + 6]]
    i += 6
    a, b, e = (v[0], v[1]), (v[2], v[3]), (v[4], v[5])
    ring += [bez(cur, a, b, e, k / 24) for k in range(1, 25)]
    cur = e
if ring:
  rings.append(ring)

w = unary_union([Polygon(r).buffer(0) for r in rings])
x0, y0, x1, y1 = w.bounds
# Full size: the source's width, centred in the box.
w = affinity.translate(w, 9 - (x0 + x1) / 2, 9 - (y0 + y1) / 2)
print("w bounds", [round(v, 2) for v in w.bounds])


def pill(x, y0, y1, r):
  return unary_union([Point(x, y0 + r).buffer(r, 32), Point(x, y1 - r).buffer(r, 32), box(x - r, y0 + r, x + r, y1 - r)])


# The marks, centred on the top-right corner of the box.
CX, CY = 14.4, 3.6
marks = {
  "idle": None,
  "recording": Point(CX, CY).buffer(3.4, 48),
  "transcribing": unary_union([Point(x, 1.9).buffer(1.4, 32) for x in (10.2, 13.4, 16.55)]),
  # The recording's dot with "!" cut out of it.
  "warning": Point(CX, CY).buffer(3.4, 48).difference(
    unary_union([pill(CX, 1.2, 4.5, 0.62), Point(CX, 5.65).buffer(0.66, 32)])
  ),
}


def path(geom):
  polys = getattr(geom, "geoms", [geom])
  out = []
  for p in polys:
    for r in [p.exterior, *p.interiors]:
      pts = list(r.simplify(0.02).coords)[:-1]
      out.append("M " + " L ".join(f"{x:.2f} {y:.2f}" for x, y in pts) + " Z")
  return " ".join(out)


for name, mark in marks.items():
  body = w if mark is None else w.difference(mark.buffer(GAP, 32))
  parts = [path(body)] + ([path(mark)] if mark is not None else [])
  for g in [body] + ([mark] if mark is not None else []):
    lo = min(g.bounds[:2])
    hi = max(g.bounds[2:])
    assert lo >= 0 and hi <= 18, f"{name} leaves the box: {g.bounds}"
  svg = '<svg xmlns="http://www.w3.org/2000/svg" width="18" height="18" viewBox="0 0 18 18">\n'
  svg += "".join(f'  <path d="{p}" fill-rule="evenodd"/>\n' for p in parts) + "</svg>\n"
  open(f"{OUT}/{name}.svg", "w").write(svg)
  print(name, "ok")
