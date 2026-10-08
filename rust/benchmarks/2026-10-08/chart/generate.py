#!/usr/bin/env python3
"""Rebuild the comparison from retained benchmark summaries; no Python packages."""
import argparse
import hashlib
import html
import json
from pathlib import Path
import subprocess
import tempfile

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
W, H = 1200, 1400
BG, PANEL, BORDER = '#0a0a0a', '#121416', '#25292c'
WHITE, MUTED, GRAY, CYAN = '#f5f6f7', '#929bA2', '#737b83', '#36d9ee'
FONT = 'Helvetica Neue, Helvetica, Arial, sans-serif'
scene = []

def box(x, y, width, height, color, radius=0, stroke=None):
    scene.append(dict(kind='rect', x=x, y=y, width=width, height=height, fill=color, radius=radius, stroke=stroke))

def text(x, y, value, size=16, color=WHITE, weight=400, align='left', kern=0):
    scene.append(dict(kind='text', x=x, y=y, text=value, size=size, fill=color, weight=weight, align=align, kern=kern))

def line(x1, y1, x2, y2, color=BORDER):
    scene.append(dict(kind='line', x1=x1, y1=y1, x2=x2, y2=y2, fill=color))

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--svg-only', action='store_true', help='Skip macOS AppKit PNG renderer')
args = parser.parse_args()
backend_path = ROOT / 'backend' / 'summary.json'
desktop_path = ROOT / 'desktop' / 'comparison.json'
if not desktop_path.exists():
    desktop_path = ROOT / 'desktop' / 'warm-results.json'
backend = json.loads(backend_path.read_text())
desktop = json.loads(desktop_path.read_text())
warm = desktop['runs']['warm']['summary'] if 'runs' in desktop else desktop['summary']
original = next(group for group in backend['groups'] if group['backend'] == 'original-production' and group['phase'] == 'fresh')
rust = next(group for group in backend['groups'] if group['backend'] == 'rust-release' and group['phase'] == 'fresh')
assert original['successfulRounds'] == rust['successfulRounds'] == warm['original']['n'] == warm['rust']['n'] == 10
assert original['rpcThreadProjectionResponseBytes']['median'] == rust['rpcThreadProjectionResponseBytes']['median'] == 1065
assert original['rpcProbeResponseBytes']['median'] == rust['rpcProbeResponseBytes']['median'] == 70

pairs = [
    dict(title='Warm desktop window', detail='Warm profile · native visibility marker', original=warm['original']['medianMs'], rust=warm['rust']['medianMs'], unit='ms', scale=2200, scaleLabel='2.2 s', labels=[f"{warm['original']['medianMs']/1000:.2f} s", f"{warm['rust']['medianMs']/1000:.2f} s"]),
    dict(title='Backend accepted command', detail='Fresh state · authenticated accepted write', original=original['usableCommandReadyMs']['median'], rust=rust['usableCommandReadyMs']['median'], unit='ms', scale=2200, scaleLabel='2.2 s', labels=[f"{original['usableCommandReadyMs']['median']/1000:.2f} s", f"{rust['usableCommandReadyMs']['median']:.0f} ms"]),
    dict(title='Idle backend memory', detail='Backend + native monitor · sum RSS', original=original['idleSumRssKiB']['median']/1024, rust=rust['idleSumRssKiB']['median']/1024, unit='MiB', scale=450, scaleLabel='450 MiB', labels=[f"{original['idleSumRssKiB']['median']/1024:.1f} MiB", f"{rust['idleSumRssKiB']['median']/1024:.1f} MiB"]),
    dict(title='Thread projection RPC', detail='Fresh state · equal 1,065-byte reply', original=original['rpcThreadProjection']['median'], rust=rust['rpcThreadProjection']['median'], unit='ms', scale=1.8, scaleLabel='1.8 ms', labels=[f"{original['rpcThreadProjection']['median']:.2f} ms", f"{rust['rpcThreadProjection']['median']:.2f} ms"]),
    dict(title='Persisted thread create', detail='Fresh state · durable transaction', original=original['threadWrites']['median'], rust=rust['threadWrites']['median'], unit='ms', scale=4, scaleLabel='4 ms', labels=[f"{original['threadWrites']['median']:.2f} ms", f"{rust['threadWrites']['median']:.2f} ms"]),
    dict(title='No-op RPC', detail='Fresh state · equal 70-byte reply', original=original['rpcProbe']['median'], rust=rust['rpcProbe']['median'], unit='ms', scale=0.25, scaleLabel='0.25 ms', labels=[f"{original['rpcProbe']['median']:.3f} ms", f"{rust['rpcProbe']['median']:.3f} ms"]),
]
for pair in pairs:
    pair['ratio'] = pair['original']/pair['rust']
    assert 0 < pair['rust'] <= pair['original'] <= pair['scale']
reduction = 100 * (1-pairs[2]['rust']/pairs[2]['original'])

box(0, 0, W, H, BG)
text(56, 43, 'T3 CODE  /  PERFORMANCE SNAPSHOT', 15, MUTED, 600, kern=1.4)
text(1144, 43, '08 OCT 2026', 15, MUTED, 500, align='right')
text(56, 91, 'The Rust port, measured.', 54, WHITE, 700)
text(56, 162, 'Original production build vs the current Rust port', 22, MUTED)
box(56, 206, 1088, 43, '#0d2226', 8, '#225a63')
text(76, 217, 'INCOMPLETE RUST PORT  ·  NOT FEATURE-EQUIVALENT', 17, CYAN, 600, kern=.6)

