#!/usr/bin/env python3
"""Generate tests/vectors/tz.json: UTC offsets from the system tzdata (via Python's zoneinfo)
for every zone the ports embed, at instants from 2025 to 2032, including both sides of every
transition in that range."""
import json, os, sys
from datetime import datetime, timezone
from zoneinfo import ZoneInfo, available_timezones

START, END = 1735689600, 1956528000  # 2025-01-01 .. 2032-01-01
names = sorted(n for n in available_timezones() if n not in ('Factory', 'localtime'))
cases = []
for name in names:
    try:
        zi = ZoneInfo(name)
    except Exception:
        continue
    instants = set(range(START, END, 86400 * 61 + 3601))
    # Find transitions by scanning daily and bisecting.
    prev_t, prev_o = START, datetime.fromtimestamp(START, zi).utcoffset()
    for t in range(START + 86400, END, 86400):
        o = datetime.fromtimestamp(t, zi).utcoffset()
        if o != prev_o:
            lo, hi = prev_t, t
            while hi - lo > 1:
                mid = (lo + hi) // 2
                if datetime.fromtimestamp(mid, zi).utcoffset() == prev_o:
                    lo = mid
                else:
                    hi = mid
            instants.update({lo, hi})
        prev_t, prev_o = t, o
    for t in sorted(instants):
        cases.append([name, t, int(datetime.fromtimestamp(t, zi).utcoffset().total_seconds())])
out = os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', 'tests', 'vectors', 'tz.json')
with open(out, 'w') as f:
    json.dump({'cases': cases}, f, separators=(',', ':'))
print(f'{len(cases)} offsets for {len(names)} zones', file=sys.stderr)
