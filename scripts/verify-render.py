#!/usr/bin/env python3
"""
Lisa レンダリング・ナビゲーション・配置・場面・カメラの自動検証スクリプト。

~/Library/Application Support/Lisa/lisa.db* を APFS クローン（cp -c）し、
LISA_DATA_DIR / LISA_DEBUG_REMOTE=1 / LISA_RENDER_DIAG=1 で検証用の Lisa を起動して、
control.json 経由で全組み合わせの不変条件を検証する。

本物のアプリや本物のデータディレクトリには一切触れない。
"""

from __future__ import annotations

import argparse
import json
import math
import os
import shutil
import signal
import socket
import sqlite3
import struct
import subprocess
import sys
import tempfile
import time
import zlib
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any


REPO_ROOT = Path(__file__).resolve().parent.parent
DEFAULT_BIN = REPO_ROOT / "target" / "debug" / "lisa"
SOURCE_DATA_DIR = Path(
    os.environ.get("LISA_VERIFY_SOURCE_DATA_DIR")
    or Path.home() / "Library" / "Application Support" / "Lisa"
)
BAR_PX = 32.0


# ---------------------------------------------------------------------------
# Fast macOS ImageIO PNG helper + pure-stdlib PNG decoder fallback
# ---------------------------------------------------------------------------

_PNG_HELPER: Path | None = None

_PNG_HELPER_SWIFT = """
import Foundation
import ImageIO

let args = CommandLine.arguments
guard args.count >= 3 else { exit(2) }
let mode = args[1]
let url = URL(fileURLWithPath: args[2]) as CFURL
guard let src = CGImageSourceCreateWithURL(url, nil),
      let img = CGImageSourceCreateImageAtIndex(src, 0, nil),
      let provider = img.dataProvider,
      let cfData = provider.data,
      let ptr = CFDataGetBytePtr(cfData) else {
    fputs("decode failed\\n", stderr)
    exit(1)
}
let w = img.width
let h = img.height
let bpp = img.bitsPerPixel / 8
let stride = img.bytesPerRow
guard bpp == 3 || bpp == 4 else {
    fputs("unsupported bpp \\(bpp)\\n", stderr)
    exit(1)
}

if mode == "solid" {
    let scaleFactor = Double(args[3]) ?? 1.0
    let barPx = Double(args[4]) ?? 32.0
    let y0 = min(h - 1, Int((barPx + 8.0) * scaleFactor))
    let stepX = max(1, w / 80)
    let stepY = max(1, (h - y0) / 60)
    var colors = Set<Int>()
    var hasFirst = false
    var fr = 0, fg = 0, fb = 0
    var maxDiff = 0
    var y = y0
    while y < h {
        let rowOff = y * stride
        var x = 0
        while x < w {
            let off = rowOff + x * bpp
            let r = Int(ptr[off])
            let g = Int(ptr[off + 1])
            let b = Int(ptr[off + 2])
            if !hasFirst {
                fr = r; fg = g; fb = b; hasFirst = true
            } else {
                let d = abs(r - fr) + abs(g - fg) + abs(b - fb)
                if d > maxDiff { maxDiff = d }
            }
            colors.insert(((r >> 2) << 12) | ((g >> 2) << 6) | (b >> 2))
            x += stepX
        }
        y += stepY
    }
    print("\\(w) \\(h) \\(colors.count) \\(maxDiff) \\(fr) \\(fg) \\(fb)")
} else if mode == "rect" {
    let rx = Double(args[3]) ?? 0.0
    let ry = Double(args[4]) ?? 0.0
    let rw = Double(args[5]) ?? 0.0
    let rh = Double(args[6]) ?? 0.0
    let sf = Double(args[7]) ?? 1.0
    let mx = rw * 0.15
    let my = rh * 0.15
    let x0 = max(0, min(w - 1, Int((rx + mx) * sf)))
    let x1 = max(x0 + 1, min(w, Int((rx + rw - mx) * sf)))
    let y0 = max(0, min(h - 1, Int((ry + my) * sf)))
    let y1 = max(y0 + 1, min(h, Int((ry + rh - my) * sf)))
    let stepX = max(1, (x1 - x0) / 32)
    let stepY = max(1, (y1 - y0) / 32)
    var sr = 0, sg = 0, sb = 0, cnt = 0
    var y = y0
    while y < y1 {
        let rowOff = y * stride
        var x = x0
        while x < x1 {
            let off = rowOff + x * bpp
            sr += Int(ptr[off])
            sg += Int(ptr[off + 1])
            sb += Int(ptr[off + 2])
            cnt += 1
            x += stepX
        }
        y += stepY
    }
    guard cnt > 0 else { exit(3) }
    print("\\(Double(sr) / Double(cnt)) \\(Double(sg) / Double(cnt)) \\(Double(sb) / Double(cnt))")
} else {
    exit(2)
}
"""


def _paeth(a: int, b: int, c: int) -> int:
    p = a + b - c
    pa = abs(p - a)
    pb = abs(p - b)
    pc = abs(p - c)
    if pa <= pb and pa <= pc:
        return a
    if pb <= pc:
        return b
    return c


def decode_png(path: Path) -> tuple[int, int, int, bytes]:
    """PNG を読み、(width, height, channels, raw_bytes) を返す。"""
    try:
        from PIL import Image as PilImage  # type: ignore

        with PilImage.open(path) as img:
            rgb = img.convert("RGB")
            return rgb.width, rgb.height, 3, rgb.tobytes()
    except ImportError:
        pass
    data = path.read_bytes()
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        raise ValueError(f"PNG ヘッダが不正です: {path}")
    pos = 8
    width = height = bit_depth = color_type = 0
    idat = bytearray()
    while pos < len(data):
        length = struct.unpack(">I", data[pos : pos + 4])[0]
        ctype = data[pos + 4 : pos + 8]
        chunk = data[pos + 8 : pos + 8 + length]
        pos += 12 + length
        if ctype == b"IHDR":
            width, height, bit_depth, color_type, comp, filt, interlace = struct.unpack(
                ">IIBBBBB", chunk
            )
            if bit_depth != 8 or comp != 0 or filt != 0 or interlace != 0:
                raise ValueError(
                    f"未対応 PNG 形式: depth={bit_depth} color={color_type} interlace={interlace}"
                )
        elif ctype == b"IDAT":
            idat.extend(chunk)
        elif ctype == b"IEND":
            break
    if color_type == 6:
        bpp = 4
    elif color_type == 2:
        bpp = 3
    else:
        raise ValueError(f"未対応 PNG color_type={color_type}")
    raw = zlib.decompress(bytes(idat))
    stride = width * bpp
    out = bytearray(height * stride)
    prev = bytearray(stride)
    idx = 0
    for y in range(height):
        ftype = raw[idx]
        idx += 1
        row = bytearray(raw[idx : idx + stride])
        idx += stride
        if ftype == 0:
            pass
        elif ftype == 1:
            for i in range(bpp, stride):
                row[i] = (row[i] + row[i - bpp]) & 0xFF
        elif ftype == 2:
            for i in range(stride):
                row[i] = (row[i] + prev[i]) & 0xFF
        elif ftype == 3:
            for i in range(stride):
                a = row[i - bpp] if i >= bpp else 0
                row[i] = (row[i] + ((a + prev[i]) >> 1)) & 0xFF
        elif ftype == 4:
            for i in range(stride):
                a = row[i - bpp] if i >= bpp else 0
                b = prev[i]
                c = prev[i - bpp] if i >= bpp else 0
                row[i] = (row[i] + _paeth(a, b, c)) & 0xFF
        else:
            raise ValueError(f"未知の PNG フィルタ: {ftype}")
        out[y * stride : (y + 1) * stride] = row
        prev = row
    return width, height, bpp, bytes(out)


