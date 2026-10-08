#!/usr/bin/env python3
"""テスト ID 帳簿とコード内 ID コメントの照合スクリプト。

帳簿は docs/implementation-plan.md の「## テスト ID 帳簿」節の表から、
テストケース列の `PREFIX-NN:` 形式を抽出する。
コードは src/**/*.rs と mock/**/*.rs の行コメント(//、///、//!)中の
`[A-Z]{2,5}-\\d{2,3}` トークンを抽出する(コロン直後一致は要求しない)。
文字列・raw 文字列・文字リテラル中の `//` はコメント開始とみなさない。

分類:
- コードのみ出現 -> error(帳簿未登録)
- 帳簿のみ + 「単体テストなし」または「実機検証」マーカー -> info(正常)
- 帳簿のみ + マーカーなし -> error
- 同一 ID が複数ファイルに出現 -> warning(非 fatal)

全件一致時は接頭辞ごとの件数サマリを表示して終了コード 0、
error が 1 件でもあれば終了コード 1。
`--next` で各接頭辞の次の空き番号も併せて表示する。
"""

import argparse
import re
import sys
from collections import Counter
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
LEDGER_PATH = ROOT / "docs" / "implementation-plan.md"
LEDGER_HEADING = "## テスト ID 帳簿"
SCAN_DIRS = ("src", "mock")

# 接頭辞 2〜5 文字・番号 2〜3 桁の制約で要件 ID(F-01、N-01 系)、UTF-8、
# ISO-8601 等の非 ID を除く。前後は単語文字とハイフンを排除し、
# FOO-BAR-01 の末尾や XPOST-01 の部分一致を除く。
ID_TOKEN_RE = re.compile(r"(?<![A-Za-z0-9-])[A-Z]{2,5}-\d{2,3}(?![0-9-])")
LEDGER_ID_RE = re.compile(r"(?<![A-Za-z0-9-])([A-Z]{2,5}-\d{2,3})(?![0-9-])\s*:")
CHAR_LIT_RE = re.compile(r"'(?:\\.|[^'\\])'")
NO_TEST_MARKERS = ("単体テストなし", "実機検証")


def ledger_section_lines(path: Path) -> list[str]:
    """「## テスト ID 帳簿」節から次の `## ` 見出しまでの行を返す。"""
    lines = path.read_text(encoding="utf-8").splitlines()
    try:
        start = next(i for i, line in enumerate(lines) if line.strip() == LEDGER_HEADING)
    except StopIteration:
        sys.exit(f"error: {path} に {LEDGER_HEADING} 節が見つかりません")
    out = []
    for line in lines[start + 1 :]:
        if line.startswith("## "):
            break
        out.append(line)
    return out


def parse_ledger(path: Path) -> dict[str, str]:
    """帳簿のテストケース列から ID -> 説明文 の辞書を返す。"""
    entries: dict[str, str] = {}
    for line in ledger_section_lines(path):
        s = line.strip()
        if not s.startswith("|") or s.startswith("|-") or s.startswith("| 接頭辞"):
            continue
        cells = [c.strip() for c in s.strip("|").split("|")]
        if len(cells) < 3:
            continue
        case_text = cells[2]
        matches = list(LEDGER_ID_RE.finditer(case_text))
        for i, m in enumerate(matches):
            end = matches[i + 1].start() if i + 1 < len(matches) else len(case_text)
            entries[m.group(1)] = case_text[m.end() : end].strip(" 、")
    return entries


def comment_part(line: str) -> str:
    """行の行コメント部分を返す。
    文字列・raw 文字列・文字リテラル中の `//` と、URL スキーム `://` の
    `//` はコメント開始とみなさない。
    複数行にまたがる raw 文字列は対象外(このコードベースでは使われていない)。
    """
    in_str = False
    i = 0
    n = len(line)
    while i < n - 1:
        c = line[i]
        if in_str:
            if c == "\\":
                i += 2
                continue
            if c == '"':
                in_str = False
            i += 1
            continue
        if c == '"':
            in_str = True
            i += 1
            continue
        if c == "'":
            m = CHAR_LIT_RE.match(line, i)
            if m:
                i = m.end()
                continue
        if c == "/" and line[i + 1] == "/":
            if i == 0 or line[i - 1] != ":":
                return line[i:]
        i += 1
    return ""


def scan_code_ids() -> dict[str, set[Path]]:
    """src/ と mock/ の .rs のコメントから ID -> 出現ファイル集合 を返す。"""
    id_files: dict[str, set[Path]] = {}
    for base_name in SCAN_DIRS:
        for rs in sorted((ROOT / base_name).rglob("*.rs")):
            try:
                text = rs.read_text(encoding="utf-8")
            except (OSError, UnicodeDecodeError):
                continue
            for line in text.splitlines():
                for m in ID_TOKEN_RE.finditer(comment_part(line)):
                    id_files.setdefault(m.group(0), set()).add(rs)
    return id_files


def rel(files: set[Path]) -> str:
    return ", ".join(str(p.relative_to(ROOT)) for p in sorted(files))


def next_free(ids: list[str]) -> dict[str, str]:
    """接頭辞ごとの最大連番+1 を次の空き番号として返す。
    抽出形式の桁上限(3 桁)を超える候補は出せない旨を明示する。"""
    by_prefix: dict[str, int] = {}
    for tid in ids:
        prefix, num = tid.rsplit("-", 1)
        by_prefix[prefix] = max(by_prefix.get(prefix, 0), int(num))
    out = {}
    for p, v in sorted(by_prefix.items()):
        out[p] = f"{p}-{v + 1:02d}" if v < 999 else f"{p}-(番号の桁上限 999 に到達)"
    return out


def main() -> int:
    parser = argparse.ArgumentParser(
        description="テスト ID 帳簿とコード内 ID コメントの照合"
    )
    parser.add_argument(
        "--next",
        action="store_true",
        help="各接頭辞の次の空き番号を表示する",
    )
    args = parser.parse_args()

    ledger = parse_ledger(LEDGER_PATH)
    code = scan_code_ids()

    infos: list[str] = []
    warnings: list[str] = []
    errors: list[str] = []

    for tid in sorted(set(code) - set(ledger)):
        errors.append(f"{tid}: コードにのみ出現(帳簿未登録) [{rel(code[tid])}]")

    for tid in sorted(set(ledger) - set(code)):
        if any(marker in ledger[tid] for marker in NO_TEST_MARKERS):
            infos.append(f"{tid}: 帳簿のみ({NO_TEST_MARKERS[0]}の実機検証扱い)")
        else:
            errors.append(f"{tid}: 帳簿のみでコードに ID コメントがない")

    for tid in sorted(code):
        if len(code[tid]) > 1:
            warnings.append(f"{tid}: 複数ファイルに出現 [{rel(code[tid])}]")

    print("=== テスト ID 照合 ===")
    print(f"帳簿登録 {len(ledger)} 件、コード内 ID {len(code)} 件")
    for msg in infos:
        print(f"info: {msg}")
    for msg in warnings:
        print(f"warning: {msg}")
    for msg in errors:
        print(f"error: {msg}")

    if errors:
        print(f"不一致 {len(errors)} 件")
    else:
        counts = Counter(tid.split("-")[0] for tid in ledger)
        summary = "、".join(f"{p} {c} 件" for p, c in sorted(counts.items()))
        print(f"一致(接頭辞別): {summary}")

    if args.next:
        for prefix, suggestion in next_free(list(ledger) + list(code)).items():
            print(f"next: {prefix} -> {suggestion}")

    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
