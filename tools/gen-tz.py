#!/usr/bin/env python3
"""Generate the embedded time-zone table for cron.rs and Cron.cs.

Every IANA zone (and backward-compatible link name) maps to the POSIX TZ rule at the end of
its TZif file: the rule tzdata uses for all times after the zone's last listed transition.
Cron only schedules future times, so that rule is what matters. Run it after a tzdata update:

    python3 tools/gen-tz.py            # rewrites the GENERATED TZ blocks in both ports

Reads /usr/share/zoneinfo (override with ZONEINFO=...).
"""
import os, re, struct, sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import posix_tz

# Explicit transitions are kept from here on (2025-01-01) for zones whose tzdata lists
# transitions the footer rule can't express (e.g. Morocco's Ramadan changes).
CUTOFF = 1735689600

ROOT = os.environ.get('ZONEINFO', '/usr/share/zoneinfo')
HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)

def read_tzif(path):
    """(footer rule, [(transition_time, utc_offset_after)], [type offsets]) from a v2+ TZif file."""
    with open(path, 'rb') as f:
        data = f.read()
    if not data.startswith(b'TZif') or data[4:5] in (b'\0',):
        return None
    def counts(off):
        return struct.unpack('>6l', data[off + 20:off + 44])
    isut, isstd, leap, timecnt, typecnt, charcnt = counts(0)
    off = 44 + timecnt * 5 + typecnt * 6 + charcnt + leap * 8 + isstd + isut
    isut, isstd, leap, timecnt, typecnt, charcnt = counts(off)
    off += 44
    times = struct.unpack(f'>{timecnt}q', data[off:off + timecnt * 8]); off += timecnt * 8
    idx = data[off:off + timecnt]; off += timecnt
    types = [struct.unpack('>lBB', data[off + 6 * i:off + 6 * i + 6]) for i in range(typecnt)]
    off += typecnt * 6 + charcnt + leap * 12 + isstd + isut
    rule = data[off:].strip(b'\n').decode('ascii')
    if not rule:
        return None
    return rule, [(t, types[i][0]) for t, i in zip(times, idx)], types

def explicit_after_cutoff(rule, trans, types):
    """None if the footer rule matches tzdata from CUTOFF on; else (offset at CUTOFF, transitions)."""
    r = posix_tz.parse(rule)
    before = [o for t, o in trans if t < CUTOFF]
    initial = before[-1] if before else types[0][0]
    after = [(t, o) for t, o in trans if t >= CUTOFF]
    ok = posix_tz.offset_at(r, CUTOFF) == initial and all(
        posix_tz.offset_at(r, t) == o and posix_tz.offset_at(r, t - 1) == (after[i - 1][1] if i else initial)
        for i, (t, o) in enumerate(after))
    return None if ok else (initial, after)

zones = {}
explicit = {}
for dirpath, dirnames, filenames in os.walk(ROOT):
    rel = os.path.relpath(dirpath, ROOT)
    if rel.split(os.sep)[0] in ('posix', 'right'):
        continue
    for fn in filenames:
        name = fn if rel == '.' else os.path.join(rel, fn)
        if name in ('posixrules', 'localtime', 'Factory') or '.' in fn:
            continue
        tz = read_tzif(os.path.join(dirpath, fn))
        if tz:
            zones[name] = tz[0]
            ex = explicit_after_cutoff(*tz)
            if ex:
                explicit[name] = ex

version = 'unknown'
zi = os.path.join(ROOT, 'tzdata.zi')
if os.path.exists(zi):
    with open(zi) as f:
        for line in f:
            if line.startswith('# version'):
                version = line.split()[-1]
            parts = line.split()
            if parts and parts[0] == 'L' and parts[1] in zones and parts[2] not in zones:
                zones[parts[2]] = zones[parts[1]]
                if parts[1] in explicit:
                    explicit[parts[2]] = explicit[parts[1]]

rules = sorted(set(zones.values()))
index = {r: i for i, r in enumerate(rules)}
names = sorted(zones, key=lambda n: n.lower())
print(f'tzdata {version}: {len(names)} names, {len(rules)} distinct rules, explicit transitions for {sorted(explicit)}', file=sys.stderr)
ex_names = sorted(explicit, key=lambda n: n.lower())