def check_png_not_solid(path: Path, scale_factor: float) -> tuple[bool, str]:
    """メニューバーより下の領域が一色（背景のみ・真っ黒）でないことを確かめる。"""
    if _PNG_HELPER is not None:
        res = subprocess.run(
            [str(_PNG_HELPER), "solid", str(path), str(scale_factor), str(BAR_PX)],
            capture_output=True,
            text=True,
        )
        if res.returncode == 0:
            parts = res.stdout.strip().split()
            if len(parts) == 7:
                w, h, distinct, max_diff, fr, fg, fb = map(int, parts)
                first = (fr, fg, fb)
                if distinct <= 1 or max_diff < 12:
                    return (
                        False,
                        f"画面が一色です ({path.name}: {w}x{h}, distinct={distinct}, max_diff={max_diff}, sample={first})",
                    )
                return True, f"ok ({w}x{h}, distinct={distinct}, max_diff={max_diff})"
    w, h, bpp, pixels = decode_png(path)
    y0 = min(h - 1, int((BAR_PX + 8.0) * scale_factor))
    step_x = max(1, w // 80)
    step_y = max(1, (h - y0) // 60)
    colors: set[tuple[int, int, int]] = set()
    first: tuple[int, int, int] | None = None
    max_diff = 0
    for y in range(y0, h, step_y):
        row_off = y * w * bpp
        for x in range(0, w, step_x):
            off = row_off + x * bpp
            rgb = (pixels[off], pixels[off + 1], pixels[off + 2])
            if first is None:
                first = rgb
            else:
                diff = (
                    abs(rgb[0] - first[0])
                    + abs(rgb[1] - first[1])
                    + abs(rgb[2] - first[2])
                )
                if diff > max_diff:
                    max_diff = diff
            colors.add((rgb[0] >> 2, rgb[1] >> 2, rgb[2] >> 2))
    if len(colors) <= 1 or max_diff < 12:
        return (
            False,
            f"画面が一色です ({path.name}: {w}x{h}, distinct={len(colors)}, max_diff={max_diff}, sample={first})",
        )
    return True, f"ok ({w}x{h}, distinct={len(colors)}, max_diff={max_diff})"


def sample_rect_rgb(
    path: Path, rect: list[float], scale_factor: float
) -> tuple[float, float, float]:
    """論理ピクセル矩形 [x, y, w, h] の内側（縁 15% を除く）の平均 RGB を返す。"""
    rx, ry, rw, rh = rect
    if _PNG_HELPER is not None:
        res = subprocess.run(
            [
                str(_PNG_HELPER),
                "rect",
                str(path),
                str(rx),
                str(ry),
                str(rw),
                str(rh),
                str(scale_factor),
            ],
            capture_output=True,
            text=True,
        )
        if res.returncode == 0:
            parts = res.stdout.strip().split()
            if len(parts) == 3:
                return (float(parts[0]), float(parts[1]), float(parts[2]))
    w, h, bpp, pixels = decode_png(path)
    mx = rw * 0.15
    my = rh * 0.15
    x0 = max(0, min(w - 1, int((rx + mx) * scale_factor)))
    x1 = max(x0 + 1, min(w, int((rx + rw - mx) * scale_factor)))
    y0 = max(0, min(h - 1, int((ry + my) * scale_factor)))
    y1 = max(y0 + 1, min(h, int((ry + rh - my) * scale_factor)))
    sr = sg = sb = cnt = 0
    step_x = max(1, (x1 - x0) // 32)
    step_y = max(1, (y1 - y0) // 32)
    for y in range(y0, y1, step_y):
        row_off = y * w * bpp
        for x in range(x0, x1, step_x):
            off = row_off + x * bpp
            sr += pixels[off]
            sg += pixels[off + 1]
            sb += pixels[off + 2]
            cnt += 1
    if cnt == 0:
        raise ValueError(f"サンプリング領域が空です: {rect}")
    return (sr / cnt, sg / cnt, sb / cnt)


# ---------------------------------------------------------------------------
# Fixture discovery from cloned SQLite DB
# ---------------------------------------------------------------------------


@dataclass
class Fixture:
    drive_mount: Path
    drive_entry: int
    ancestor_chain: list[int]
    ancestor_path: Path
    parent_chain: list[int]
    parent_path: Path
    target_chain: list[int]
    target_path: Path
    sibling_chain: list[int]
    sibling_path: Path
    child_chain: list[int]
    child_path: Path
    alt_chain: list[int]
    alt_path: Path


def discover_fixture(db_path: Path) -> Fixture:
    """クローン DB から実在する 4〜5 段のテスト対象フォルダ群を探す。"""
    conn = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
    cur = conn.cursor()
    drives = cur.execute(
        "SELECT id, last_mount_path FROM drive ORDER BY CASE WHEN last_mount_path = '/System/Volumes/Data' THEN 0 ELSE 1 END, id"
    ).fetchall()

    def child_dirs(parent_id: int) -> list[tuple[int, str]]:
        rows = cur.execute(
            "SELECT id, name_nfc FROM entry WHERE parent_id = ? AND kind = 'dir' AND listed = 1 AND error IS NULL AND cloud_only = 0 AND other_volume = 0 ORDER BY name_nfc",
            (parent_id,),
        ).fetchall()
        return [(int(r[0]), str(r[1])) for r in rows]

    for drive_id, mount_str in drives:
        mount = Path(mount_str)
        if not mount.is_dir():
            continue
        root_row = cur.execute(
            "SELECT id FROM entry WHERE drive_id = ? AND parent_id IS NULL",
            (drive_id,),
        ).fetchone()
        if not root_row:
            continue
        drive_entry = int(root_row[0])

        # Prefer Users/<user> subtree when available so paths resemble real usage.
        d1_list = child_dirs(drive_entry)
        d1_list.sort(key=lambda x: (0 if x[1] == "Users" else 1, x[1]))

        for anc_id, anc_name in d1_list:
            anc_path = mount / anc_name
            if not anc_path.is_dir():
                continue
            d2_list = child_dirs(anc_id)
            d2_list.sort(key=lambda x: (0 if x[1] == os.environ.get("USER", "") else 1, x[1]))
            for par_id, par_name in d2_list:
                par_path = anc_path / par_name
                if not par_path.is_dir():
                    continue
                d3_list = [
                    (eid, ename, par_path / ename)
                    for eid, ename in child_dirs(par_id)
                    if not ename.startswith(".") and (par_path / ename).is_dir()
                ]
                if len(d3_list) < 2:
                    continue
                for idx, (tgt_id, tgt_name, tgt_path) in enumerate(d3_list):
                    d4_list = [
                        (cid, cname, tgt_path / cname)
                        for cid, cname in child_dirs(tgt_id)
                        if not cname.startswith(".") and (tgt_path / cname).is_dir()
                    ]
                    if not d4_list:
                        continue
                    sib_id, _, sib_path = d3_list[(idx + 1) % len(d3_list)]
                    alt_id, _, alt_path = d3_list[(idx + 2) % len(d3_list)]
                    ch_id, _, ch_path = d4_list[0]
                    conn.close()
                    return Fixture(
                        drive_mount=mount,
                        drive_entry=drive_entry,
                        ancestor_chain=[drive_entry, anc_id],
                        ancestor_path=anc_path,
                        parent_chain=[drive_entry, anc_id, par_id],
                        parent_path=par_path,
                        target_chain=[drive_entry, anc_id, par_id, tgt_id],
                        target_path=tgt_path,
                        sibling_chain=[drive_entry, anc_id, par_id, sib_id],
                        sibling_path=sib_path,
                        child_chain=[drive_entry, anc_id, par_id, tgt_id, ch_id],
                        child_path=ch_path,
                        alt_chain=[drive_entry, anc_id, par_id, alt_id],
                        alt_path=alt_path,
                    )
    conn.close()
    raise RuntimeError("DB から検証用の深いフォルダ（4〜5 段）を見つけられませんでした")


# ---------------------------------------------------------------------------
# Lisa test harness (isolated LISA_DATA_DIR + APFS DB clone)
# ---------------------------------------------------------------------------


class LisaHarness:
    def __init__(self, binary: Path, keep_tmp: bool = False):
        self.binary = binary
        self.keep_tmp = keep_tmp
        self.tmp_dir = Path(tempfile.mkdtemp(prefix="lisa-verify-", dir="/tmp"))
        self.proc: subprocess.Popen[bytes] | None = None
        self.endpoint: dict[str, Any] | None = None
        self.activator: Path | None = None
        self.log_file = self.tmp_dir / "app.log"
        self.capture_seq = 0

    def prepare(self) -> Fixture:
        # 1. APFS clone lisa.db*
        db_files = sorted(SOURCE_DATA_DIR.glob("lisa.db*"))
        if not db_files:
            raise RuntimeError(f"lisa.db が見つかりません: {SOURCE_DATA_DIR}")
        for src in db_files:
            dst = self.tmp_dir / src.name
            res = subprocess.run(["cp", "-c", str(src), str(dst)], capture_output=True)
            if res.returncode != 0:
                shutil.copy2(src, dst)

        # 2. Disable LAN sharing in isolated settings.json
        (self.tmp_dir / "settings.json").write_text(
            json.dumps({"lan_share": 0.0}, indent=2) + "\n", encoding="utf-8"
        )

        # 3. Compile tiny window activator & PNG analyzer helpers (macOS)
        if sys.platform == "darwin":
            swift_src = self.tmp_dir / "act.swift"
            swift_src.write_text(
                "import AppKit\n"
                "let pid = pid_t(CommandLine.arguments[1])!\n"
                "if let app = NSRunningApplication(processIdentifier: pid) {\n"
                "    _ = app.activate(options: [.activateAllWindows])\n"
                "}\n",
                encoding="utf-8",
            )
            act_bin = self.tmp_dir / "activate"
            res = subprocess.run(
                ["swiftc", str(swift_src), "-o", str(act_bin)],
                capture_output=True,
                text=True,
            )
            if res.returncode == 0:
                self.activator = act_bin

            global _PNG_HELPER
            png_src = self.tmp_dir / "pngtool.swift"
            png_src.write_text(_PNG_HELPER_SWIFT, encoding="utf-8")
            png_bin = self.tmp_dir / "pngtool"
            res2 = subprocess.run(
                ["swiftc", "-O", str(png_src), "-o", str(png_bin)],
                capture_output=True,
                text=True,
            )
            if res2.returncode == 0:
                _PNG_HELPER = png_bin

        return discover_fixture(self.tmp_dir / "lisa.db")

    def start(self) -> None:
        env = os.environ.copy()
        env["LISA_DATA_DIR"] = str(self.tmp_dir)
        env["LISA_DEBUG_REMOTE"] = "1"
        env["LISA_RENDER_DIAG"] = "1"
        env.setdefault("LISA_WINDOW_SIZE", "960x600")
        control_path = self.tmp_dir / "control.json"
        if control_path.exists():
            control_path.unlink()
        log_fp = open(self.log_file, "ab")
        self.proc = subprocess.Popen(
            [str(self.binary)],
            env=env,
            stdout=log_fp,
            stderr=subprocess.STDOUT,
            cwd=str(REPO_ROOT),
        )
        log_fp.close()
        deadline = time.monotonic() + 25.0
        while time.monotonic() < deadline:
            if self.proc.poll() is not None:
                raise RuntimeError(
                    f"Lisa が起動直後に終了しました (code={self.proc.returncode}). log={self.log_file}"
                )
            if control_path.exists():
                try:
                    ep = json.loads(control_path.read_text(encoding="utf-8"))
                    if ep.get("pid") == self.proc.pid:
                        self.endpoint = ep
                        break
                except Exception:
                    pass
            time.sleep(0.05)
        if not self.endpoint:
            raise RuntimeError(f"control.json が現れませんでした ({control_path})")
        self.activate()
        # Wait until initial startup settles and window is not occluded
        self.wait_settled(timeout=15.0)
        # Pause background scanning so only our controlled DebugChanged/Rescan runs
        self.send({"command": "pause", "paused": True})

    def stop(self) -> None:
        if self.proc is None:
            return
        if self.proc.poll() is None:
            try:
                self.send({"command": "quit"}, timeout=3.0)
            except Exception:
                pass
            try:
                self.proc.wait(timeout=5.0)
            except subprocess.TimeoutExpired:
                self.proc.terminate()
                try:
                    self.proc.wait(timeout=3.0)
                except subprocess.TimeoutExpired:
                    self.proc.kill()
        self.proc = None
        self.endpoint = None

    def tail_logs(self, lines: int = 40) -> str:
        out: list[str] = []
        crash = self.tmp_dir / "crash.log"
        if crash.exists():
            out.append("--- crash.log ---")
            out.append(crash.read_text(encoding="utf-8", errors="replace")[-2000:])
        lisa_log = self.tmp_dir / "logs" / "lisa.log"
        for label, p in [("lisa.log", lisa_log), ("app.log", self.log_file)]:
            if p.exists():
                text = p.read_text(encoding="utf-8", errors="replace").splitlines()
                if text:
                    out.append(f"--- {label} (last {lines} lines) ---")
                    out.extend(text[-lines:])
        return "\n".join(out)

    def ensure_alive(self) -> None:
        if self.proc is None or self.proc.poll() is not None:
            self.stop()
            self.start()

    def cleanup(self) -> None:
        self.stop()
        if not self.keep_tmp and self.tmp_dir.exists():
            shutil.rmtree(self.tmp_dir, ignore_errors=True)

    def activate(self) -> None:
        if self.proc is None or self.activator is None:
            return
        subprocess.run(
            [str(self.activator), str(self.proc.pid)],
            capture_output=True,
            timeout=5.0,
        )

    def send(self, command: dict[str, Any], timeout: float = 10.0) -> dict[str, Any]:
        assert self.endpoint is not None
        payload = {"token": self.endpoint["token"], **command}
        line = (json.dumps(payload) + "\n").encode("utf-8")
        with socket.create_connection(
            ("127.0.0.1", int(self.endpoint["port"])), timeout=timeout
        ) as sock:
            sock.settimeout(timeout)
            sock.sendall(line)
            buf = bytearray()
            while not buf.endswith(b"\n"):
                chunk = sock.recv(65536)
                if not chunk:
                    break
                buf.extend(chunk)
        return json.loads(buf.decode("utf-8"))

    def debug_state(self) -> dict[str, Any]:
        rep = self.send({"command": "debug_state"})
        if not rep.get("ok"):
            raise RuntimeError(f"debug_state 失敗: {rep}")
        return rep["debug"]

    def show(self, chain: list[int]) -> None:
        rep = self.send({"command": "show", "chain": chain})
        if not rep.get("ok"):
            raise RuntimeError(f"show 失敗: {rep}")

    def debug_changed(self, path: Path) -> None:
        rep = self.send({"command": "debug_changed", "path": str(path)})
        if not rep.get("ok"):
            raise RuntimeError(f"debug_changed 失敗: {rep}")

    def pinch(
        self,
        delta: float,
        touching: bool | None = None,
        cursor: list[float] | None = None,
    ) -> None:
        cmd: dict[str, Any] = {"command": "debug_pinch", "delta": delta}
        if touching is not None:
            cmd["touching"] = touching
        if cursor is not None:
            cmd["cursor"] = cursor
        rep = self.send(cmd)
        if not rep.get("ok"):
            raise RuntimeError(f"debug_pinch 失敗: {rep}")

    def pan(
        self,
        dx: float,
        dy: float,
        phase: str = "tap",
        cursor: list[float] | None = None,
    ) -> None:
        cmd: dict[str, Any] = {
            "command": "debug_pan",
            "dx": dx,
            "dy": dy,
            "phase": phase,
        }
        if cursor is not None:
            cmd["cursor"] = cursor
        rep = self.send(cmd)
        if not rep.get("ok"):
            raise RuntimeError(f"debug_pan 失敗: {rep}")

    def nav_action(
        self, action: str, screen_pos: list[float] | None = None
    ) -> None:
        cmd: dict[str, Any] = {"command": "debug_nav", "action": action}
        if screen_pos is not None:
            cmd["screen_pos"] = screen_pos
        rep = self.send(cmd)
        if not rep.get("ok"):
            raise RuntimeError(f"debug_nav 失敗: {rep}")

    def capture(self, tag: str) -> Path:
        self.capture_seq += 1
        safe = "".join(c if c.isalnum() or c in "-_" else "_" for c in tag)[:48]
        name = f"cap_{self.capture_seq:04d}_{safe}"
        out_path = self.tmp_dir / "screenshots" / f"{name}.png"
        if out_path.exists():
            out_path.unlink()
        for attempt in range(5):
            self.activate()
            # 窓の可視復帰イベントが処理されるのを少し待つ
            wait_vis = time.monotonic() + 1.5
            while time.monotonic() < wait_vis:
                st = self.debug_state()
                if not st["occluded"]:
                    break
                time.sleep(0.05)
            rep = self.send({"command": "capture", "name": name})
            if rep.get("ok"):
                break
            time.sleep(0.25)
            if attempt == 4:
                raise RuntimeError(f"capture 失敗: {rep}")
        deadline = time.monotonic() + 8.0
        while time.monotonic() < deadline:
            if out_path.exists() and out_path.stat().st_size > 100:
                time.sleep(0.05)
                return out_path
            # Pump redraw via lightweight debug_state
            try:
                self.send({"command": "debug_state"})
            except Exception:
                pass
            time.sleep(0.05)
        raise RuntimeError(f"capture PNG が保存されませんでした: {out_path}")

    def wait_settled(
        self,
        timeout: float = 8.0,
        wait_bake: bool = True,
        min_quiet: float = 0.45,
    ) -> dict[str, Any]:
        """アニメーション・フェード・ジェスチャー・Jump・TreeChanged・(任意でBake) が落ち着くまで待つ。"""
        deadline = time.monotonic() + timeout
        quiet_since: float | None = None
        last = self.debug_state()
        while time.monotonic() < deadline:
            st = self.debug_state()
            last = st
            bake_busy = wait_bake and st["bake_state"] not in ("Done", "Idle")
            nav_folder = st["nav_nodes"][-1] if st["nav_nodes"] else 0
            needs_reanchor = (
                not st["gesture_has_parent_scene"]
                and st["current_scene_folder"] is not None
                and st["anchor_node"] != st["current_scene_folder"]
            )
            busy = (
                st["animating"]
                or st["fades"] > 0
                or st["gesture_active"]
                or st["jump_pending"]
                or st["tree_changed_pending"]
                or bake_busy
                or needs_reanchor
            )
            if not busy:
                if quiet_since is None:
                    quiet_since = time.monotonic()
                elif time.monotonic() - quiet_since >= min_quiet:
                    return st
            else:
                quiet_since = None
            time.sleep(0.05)
        return last

    def wait_for_bake_start(self, timeout: float = 3.0) -> dict[str, Any]:
        deadline = time.monotonic() + timeout
        last = self.debug_state()
        while time.monotonic() < deadline:
            st = self.debug_state()
            last = st
            if st["bake_state"] not in ("Done", "Idle"):
                return st
            time.sleep(0.02)
        return last


# ---------------------------------------------------------------------------
# Invariant checker
# ---------------------------------------------------------------------------


def check_invariants(
    st: dict[str, Any],
    label: str,
    *,
    settled: bool = True,
    png_path: Path | None = None,
) -> list[str]:
    errors: list[str] = []

    if not st["nav_top_attached"]:
        errors.append("nav.top() が Root につながっていない (detached)")
    if not st["anchor_attached"]:
        errors.append(
            f"layouts.anchor ({st['anchor_node']}:{st['anchor_name']}) が Root につながっていない"
        )
    if not st["nav_top_in_layout"]:
        errors.append(
            f"nav.top() ({st['nav_nodes'][-1]}:{st['nav_names'][-1]}) が layouts に無い"
        )

    names = st["nav_names"][1:]
    shown_names = st["nav_display_names"]
    if len(shown_names) != len(names) or any(
        shown != name and (index >= 2 or not shown.startswith(name + " ("))
        for index, (shown, name) in enumerate(zip(shown_names, names))
    ):
        errors.append(f"端末起点の表示列が不正: {shown_names!r} (木: {names!r})")
    expected_bc = " › ".join(shown_names) or "端末一覧"
    if st["breadcrumb"] != expected_bc:
        errors.append(
            f"パンくず不一致: breadcrumb={st['breadcrumb']!r} != nav={expected_bc!r}"
        )
    if st["menu_breadcrumb"] is not None and st["menu_breadcrumb"] != expected_bc:
        errors.append(
            f"メニューバーのパンくず不一致: menu={st['menu_breadcrumb']!r} != nav={expected_bc!r}"
        )

    if not st["camera_pos_finite"]:
        errors.append(f"camera_pos が非有限: {st['camera_pos']}")
    if not st["camera_scale_finite"] or st["camera_scale"] <= 0.0:
        errors.append(
            f"camera_scale が不正: scale={st['camera_scale']} finite={st['camera_scale_finite']}"
        )
    if not st["fit_scale_finite"] or st["fit_scale"] <= 0.0:
        errors.append(
            f"fit_scale が不正: fit_scale={st['fit_scale']} finite={st['fit_scale_finite']}"
        )

    main_scenes = st["main_scenes"]
    if settled:
        if st["gesture_active"] or st["animating"]:
            errors.append(
                f"停止待ちがタイムアウトした: gesture_active={st['gesture_active']} animating={st['animating']}"
            )
        if st["fades"] > 0:
            errors.append(f"停止後なのに Fade が残っている: fades={st['fades']}")
        expected_scenes = 2 if st["gesture_has_parent_scene"] else 1
        if main_scenes != expected_scenes:
            scene_desc = [
                f"(folder={s['folder']}:{s['folder_name']},cur={s['is_current']},par={s['is_parent_scene']},fade={s['has_fade']},ch={s['children_count']})"
                for s in st["scenes"]
            ]
            errors.append(
                f"SceneRoot 数が不正: main_scenes={main_scenes} (期待 {expected_scenes}) scenes={scene_desc}"
            )
        # Camera scale within valid zoom range
        if (
            st["camera_scale_finite"]
            and st["fit_scale_finite"]
            and st["camera_scale"] > 0.0
            and st["fit_scale"] > 0.0
        ):
            lo = st["fit_scale"] / 42.0
            hi = max(st["root_fit_scale"] * 1.25, st["fit_scale"] * 1.25)
            if not (lo <= st["camera_scale"] <= hi):
                errors.append(
                    f"camera_scale が範囲外: cam={st['camera_scale']:.6g} fit={st['fit_scale']:.6g} root={st['root_fit_scale']:.6g}"
                )
        # Current scene must match nav's folder (nav[-1] if dir; we check scenes list)
        cur_scenes = [s for s in st["scenes"] if s["is_current"]]
        if len(cur_scenes) != 1:
            errors.append(f"CurrentScene がレイヤー0に 1 個でない: {cur_scenes}")
        elif st["nav_nodes"] and cur_scenes[0]["folder"] not in st["nav_nodes"][-2:]:
            errors.append(
                f"CurrentScene のフォルダ({cur_scenes[0]['folder']}:{cur_scenes[0]['folder_name']})が nav({st['nav_nodes'][-2:]}) とずれている"
            )
    else:
        if main_scenes < 1 or main_scenes > 2:
            errors.append(f"動作中の SceneRoot 数が範囲外: main_scenes={main_scenes}")

    if png_path is not None:
        ok, msg = check_png_not_solid(png_path, st["window_scale_factor"])
        if not ok:
            errors.append(f"capture 不正 ({label}): {msg}")

    return errors


# ---------------------------------------------------------------------------
# High-level user actions
# ---------------------------------------------------------------------------


def do_pinch_out_1(h: LisaHarness) -> None:
    h.pinch(-0.35, touching=True)
    time.sleep(0.05)
    h.pinch(-0.35, touching=True)
    time.sleep(0.05)
    h.pinch(0.0, touching=False)


def do_pinch_out_2(h: LisaHarness) -> None:
    """2 段以上一気にズームアウトする連続ピンチ。"""
    for _ in range(10):
        h.pinch(-0.32, touching=True)
        time.sleep(0.025)
    h.pinch(0.0, touching=False)


def do_pinch_in(h: LisaHarness) -> None:
    st = h.debug_state()
    cursor = None
    for t in st["child_tiles"]:
        if t["is_dir"] and t["screen_rect"]:
            x, y, w, hh = t["screen_rect"]
            cursor = [x + w * 0.5, y + hh * 0.5]
            break
    for _ in range(6):
        h.pinch(0.32, touching=True, cursor=cursor)
        time.sleep(0.025)
    h.pinch(0.0, touching=False, cursor=cursor)


def do_pan(h: LisaHarness) -> None:
    h.pan(0.0, 0.0, phase="began")
    time.sleep(0.02)
    h.pan(-140.0, 0.0, phase="changed")
    time.sleep(0.02)
    h.pan(-140.0, 0.0, phase="changed")
    time.sleep(0.02)
    h.pan(0.0, 0.0, phase="ended")


def do_click_child(h: LisaHarness) -> bool:
    st = h.debug_state()
    for t in st["child_tiles"]:
        if t["is_dir"] and t["screen_rect"]:
            x, y, w, hh = t["screen_rect"]
            if w > 4.0 and hh > 4.0:
                h.nav_action("click", [x + w * 0.5, y + hh * 0.5])
                return True
    return False


# ---------------------------------------------------------------------------
# Test suites
# ---------------------------------------------------------------------------


@dataclass
class CaseResult:
    name: str
    passed: bool
    errors: list[str] = field(default_factory=list)
    state: dict[str, Any] | None = None


def run_group_a(
    h: LisaHarness,
    fix: Fixture,
    *,
    quick: bool,
    stop_on_fail: bool,
    capture_all: bool,
) -> list[CaseResult]:
    results: list[CaseResult] = []
    changed_targets = {
        "ancestor": fix.ancestor_path,
        "parent": fix.parent_path,
        "self": fix.target_path,
        "sibling": fix.sibling_path,
    }
    actions = ["pinch_out_1", "pinch_out_2", "pinch_in", "pan", "show_move"]
    orders = ["before_changed", "after_changed"]
    timings = ["idle", "during_capture"]
    if quick:
        # 主要な組み合わせ（全 4 対象 × 全 5 操作 × idle/during_capture × before/after を網羅する 12 件）
        selected = [
            ("idle", "ancestor", "after_changed", "pinch_out_2"),
            ("idle", "ancestor", "after_changed", "pinch_in"),
            ("idle", "ancestor", "before_changed", "pan"),
            ("idle", "parent", "after_changed", "pinch_out_1"),
            ("idle", "parent", "before_changed", "show_move"),
            ("idle", "self", "after_changed", "pinch_out_2"),
            ("idle", "sibling", "after_changed", "pan"),
            ("during_capture", "ancestor", "after_changed", "pinch_out_2"),
            ("during_capture", "ancestor", "before_changed", "pinch_in"),
            ("during_capture", "parent", "before_changed", "pinch_out_1"),
            ("during_capture", "self", "before_changed", "pinch_out_2"),
            ("during_capture", "sibling", "after_changed", "show_move"),
        ]
    else:
        selected = [
            (timing, ct_name, order, action)
            for timing in timings
            for ct_name in changed_targets
            for order in orders
            for action in actions
        ]

    def run_action(action: str) -> None:
        if action == "pinch_out_1":
            do_pinch_out_1(h)
        elif action == "pinch_out_2":
            do_pinch_out_2(h)
        elif action == "pinch_in":
            do_pinch_in(h)
        elif action == "pan":
            do_pan(h)
        elif action == "show_move":
            h.show(fix.alt_chain)

    for timing, ct_name, order, action in selected:
        ct_path = changed_targets[ct_name]
        case_name = f"A/{timing}/{ct_name}/{order}/{action}"
        try:
            h.ensure_alive()
            # 1. Reset to deep target (4 levels deep)
            h.show(fix.target_chain)
            if timing == "idle":
                h.wait_settled(timeout=10.0, wait_bake=True, min_quiet=0.25)
            else:
                # Trigger a fresh jump so capture/disk load is in flight
                h.show(fix.alt_chain)
                time.sleep(0.05)
                h.show(fix.target_chain)
                time.sleep(0.04)

            if order == "before_changed":
                run_action(action)
                time.sleep(0.05)
                h.debug_changed(ct_path)
            else:
                h.debug_changed(ct_path)
                time.sleep(0.05)
                run_action(action)

            st = h.wait_settled(timeout=10.0, wait_bake=True, min_quiet=0.25)
            png = None
            if capture_all or action in ("pinch_out_2", "show_move"):
                png = h.capture(case_name)
            errs = check_invariants(st, case_name, settled=True, png_path=png)
        except Exception as exc:
            st = None
            errs = [f"例外/クラッシュ: {exc}"]
            print(h.tail_logs(25), flush=True)
            h.stop()
            h.start()
        res = CaseResult(
            name=case_name,
            passed=len(errs) == 0,
            errors=errs,
            state=st if errs else None,
        )
        results.append(res)
        status = "PASS" if res.passed else "FAIL"
        print(f"[{status}] {case_name}", flush=True)
        for e in errs:
            print(f"    -> {e}", flush=True)
        if errs and stop_on_fail:
            return results
    return results


def run_group_b(
    h: LisaHarness, fix: Fixture, *, stop_on_fail: bool
) -> list[CaseResult]:
    """既知の 3 症状および競合エッジケースを狙い撃ちする検証。"""
    results: list[CaseResult] = []

    def record(name: str, st: dict[str, Any], png: Path | None, extra_errs: list[str] | None = None) -> bool:
        errs = check_invariants(st, name, settled=True, png_path=png)
        if extra_errs:
            errs.extend(extra_errs)
        res = CaseResult(name=name, passed=len(errs) == 0, errors=errs, state=st if errs else None)
        results.append(res)
        print(f"[{'PASS' if res.passed else 'FAIL'}] {name}", flush=True)
        for e in errs:
            print(f"    -> {e}", flush=True)
        return not res.passed and stop_on_fail

    def run_case(name: str, fn: Any) -> bool:
        try:
            h.ensure_alive()
            res_tuple = fn()
            if res_tuple is None:
                return False
            if len(res_tuple) == 3:
                st, png, extra = res_tuple
            else:
                st, png = res_tuple
                extra = None
            return record(name, st, png, extra_errs=extra)
        except Exception as exc:
            errs = [f"例外/クラッシュ: {exc}"]
            print(f"[FAIL] {name}\n    -> {errs[0]}", flush=True)
            print(h.tail_logs(35), flush=True)
            results.append(CaseResult(name=name, passed=False, errors=errs))
            h.stop()
            h.start()
            return stop_on_fail

    def b1() -> Any:
        h.show(fix.target_chain)
        h.wait_settled(timeout=10.0)
        if not do_click_child(h):
            return None
        time.sleep(0.38)
        h.debug_changed(fix.sibling_path)
        time.sleep(0.05)
        st = h.wait_settled(timeout=10.0)
        return st, h.capture("B1_click_fade_vs_changed")

    if run_case("B1/click_fade_vs_changed", b1):
        return results

    def b2() -> Any:
        h.show(fix.target_chain)
        h.wait_settled(timeout=10.0)
        do_pinch_out_2(h)
        st = h.wait_settled(timeout=10.0)
        return st, h.capture("B2_pinch_out_2_reanchor")

    if run_case("B2/pinch_out_2_reanchor", b2):
        return results

    def b3() -> Any:
        h.show(fix.target_chain)
        h.wait_settled(timeout=10.0)
        h.pinch(-0.10, touching=True)
        time.sleep(0.04)
        h.pan(30.0, 0.0, phase="tap")
        time.sleep(0.1)
        h.pinch(0.0, touching=False)
        time.sleep(0.15)
        h.debug_changed(fix.ancestor_path)
        st = h.wait_settled(timeout=10.0)
        return st, h.capture("B3_parent_scene_plus_ancestor_changed")

    if run_case("B3/parent_scene_plus_ancestor_changed", b3):
        return results

    def b4() -> Any:
        """ピンチ操作中に祖先の Changed が届いても操作が切れず保留され、停止後に正しくそろえられる。"""
        h.show(fix.target_chain)
        h.wait_settled(timeout=10.0)
        h.pinch(-0.25, touching=True)
        time.sleep(0.04)
        h.debug_changed(fix.ancestor_path)
        time.sleep(0.08)
        mid = h.debug_state()
        extra: list[str] = []
        if not mid["gesture_active"]:
            extra.append("B4: ピンチ中に Changed が届いて操作が途中で切れた (gesture_active=False)")
        if not mid["nav_top_attached"]:
            extra.append("B4: ピンチ中に nav.top() が detached になった")
        h.pinch(-0.25, touching=True)
        time.sleep(0.04)
        h.pinch(0.0, touching=False)
        st = h.wait_settled(timeout=10.0)
        return st, h.capture("B4_ancestor_changed_during_pinch"), extra

    if run_case("B4/ancestor_changed_during_pinch", b4):
        return results

    def b5() -> Any:
        """クリックの Animating 中に祖先の Changed が届いてもクラッシュせず、保留後に正しくそろえられる。"""
        h.show(fix.target_chain)
        h.wait_settled(timeout=10.0)
        if not do_click_child(h):
            return None
        time.sleep(0.10)
        h.debug_changed(fix.ancestor_path)
        time.sleep(0.06)
        mid = h.debug_state()
        extra: list[str] = []
        if not mid["nav_top_attached"]:
            extra.append("B5: Animating 中に nav.top() が detached になった")
        st = h.wait_settled(timeout=10.0)
        return st, h.capture("B5_ancestor_changed_during_animating"), extra

    if run_case("B5/ancestor_changed_during_animating", b5):
        return results

    def b6() -> Any:
        h.show(fix.target_chain)
        h.wait_settled(timeout=10.0)
        h.nav_action("up")
        time.sleep(0.15)
        h.debug_changed(fix.parent_path)
        st = h.wait_settled(timeout=10.0)
        h.nav_action("up")
        st = h.wait_settled(timeout=10.0)
        return st, h.capture("B6_up_x2_with_changed")

    if run_case("B6/up_x2_with_changed", b6):
        return results

    def b7() -> Any:
        h.show(fix.target_chain)
        h.wait_settled(timeout=10.0)
        if not do_click_child(h):
            return None
        time.sleep(0.12)
        do_pinch_out_2(h)
        st = h.wait_settled(timeout=10.0)
        return st, h.capture("B7_interrupt_anim_with_pinch_out_2")

    if run_case("B7/interrupt_anim_with_pinch_out_2", b7):
        return results

    def b8() -> Any:
        h.show(fix.target_chain)
        h.wait_settled(timeout=10.0)
        if not do_click_child(h):
            return None
        time.sleep(0.43)
        do_pinch_out_1(h)
        st = h.wait_settled(timeout=10.0)
        return st, h.capture("B8_interrupt_fade_with_pinch_out")

    if run_case("B8/interrupt_fade_with_pinch_out", b8):
        return results

    def b9() -> Any:
        h.show(fix.target_chain)
        time.sleep(0.03)
        h.debug_changed(fix.ancestor_path)
        time.sleep(0.03)
        h.debug_changed(fix.parent_path)
        st = h.wait_settled(timeout=10.0)
        return st, h.capture("B9_double_ancestor_changed")

    if run_case("B9/double_ancestor_changed", b9):
        return results

    def b10() -> Any:
        """Bake が走っている最中に子をクリックし、Fade(fade_in, replaces=old) 中に BakeFinished を踏む再現。"""
        # First jump somewhere else, then jump to target and immediately click child while Bake is running
        h.show(fix.parent_chain)
        h.wait_settled(timeout=10.0)
        h.show(fix.target_chain)
        # Wait just enough for jump to populate target's layout, while Bake is still Running/Loading
        deadline = time.monotonic() + 3.0
        while time.monotonic() < deadline:
            st0 = h.debug_state()
            if st0["nav_names"] and st0["nav_names"][-1] == fix.target_path.name:
                break
            time.sleep(0.02)
        if not do_click_child(h):
            return None
        # After click settles, zoom out 2 levels so reanchor exposes any orphaned scene!
        st = h.wait_settled(timeout=10.0)
        return st, h.capture("B10_click_fade_during_bake")

    if run_case("B10/click_fade_during_bake", b10):
        return results

    return results


def run_group_c(
    h: LisaHarness, fix: Fixture, *, stop_on_fail: bool
) -> list[CaseResult]:
    """サムネの色の食い違い検証: 撮った/焼いた直後と、再起動してディスクから読み直したあとの色を比べる。"""
    results: list[CaseResult] = []
    name = "C/thumbnail_color_consistency"
    errs: list[str] = []

    # キャッシュとメモリを空の状態から始める（前のテストで開いた兄弟フォルダのサムネがメモリに残らないように）
    h.stop()
    cache_imgs = h.tmp_dir / "cache" / "folder-images"
    if cache_imgs.exists():
        shutil.rmtree(cache_imgs, ignore_errors=True)
    h.start()
    h.show(fix.target_chain)
    h.wait_settled(timeout=12.0, wait_bake=True, min_quiet=0.6)
    # 親へ戻る（撮影直後のメモリ上のサムネで親のタイルが描かれる）
    h.nav_action("up")
    st1 = h.wait_settled(timeout=12.0, wait_bake=True, min_quiet=0.6)
    png1 = h.capture("C1_after_fresh_capture")
    errs.extend(check_invariants(st1, "C1", settled=True, png_path=png1))

    tgt_name = fix.target_path.name
    tile1 = next((t for t in st1["child_tiles"] if t["name"] == tgt_name), None)
    if tile1 is None or not tile1["screen_rect"]:
        errs.append(f"C1: 親フォルダに {tgt_name!r} のタイルが見つかりません")
        results.append(CaseResult(name=name, passed=False, errors=errs, state=st1))
        print(f"[FAIL] {name}\n    -> {errs[-1]}", flush=True)
        return results
    if not tile1["has_image"]:
        errs.append(f"C1: {tgt_name!r} のサムネ画像がまだ付いていません")
    # まだ一度も開いていない兄弟フォルダは先読みで焼かれず画像なしであることを確認する。
    sib_name = fix.sibling_path.name
    sib_tile = next((t for t in st1["child_tiles"] if t["name"] == sib_name), None)
    if sib_tile is not None and sib_tile["has_image"]:
        errs.append(f"C1: 未開の兄弟フォルダ {sib_name!r} に先読み画像が付いています")
    rgb1 = sample_rect_rgb(png1, tile1["screen_rect"], st1["window_scale_factor"])
    fmt1 = tile1["image_format"]
    if fmt1 != "Rgba8UnormSrgb":
        errs.append(f"C1: 撮影直後の TextureFormat が Rgba8UnormSrgb でない: {fmt1}")

    # Wait briefly so async WebP writer flushes to disk
    time.sleep(0.8)

    # 2. Restart Lisa with the same LISA_DATA_DIR so it loads the WebP from disk
    h.stop()
    h.start()
    h.show(fix.parent_chain)
    st2 = h.wait_settled(timeout=12.0, wait_bake=True, min_quiet=0.6)
    png2 = h.capture("C2_after_disk_reload")
    errs.extend(check_invariants(st2, "C2", settled=True, png_path=png2))

    tile2 = next((t for t in st2["child_tiles"] if t["name"] == tgt_name), None)
    if tile2 is None or not tile2["screen_rect"]:
        errs.append(f"C2: 再起動後に {tgt_name!r} のタイルが見つかりません")
    else:
        if not tile2["has_image"]:
            errs.append(f"C2: 再起動後に {tgt_name!r} のサムネ画像がディスクから読まれていません")
        rgb2 = sample_rect_rgb(png2, tile2["screen_rect"], st2["window_scale_factor"])
        fmt2 = tile2["image_format"]
        if fmt1 != fmt2:
            errs.append(
                f"サムネの TextureFormat が不一致: 直後={fmt1} vs 再読込後={fmt2}"
            )
        diff = [abs(a - b) for a, b in zip(rgb1, rgb2)]
        max_ch_diff = max(diff)
        print(
            f"    [color] tile={tgt_name!r} fresh={tuple(round(v, 1) for v in rgb1)} ({fmt1}) "
            f"vs reload={tuple(round(v, 1) for v in rgb2)} ({fmt2}) max_diff={max_ch_diff:.1f}",
            flush=True,
        )
        if max_ch_diff > 10.0:
            errs.append(
                f"サムネの色が一致しない: fresh={rgb1} vs reload={rgb2} (max_diff={max_ch_diff:.1f} > 10.0)"
            )

    # 3. 開き直したらサムネの WebP が更新（上書き）されることも確かめる
    webp_files = list((h.tmp_dir / "cache" / "folder-images").glob("*.webp"))
    before_mtimes = {p: p.stat().st_mtime_ns for p in webp_files}
    time.sleep(0.05)
    h.show(fix.target_chain)
    h.wait_settled(timeout=12.0, wait_bake=True, min_quiet=0.6)
    time.sleep(0.6)
    after_files = list((h.tmp_dir / "cache" / "folder-images").glob("*.webp"))
    updated = any(
        p.stat().st_mtime_ns > before_mtimes.get(p, 0) for p in after_files
    )
    if not updated:
        errs.append("C3: フォルダを開き直したのにサムネ WebP が更新されませんでした")

    res = CaseResult(name=name, passed=len(errs) == 0, errors=errs, state=st2 if errs else None)
    results.append(res)
    print(f"[{'PASS' if res.passed else 'FAIL'}] {name}", flush=True)
    for e in errs:
        print(f"    -> {e}", flush=True)
    return results


def main() -> int:
    ap = argparse.ArgumentParser(description="Lisa レンダリング・ナビゲーション自動検証")
    ap.add_argument("--bin", type=Path, default=DEFAULT_BIN, help="Lisa バイナリのパス")
    ap.add_argument(
        "--mode",
        choices=["all", "quick", "group-a", "group-b", "group-c"],
        default="quick",
        help="実行する検証グループ（既定: quick、全件は all）",
    )
    ap.add_argument(
        "--stop-on-fail",
        action="store_true",
        help="最初の不変条件違反で止める",
    )
    ap.add_argument(
        "--capture-all",
        action="store_true",
        help="Group A の全組み合わせで capture PNG を撮る",
    )
    ap.add_argument(
        "--keep-tmp",
        action="store_true",
        help="終了後も一時データディレクトリを残す",
    )
    args = ap.parse_args()

    if not args.bin.exists():
        print(f"バイナリが見つかりません: {args.bin}", file=sys.stderr)
        return 2

    harness = LisaHarness(args.bin, keep_tmp=args.keep_tmp)

    def _on_sig(signum: int, _frame: Any) -> None:
        harness.cleanup()
        sys.exit(128 + signum)

    signal.signal(signal.SIGINT, _on_sig)
    signal.signal(signal.SIGTERM, _on_sig)

    all_results: list[CaseResult] = []
    try:
        fix = harness.prepare()
        print(
            f"Fixture:\n"
            f"  ancestor: {fix.ancestor_path} ({fix.ancestor_chain})\n"
            f"  parent:   {fix.parent_path} ({fix.parent_chain})\n"
            f"  target:   {fix.target_path} ({fix.target_chain})\n"
            f"  sibling:  {fix.sibling_path} ({fix.sibling_chain})\n"
            f"  child:    {fix.child_path} ({fix.child_chain})\n"
            f"  tmp_dir:  {harness.tmp_dir}",
            flush=True,
        )
        harness.start()

        if args.mode in ("all", "quick", "group-b"):
            all_results.extend(
                run_group_b(harness, fix, stop_on_fail=args.stop_on_fail)
            )
            if args.stop_on_fail and any(not r.passed for r in all_results):
                raise SystemExit(1)

        if args.mode in ("all", "quick", "group-a"):
            all_results.extend(
                run_group_a(
                    harness,
                    fix,
                    quick=(args.mode == "quick"),
                    stop_on_fail=args.stop_on_fail,
                    capture_all=args.capture_all,
                )
            )
            if args.stop_on_fail and any(not r.passed for r in all_results):
                raise SystemExit(1)

        if args.mode in ("all", "quick", "group-c"):
            all_results.extend(
                run_group_c(harness, fix, stop_on_fail=args.stop_on_fail)
            )

    except SystemExit:
        pass
    finally:
        failed = [r for r in all_results if not r.passed]
        if failed:
            harness.keep_tmp = True
        harness.cleanup()

    passed = sum(1 for r in all_results if r.passed)
    failed_list = [r for r in all_results if not r.passed]
    print("\n=== SUMMARY ===", flush=True)
    print(f"Total: {len(all_results)}  Passed: {passed}  Failed: {len(failed_list)}", flush=True)
    if failed_list:
        print(f"Failure logs/screenshots kept at: {harness.tmp_dir}", flush=True)
        for r in failed_list:
            print(f"  FAIL {r.name}:", flush=True)
            for e in r.errors:
                print(f"    - {e}", flush=True)
            if r.state:
                brief = {
                    k: r.state[k]
                    for k in (
                        "nav_names",
                        "breadcrumb",
                        "anchor_name",
                        "anchor_attached",
                        "nav_top_in_layout",
                        "camera_scale",
                        "fit_scale",
                        "main_scenes",
                        "scenes",
                        "gesture_has_parent_scene",
                        "bake_state",
                        "jump_pending",
                        "tree_changed_pending",
                    )
                }
                print(f"    state: {json.dumps(brief, ensure_ascii=False)}", flush=True)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
