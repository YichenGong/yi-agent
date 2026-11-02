#!/usr/bin/env python3
"""把指定顶层项改成 pub(crate)，只加修饰符，不动其它字符。

用法: promote.py --file F --names a,b,c
对 impl 块：跳过（impl 没有可见性修饰符），并打印 skipped。
"""
import argparse, re, pathlib, sys

RX = re.compile(
    r'^(pub(?:\((?:crate|super)\))?\s+)?(?:async\s+)?fn\s+(\w+)'
    r'|^(pub(?:\((?:crate|super)\))?\s+)?type\s+(\w+)'
    r'|^(pub(?:\((?:crate|super)\))?\s+)?(?:struct|enum|union)\s+(\w+)'
    r'|^(pub(?:\((?:crate|super)\))?\s+)?(?:const|static)\s+(\w+)'
)

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--file', required=True); ap.add_argument('--names', required=True)
    a = ap.parse_args()
    p = pathlib.Path(a.file)
    lines = p.read_text().splitlines()
    want = [n.strip() for n in a.names.split(',') if n.strip()]

    hit = {n: 0 for n in want}
    for i, l in enumerate(lines):
        m = RX.match(l)
        if not m:
            continue
        name = next(g for g in (m.group(2), m.group(4), m.group(6), m.group(8)) if g)
        if name not in want:
            continue
        hit[name] += 1
        if m.group(1) or m.group(3) or m.group(5) or m.group(7):
            continue  # 已有可见性修饰符
        # 在 'fn|type|struct|enum|union|const|static' 之前插入 pub(crate)
        lines[i] = re.sub(r'\b(fn|type|struct|enum|union|const|static)\b',
                          r'pub(crate) \1', l, count=1)

    p.write_text('\n'.join(lines) + '\n')
    bad = 0
    for n in want:
        if hit[n] == 0:
            print(f"WARN: 未找到 {n}（可能是 impl 或 thread_local 块，跳过）", file=sys.stderr)
            bad += 1
        elif hit[n] > 1:
            print(f"ERROR: {n} 命中 {hit[n]} 次，不唯一", file=sys.stderr)
            bad += 1
    print(f"promoted {sum(1 for n in want if hit[n]==1)} / {len(want)} names in {p}")
    sys.exit(1 if bad and any(hit[n] > 1 for n in want) else 0)

if __name__ == '__main__':
    main()