def chunk(items, width=110, indent='    '):
    lines, cur = [], indent
    for it in items:
        if len(cur) + len(it) + 1 > width and cur.strip():
            lines.append(cur.rstrip())
            cur = indent
        cur += it + ' '
    if cur.strip():
        lines.append(cur.rstrip())
    return '\n'.join(lines)

# Rust
rs_rules = chunk([f'"{r}",' for r in rules])
rs_names = chunk([f'("{n.lower()}", {index[zones[n]]}),' for n in names])
rust = (f'// BEGIN GENERATED TZ (tools/gen-tz.py, tzdata {version})\n'
        f'/// IANA tzdata version the zone table was generated from.\n'
        f'pub const TZDATA_VERSION: &str = "{version}";\n'
        f'#[rustfmt::skip]\nconst TZ_RULES: &[&str] = &[\n{rs_rules}\n];\n'
        f'/// Lowercase zone names (sorted) and their index into `TZ_RULES`.\n'
        f'#[rustfmt::skip]\nconst TZ_NAMES: &[(&str, u16)] = &[\n{rs_names}\n];\n'
        f'/// Zones whose tzdata transitions from 2025 on aren\'t expressible by their rule:\n'
        f'/// (name, UTC offset at 2025-01-01, [(transition unix seconds, UTC offset after)]).\n'
        f'/// The rule in `TZ_RULES` applies after the last transition.\n'
        f'type TzExplicit = (&\'static str, i32, &\'static [(i64, i32)]);\n'
        f'#[rustfmt::skip]\nconst TZ_EXPLICIT: &[TzExplicit] = &[\n'
        + ''.join(f'    ("{n.lower()}", {explicit[n][0]}, &[\n' + chunk([f'({t}, {o}),' for t, o in explicit[n][1]], indent='        ') + '\n    ]),\n' for n in ex_names)
        + f'];\n'
        f'// END GENERATED TZ')

# C#
cs_rules = chunk([f'"{r}",' for r in rules], indent='        ')
cs_names = chunk([f'"{n.lower()}",' for n in names], indent='        ')
cs_index = chunk([f'{index[zones[n]]},' for n in names], indent='        ')
cs = (f'    // BEGIN GENERATED TZ (tools/gen-tz.py, tzdata {version})\n'
      f'    /// <summary>IANA tzdata version the zone table was generated from.</summary>\n'
      f'    public const string TzdataVersion = "{version}";\n'
      f'    static readonly string[] TzRules =\n    {{\n{cs_rules}\n    }};\n'
      f'    /// <summary>Lowercase zone names (sorted ordinally).</summary>\n'
      f'    static readonly string[] TzNames =\n    {{\n{cs_names}\n    }};\n'
      f'    /// <summary>Index into TzRules for each entry of TzNames.</summary>\n'
      f'    static readonly ushort[] TzNameRule =\n    {{\n{cs_index}\n    }};\n'
      f'    /// <summary>Zones whose tzdata transitions from 2025 on aren\'t expressible by their rule.</summary>\n'
      f'    static readonly (string Name, int Initial, (long At, int Offset)[] Transitions)[] TzExplicit =\n    {{\n'
      + ''.join(f'        ("{n.lower()}", {explicit[n][0]}, new (long, int)[]\n        {{\n' + chunk([f'({t}, {o}),' for t, o in explicit[n][1]], indent='            ') + '\n        }),\n' for n in ex_names)
      + f'    }};\n'
      f'    // END GENERATED TZ')

pat = re.compile(r'[ \t]*// BEGIN GENERATED TZ.*?// END GENERATED TZ', re.S)
for path, block in ((os.path.join(REPO, 'cron/rust/cron.rs'), rust), (os.path.join(REPO, 'cron/csharp/Cron.cs'), cs)):
    if not os.path.exists(path):
        continue
    src = open(path).read()
    if not pat.search(src):
        sys.exit(f'{path}: no GENERATED TZ block')
    open(path, 'w').write(pat.sub(lambda _: block, src, count=1))
    print(f'updated {os.path.relpath(path, REPO)}', file=sys.stderr)
