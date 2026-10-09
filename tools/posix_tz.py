"""POSIX TZ rule evaluation (the same algorithm cron.rs and Cron.cs implement).

offset_at(rule, t) -> UTC offset in seconds (east positive) at Unix time t.
"""
import re

def days_from_civil(y, m, d):
    y -= m <= 2
    era = (y if y >= 0 else y - 399) // 400
    yoe = y - era * 400
    doy = (153 * (m + (-3 if m > 2 else 9)) + 2) // 5 + d - 1
    doe = yoe * 365 + yoe // 4 - yoe // 100 + doy
    return era * 146097 + doe - 719468

def civil_from_days(z):
    z += 719468
    era = (z if z >= 0 else z - 146096) // 146097
    doe = z - era * 146097
    yoe = (doe - doe // 1460 + doe // 36524 - doe // 146096) // 365
    y = yoe + era * 400
    doy = doe - (365 * yoe + yoe // 4 - yoe // 100)
    mp = (5 * doy + 2) // 153
    d = doy - (153 * mp + 2) // 5 + 1
    m = mp + (3 if mp < 10 else -9)
    return (y + (m <= 2), m, d)

def is_leap(y):
    return y % 4 == 0 and (y % 100 != 0 or y % 400 == 0)

def parse(rule):
    i = 0
    s = rule
    def name():
        nonlocal i
        if s[i] == '<':
            j = s.index('>', i)
            n = s[i + 1:j]; i = j + 1
            return n
        j = i
        while j < len(s) and s[j].isalpha():
            j += 1
        n = s[i:j]; i = j
        return n
    def time(signed=True):
        nonlocal i
        sign = 1
        if signed and i < len(s) and s[i] in '+-':
            sign = -1 if s[i] == '-' else 1
            i += 1
        m = re.match(r'(\d+)(?::(\d+))?(?::(\d+))?', s[i:])
        i += m.end()
        return sign * (int(m.group(1)) * 3600 + int(m.group(2) or 0) * 60 + int(m.group(3) or 0))
    std_name = name()
    std = -time()
    if i == len(s):
        return {'std': std, 'dst': None}
    dst_name = name()
    dst = std + 3600
    if i < len(s) and s[i] not in ',':
        dst = -time()
    rules = []
    while i < len(s) and s[i] == ',':
        i += 1
        if s[i] == 'M':
            m = re.match(r'M(\d+)\.(\d+)\.(\d+)', s[i:]); i += m.end()
            date = ('M', int(m.group(1)), int(m.group(2)), int(m.group(3)))
        elif s[i] == 'J':
            m = re.match(r'J(\d+)', s[i:]); i += m.end()
            date = ('J', int(m.group(1)))
        else:
            m = re.match(r'(\d+)', s[i:]); i += m.end()
            date = ('N', int(m.group(1)))
        tm = 7200
        if i < len(s) and s[i] == '/':
            i += 1
            tm = time()
        rules.append((date, tm))
    if not rules:  # POSIX default (US rules)
        rules = [(('M', 3, 2, 0), 7200), (('M', 11, 1, 0), 7200)]
    return {'std': std, 'dst': dst, 'start': rules[0], 'end': rules[1]}

def rule_day(date, y):
    """Days since epoch of the rule's date in year y."""
    kind = date[0]
    if kind == 'J':
        n = date[1]
        doy = n - 1 + (1 if is_leap(y) and n > 59 else 0)
        return days_from_civil(y, 1, 1) + doy
    if kind == 'N':
        return days_from_civil(y, 1, 1) + date[1]
    _, m, w, d = date
    first = days_from_civil(y, m, 1)
    wd_first = (first + 4) % 7  # 1970-01-01 was a Thursday
    day = first + (d - wd_first) % 7 + (w - 1) * 7
    days_in = days_from_civil(y + (m == 12), m % 12 + 1, 1) - first
    while day >= first + days_in:
        day -= 7
    return day

def transitions(r, y):
    """(start_utc, end_utc) of DST in year y."""
    sd, st = r['start']
    ed, et = r['end']
    start = rule_day(sd, y) * 86400 + st - r['std']
    end = rule_day(ed, y) * 86400 + et - r['dst']
    return start, end

def offset_at(r, t):
    if r['dst'] is None:
        return r['std']
    y = civil_from_days(t // 86400)[0]
    events = []
    for yy in (y - 1, y, y + 1):
        s, e = transitions(r, yy)
        events.append((s, r['dst']))
        events.append((e, r['std']))
    events.sort()
    cur = None
    for when, off in events:
        if when <= t:
            cur = off
    if cur is None:
        # Before the first event: the state that the first event ends.
        cur = r['std'] if events[0][1] == r['dst'] else r['dst']
    return cur
