#!/usr/bin/env python3
"""按「顶层项」搬移 Rust 源文件里的项。

顶层项 = 紧贴其上的 doc/属性注释 + 声明行 + 主体（到列 0 的 '}' 或行尾 ';'）。

用法:
  extract.py --list SRC
  extract.py --src SRC --dst DST --names a,b,c [--header FILE] [--append] [--move]
"""
import argparse, re, sys, pathlib

ITEM_RE = [
    ('fn',     re.compile(r'^(pub(?:\((?:crate|super)\))?\s+)?(?:async\s+)?fn\s+(\w+)')),
    ('type',   re.compile(r'^(pub(?:\((?:crate|super)\))?\s+)?type\s+(\w+)')),
    ('struct', re.compile(r'^(pub(?:\((?:crate|super)\))?\s+)?(?:struct|enum|union)\s+(\w+)')),
    ('const',  re.compile(r'^(pub(?:\((?:crate|super)\))?\s+)?(?:const|static)\s+(\w+)')),
    ('mod',    re.compile(r'^(pub\(crate\)\s+)?mod\s+(\w+);')),
]
IMPL_RE = re.compile(r'^impl\b')
TL_RE = re.compile(r'^thread_local!\s*\{')

ATTR_PREFIX = ('///', '//!', '//', '#[', '#!')

def find_close_brace(lines, start):
    for e in range(start, len(lines)):
        if lines[e] == '}':
            return e
    raise RuntimeError(f'L{start+1}: 找不到列 0 的 }}')

def find_semicolon(lines, start):
    for e in range(start, len(lines)):
        if lines[e].rstrip().endswith(';'):
            return e
    raise RuntimeError(f'L{start+1}: 找不到行尾 ;')

def strip_doc(lines, start):
    s = start
    while s > 0 and lines[s-1].strip().startswith(ATTR_PREFIX):
        s -= 1
    return s

def scan(lines):
    items = []
    for i, l in enumerate(lines):
        kind = name = None
        for k, rx in ITEM_RE:
            m = rx.match(l)
            if m:
                kind, name = k, m.group(2)
                if k == 'struct':
                    kw = re.match(r'^(?:pub(?:\((?:crate|super)\))?\s+)?(struct|enum|union)\b', l)
                    kind = kw.group(1) if kw else 'struct'
                elif k == 'const':
                    kw = re.match(r'^(?:pub(?:\((?:crate|super)\))?\s+)?(const|static)\b', l)
                    kind = kw.group(1) if kw else 'const'
                break
        if kind:
            start = strip_doc(lines, i)
            end = i if kind == 'mod' else (
                find_semicolon(lines, i) if kind in ('type', 'const', 'static') else find_close_brace(lines, i))
            items.append({'kind': kind, 'name': name, 'start': start, 'end': end, 'decl': i})
        elif IMPL_RE.match(l):
            start = strip_doc(lines, i)
            items.append({'kind': 'impl', 'name': 'impl', 'start': start,
                          'end': find_close_brace(lines, i), 'decl': i})
        elif TL_RE.match(l):
            end = find_close_brace(lines, i)
            m = re.search(r'static\s+([A-Z_][A-Z0-9_]*)', '\n'.join(lines[i:end+1]))
            start = strip_doc(lines, i)
            items.append({'kind': 'tl', 'name': m.group(1) if m else f'tl@{i+1}',
                          'start': start, 'end': end, 'decl': i})
    items.sort(key=lambda x: x['start'])
    for a, b in zip(items, items[1:]):
        if a['end'] >= b['start']:
            raise RuntimeError(f"项重叠: {a['kind']} {a['name']} 与 {b['kind']} {b['name']}")
    return items

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--list', metavar='SRC')
    ap.add_argument('--src'); ap.add_argument('--dst')
    ap.add_argument('--names', default=''); ap.add_argument('--header')
    ap.add_argument('--append', action='store_true'); ap.add_argument('--move', action='store_true')
    a = ap.parse_args()

    if a.list:
        lines = pathlib.Path(a.list).read_text().splitlines()
        items = scan(lines)
        from collections import Counter
        c = Counter(i['kind'] for i in items)
        print(f"{len(items)} items  ({' / '.join(f'{k} {v}' for k, v in sorted(c.items()))})")
        if '-v' in sys.argv or '--verbose' in sys.argv:
            for it in items:
                print(f"  {it['kind']:6} {it['name']:38} L{it['start']+1:5}-{it['end']+1:5} "
                      f"{it['end']-it['start']+1:4}")
        return

    src = pathlib.Path(a.src); dst = pathlib.Path(a.dst)
    lines = src.read_text().splitlines()
    items = scan(lines)
    want = [n.strip() for n in a.names.split(',') if n.strip()]

    byname = {}
    for it in items:
        byname.setdefault(it['name'], []).append(it)

    picked, missing = [], []
    for n in want:
        if n in byname:
            picked.append(byname[n][0])
        else:
            missing.append(n)
    if missing:
        print(f"ERROR: 未找到 {missing}", file=sys.stderr)
        sys.exit(1)

    # 自动带上 impl 块：按 impl 头里出现的被搬类型名匹配（不看相邻）。
    # 相邻规则在这里是错的：ServeLauncher 的 impl 夹在 LeadTarget 与
    # LeadTickDelivery 之间，两者都属别的文件，相邻规则会把 impl 带走。
    picked_names = {it['name'] for it in picked if it['kind'] in ('struct', 'type', 'enum')}
    picked_ids = {id(p) for p in picked}
    pulled = []
    for it in items:
        if it['kind'] != 'impl' or id(it) in picked_ids:
            continue
        hdr = lines[it['decl']]
        if any(re.search(r'\b' + re.escape(n) + r'\b', hdr) for n in picked_names):
            picked.append(it); picked_ids.add(id(it)); pulled.append(it)

    picked.sort(key=lambda x: x['start'])
    body = '\n'.join(
        '\n'.join(lines[it['start']:it['end']+1]).rstrip() for it in picked)
    nlines = sum(it['end'] - it['start'] + 1 for it in picked)

    if a.append and dst.exists():
        cur = dst.read_text().rstrip()
        dst.write_text(cur + '\n\n' + body + '\n')
    else:
        head = ''
        if a.header:
            head = pathlib.Path(a.header).read_text().rstrip() + '\n\n'
        dst.write_text(head + body + '\n')

    if a.move:
        kill = set()
        for it in picked:
            kill.update(range(it['start'], it['end']+1))
        keep = [l for i, l in enumerate(lines) if i not in kill]
        # 折叠连续空行
        out, prev_blank = [], False
        for l in keep:
            blank = not l.strip()
            if blank and prev_blank:
                continue
            out.append(l); prev_blank = blank
        src.write_text('\n'.join(out) + '\n')

    print(f"moved {len(picked)} items / {nlines} lines from {src} -> {dst}")
    if pulled:
        print(f"impl items pulled in: {len(pulled)}")
    for it in picked:
        print(f"  {it['kind']:6} {it['name']:38} {it['end']-it['start']+1:4} lines")

if __name__ == '__main__':
    main()
