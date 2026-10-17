#!/usr/bin/env python3
"""Makes the screenshot for the README.

Runs nginx-tail against a few busy, fake log files inside tmux and renders
the screen as an SVG. The traffic is random but seeded, so the screenshot only
changes much when nginx-tail's output changes.

Usage: ./make_screenshot.py [output.svg]
Needs: cargo, python3 and tmux (sudo apt install tmux)
"""

import os
import random
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from datetime import datetime, timezone
from pathlib import Path

COLS, ROWS = 110, 30
SECONDS = 4.5  # just before the 2nd batch of log lines gets printed

# site: (requests per second, {status: weight})
SITES = {
    "shop": (220, {200: 80, 304: 8, 301: 3, 404: 5, 499: 2, 500: 1}),
    "api": (140, {200: 85, 201: 5, 401: 4, 404: 3, 429: 2, 502: 1}),
    "static": (90, {200: 60, 304: 38, 404: 2}),
    "blog": (12, {200: 90, 301: 6, 404: 4}),
    "intranet": (2, {200: 95, 403: 5}),
}
PATHS = {
    "shop": ["/", "/cart", "/checkout", "/products/espresso-machine", "/products/grinder", "/search?q=beans", "/account/orders"],
    "api": ["/v2/orders", "/v2/orders/8121", "/v2/products?page=3", "/v2/auth/token", "/v2/stock", "/healthz"],
    "static": ["/css/site.css", "/js/app.js", "/img/logo.svg", "/img/products/espresso.webp", "/fonts/inter.woff2"],
    "blog": ["/", "/2026/10/roasting-at-home", "/2026/09/water-hardness", "/feed.xml", "/tags/brewing"],
    "intranet": ["/", "/wiki/onboarding", "/reports/weekly"],
}
AGENTS = [
    "Mozilla/5.0 (X11; Linux x86_64; rv:140.0) Gecko/20100101 Firefox/140.0",
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 15_6) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/19.0 Safari/605.1.15",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36",
    "curl/8.14.1",
    "okhttp/5.1.0",
]

PALETTE = {  # One Dark-ish
    30: "#3f4451", 31: "#e06c75", 32: "#98c379", 33: "#e5c07b", 34: "#61afef",
    35: "#c678dd", 36: "#56b6c2", 37: "#d7dae0", 90: "#5c6370", 91: "#ff7a85",
    92: "#b5e890", 93: "#f0a45d", 94: "#82c4ff", 95: "#de99f0", 96: "#6edbe6", 97: "#ffffff",
}
FG, BG, FONT, CHAR_W, LINE_H, PAD, BAR = "#abb2bf", "#21252b", 13, 7.8, 17, 16, 28

PLAIN = (None, False, False)  # color, bold, reverse


def start_traffic(logdir):
    """Keeps appending log lines to a fake access.log per site, until the returned event is set"""
    rng = random.Random(42)
    files = {}
    for site in SITES:
        (logdir / site).mkdir(parents=True)
        files[site] = open(logdir / site / "access.log", "a", buffering=1)

    def line(site, statuses):
        ip = f"{rng.choice(['192.0.2', '198.51.100', '203.0.113'])}.{rng.randrange(1, 255)}"
        date = datetime.now(timezone.utc).strftime("%d/%b/%Y:%H:%M:%S +0000")
        status = rng.choices(list(statuses), list(statuses.values()))[0]
        method = "POST" if site == "api" and rng.random() < 0.3 else "GET"
        size = 0 if status == 304 else rng.randrange(200, 40000)
        return f'{ip} - - [{date}] "{method} {rng.choice(PATHS[site])} HTTP/1.1" {status} {size} "-" "{rng.choice(AGENTS)}"\n'

    stop = threading.Event()

    def generate():
        while not stop.wait(0.05):
            for site, (rate, statuses) in SITES.items():
                # rate * 0.05 lines on average, also for the quiet sites
                count = int(rate * 0.05 + rng.random())
                files[site].write("".join(line(site, statuses) for _ in range(count)))

    threading.Thread(target=generate, daemon=True).start()
    return stop


def capture(command, workdir):
    """Runs the command in a detached tmux session and returns the screen, with colors"""
    config = workdir / "tmux.conf"
    config.write_text("set -g status off\nset -g remain-on-exit on\n")
    tmux = ["tmux", "-L", f"nginx-tail-screenshot-{os.getpid()}", "-f", str(config)]
    try:
        subprocess.run(
            tmux + ["new-session", "-d", "-s", "screenshot", "-x", str(COLS), "-y", str(ROWS), shlex.join(command)],
            check=True,
        )
        time.sleep(SECONDS)
        screen = subprocess.run(
            tmux + ["capture-pane", "-p", "-e", "-t", "screenshot"], check=True, capture_output=True, text=True
        ).stdout
        dead = subprocess.run(
            tmux + ["display-message", "-p", "-t", "screenshot", "#{pane_dead}"], check=True, capture_output=True, text=True
        ).stdout
        if dead.strip() == "1":
            sys.exit(f"nginx-tail exited early:\n{screen}")
        return screen
    finally:
        subprocess.run(tmux + ["kill-server"], check=False, stderr=subprocess.DEVNULL)