heroes = [
    (56, f"{pairs[0]['ratio']:.2f}×", 'faster desktop visibility', 'Warm profiles · UI only'),
    (422, f"{pairs[1]['ratio']:.1f}×", 'faster backend readiness', 'Fresh state · accepted command'),
    (807, f"{reduction:.0f}%", 'lower backend memory', 'Idle backend + native monitor'),
]
for x, headline, caption, note in heroes:
    text(x, 273, headline, 70, WHITE, 700)
    text(x, 353, caption, 18, CYAN, 500)
    text(x, 380, note, 14, MUTED)
line(393, 284, 393, 396)
line(777, 284, 777, 396)
box(56, 420, 17, 8, GRAY, 3)
text(82, 413, 'Original', 16, WHITE)
box(177, 420, 17, 8, CYAN, 3)
text(203, 413, 'Rust port', 16, WHITE)
text(1144, 414, 'LOWER IS BETTER  ·  PER-PANEL SCALES', 13, MUTED, 500, align='right')

for index, pair in enumerate(pairs):
    x = 56 + (index % 2) * 556
    y = 452 + (index // 2) * 246
    box(x, y, 532, 226, PANEL, 12, BORDER)
    text(x+24, y+23, pair['title'], 21, WHITE, 600)
    badge = f"{pair['ratio']:.2f}×" if index != 2 else f"{reduction:.0f}% ↓"
    text(x+508, y+23, badge, 25, CYAN, 600, align='right')
    text(x+24, y+57, pair['detail'], 13, MUTED)
    plot_x, plot_width = x+24, 484
    for series, label_y, bar_y, color, value, label in [
        ('Original', y+89, y+118, GRAY, pair['original'], pair['labels'][0]),
        ('Rust port', y+144, y+173, CYAN, pair['rust'], pair['labels'][1]),
    ]:
        text(plot_x, label_y, series, 15, MUTED if series == 'Original' else CYAN, 500)
        text(x+508, label_y-2, label, 21, WHITE, 600, align='right')
        box(plot_x, bar_y, plot_width, 12, '#202529', 4)
        length = plot_width * value / pair['scale']
        # Exact linear length, including very short Rust readiness bar. No minimum width.
        box(plot_x, bar_y, length, 12, color, min(4, length/2))
    text(plot_x, y+197, '0', 11, MUTED)
    text(x+508, y+197, pair['scaleLabel'], 11, MUTED, align='right')

line(56, 1212, 1144, 1212)
text(56, 1233, 'Apple M4 · 32 GiB RAM · macOS 27 · 10 rounds per runtime / condition · medians', 15, WHITE, 500)
text(56, 1260, 'Backend: all providers disabled; fresh app state; OS caches not cleared; both include the same native monitor.', 13, MUTED)
text(56, 1282, 'Desktop: disconnected UI-only visibility lifecycle; not first paint, usable UI, or total app startup.', 13, MUTED)
text(56, 1304, 'RPC medians pool 1,000 reads; thread creates pool 300 writes. sum RSS may count shared pages twice.', 13, MUTED)
text(56, 1326, 'Different feature coverage and startup work. Common-workload results do not establish full application parity.', 12, MUTED)

svg = [f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}">', '<title>T3 Code: original production versus incomplete Rust port</title>', '<desc>Six paired benchmark charts. Warm desktop visibility 1.65 times faster, fresh backend accepted-command readiness 40.4 times faster, and idle backend plus native monitor sum RSS about 90 percent lower. Different feature coverage; not full application parity.</desc>']
for item in scene:
    if item['kind'] == 'rect':
        svg.append(f'<rect x="{item["x"]}" y="{item["y"]}" width="{item["width"]}" height="{item["height"]}" rx="{item["radius"]}" fill="{item["fill"]}"'+(f' stroke="{item["stroke"]}"' if item['stroke'] else '')+'/>')
    elif item['kind'] == 'line':
        svg.append(f'<path d="M{item["x1"]},{item["y1"]} L{item["x2"]},{item["y2"]}" stroke="{item["fill"]}"/>')
    else:
        anchor = 'end' if item['align'] == 'right' else 'start'
        svg.append(f'<text x="{item["x"]}" y="{item["y"]}" dominant-baseline="text-before-edge" text-anchor="{anchor}" font-family="{FONT}" font-size="{item["size"]}" font-weight="{item["weight"]}" letter-spacing="{item["kern"]}" fill="{item["fill"]}">{html.escape(item["text"])}</text>')
svg.append('</svg>')
(HERE/'comparison.svg').write_text('\n'.join(svg)+'\n')
(HERE/'scene.json').write_text(json.dumps(dict(width=W, height=H, items=scene), indent=2)+'\n')
metrics = dict(date='2026-10-08', sourceCommit='fcd48c83a', aggregation='Backend startup/RSS: 10-round medians. RPCs: pooled 1000-read medians. Creates: pooled 300-write medians. Desktop: 10 warm-profile medians.', featureEquivalent=False, memoryReductionPercent=reduction, pairs=pairs, inputs={str(path.relative_to(ROOT)): hashlib.sha256(path.read_bytes()).hexdigest() for path in [backend_path, desktop_path]})
(HERE/'metrics.json').write_text(json.dumps(metrics, indent=2)+'\n')
if not args.svg_only:
    with tempfile.TemporaryDirectory(prefix='t3-chart-swift-', dir='/private/tmp') as cache:
        subprocess.run(['swift', '-module-cache-path', cache, str(HERE/'render.swift'), str(HERE/'scene.json'), str(HERE/'comparison.png')], check=True)
print(json.dumps({'width':W,'height':H,'desktopSpeedup':pairs[0]['ratio'],'backendSpeedup':pairs[1]['ratio'],'memoryReductionPercent':reduction,'svg':str(HERE/'comparison.svg'),'png':None if args.svg_only else str(HERE/'comparison.png')}))