def apply_sgr(params, style):
    color, bold, reverse = style
    codes = [int(code) if code else 0 for code in params.split(";")]
    i = 0
    while i < len(codes):
        code = codes[i]
        if code == 0:
            color, bold, reverse = PLAIN
        elif code == 1:
            bold = True
        elif code == 7:
            reverse = True
        elif code == 22:
            bold = False
        elif code == 27:
            reverse = False
        elif 30 <= code <= 37 or 90 <= code <= 97:
            color = code
        elif code == 39:
            color = None
        elif code in (38, 48) and codes[i + 1 : i + 2] == [5]:  # 256 colors
            n = codes[i + 2] if i + 2 < len(codes) else 0
            if code == 38:
                color = 30 + n if n < 8 else 90 + n - 8 if n < 16 else None
            i += 2
        elif code in (38, 48) and codes[i + 1 : i + 2] == [2]:  # true color, we don't use it
            i += 4
        i += 1
    return color, bold, reverse


def parse(screen):
    """Turns captured text with color codes into rows of (char, style) cells"""
    rows = []
    style = PLAIN
    for line in screen.split("\n")[:ROWS]:
        cells = []
        # odd parts are the parameters of the color codes
        for i, part in enumerate(re.split(r"\x1b\[([0-9;]*)m", line)):
            if i % 2:
                style = apply_sgr(part, style)
            else:
                cells += [(char, style) for char in re.sub(r"\x1b\[[0-9;?]*[A-Za-z]", "", part)]
        rows.append((cells + [(" ", PLAIN)] * COLS)[:COLS])
    while rows and all(char == " " for char, _ in rows[-1]):
        rows.pop()
    return rows


def render_svg(rows):
    def escape(text):
        return text.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")

    width = round(COLS * CHAR_W + 2 * PAD)
    height = round(len(rows) * LINE_H + 2 * PAD + BAR)
    svg = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}">',
        f'<rect width="{width}" height="{height}" rx="8" fill="{BG}"/>',
        '<circle cx="18" cy="16" r="5.5" fill="#ff5f57"/><circle cx="36" cy="16" r="5.5" fill="#febc2e"/><circle cx="54" cy="16" r="5.5" fill="#28c840"/>',
        f'<text x="{width / 2}" y="20" fill="#7f848e" font-family="sans-serif" font-size="12" text-anchor="middle">nginx-tail</text>',
        f'<g font-family="ui-monospace, SFMono-Regular, Menlo, Consolas, \'DejaVu Sans Mono\', monospace" font-size="{FONT}" fill="{FG}" xml:space="preserve">',
    ]
    for row, cells in enumerate(rows):
        baseline = BAR + PAD + row * LINE_H + FONT
        col = 0
        while col < COLS:
            style = cells[col][1]
            end = col
            while end < COLS and cells[end][1] == style:
                end += 1
            text = "".join(char for char, _ in cells[col:end])
            color, bold, reverse = style
            left = PAD + col * CHAR_W
            if reverse:
                svg.append(
                    f'<rect x="{left:.1f}" y="{baseline - FONT + 1}" width="{(end - col) * CHAR_W:.1f}" height="{LINE_H - 1}" fill="{PALETTE.get(color, FG)}"/>'
                )
            if text.strip():
                fill = BG if reverse else PALETTE.get(color)
                attrs = (f' fill="{fill}"' if fill else "") + (' font-weight="bold"' if bold else "")
                svg.append(f'<text x="{left:.1f}" y="{baseline}"{attrs}>{escape(text.rstrip() if end == COLS else text)}</text>')
            col = end
    svg += ["</g>", "</svg>", ""]
    return "\n".join(svg)


def main():
    os.chdir(Path(__file__).parent)
    output = Path(sys.argv[1] if len(sys.argv) > 1 else "screenshot.svg")
    if not shutil.which("tmux"):
        sys.exit("Please install tmux first: sudo apt install tmux")

    subprocess.run(["cargo", "build", "--release"], check=True)
    with tempfile.TemporaryDirectory() as workdir:
        workdir = Path(workdir)
        stop = start_traffic(workdir / "nginx")
        time.sleep(0.3)
        try:
            screen = capture(["./target/release/nginx-tail", str(workdir / "nginx")], workdir)
        finally:
            stop.set()

    rows = parse(screen)
    if not rows:
        sys.exit("The screen is empty, did nginx-tail start?")
    output.write_text(render_svg(rows))
    # also show it in the terminal, as a quick check
    print("\n".join("".join(char for char, _ in cells).rstrip() for cells in rows))
    print(f"\nWrote {output}")


if __name__ == "__main__":
    main()
