#!/usr/bin/env python3
"""Build the whole course into one PDF: docs/course/2khz-course.pdf.

    python3 -m venv /tmp/pdfenv
    /tmp/pdfenv/bin/pip install weasyprint markdown pygments
    /tmp/pdfenv/bin/python docs/course/build-pdf.py

Each chapter is converted on its own so heading ids can be prefixed per
chapter, every chapter has a "Check yourself", and links between chapter
files become links within the PDF.
"""

import re
from pathlib import Path

import markdown
from pygments.formatters import HtmlFormatter
from weasyprint import HTML

HERE = Path(__file__).resolve().parent
REPO = "https://github.com/nab-os/2kHz/blob/main"
ORDER = ["README.md", *sorted(HERE.glob("[0-9][0-9]-*.md")), "glossary.md"]


def slug(name: str) -> str:
    return "ch-" + Path(name).stem.lower()


def nest_lists(text: str) -> str:
    """Python-Markdown nests lists at four spaces; the course uses two."""
    return re.sub(r"(?m)^  ([-*]|\d+\.) ", r"    \1 ", text)


def keep_chapter_numbers(text: str) -> str:
    """The contents list starts at chapter 0 and restarts numbering per part,
    and Python-Markdown renumbers every ordered list from 1. Spell the
    numbers out instead."""
    return re.sub(r"(?m)^(\d+)\. (\[)", r"- **\1.** \2", text)


def rewrite_link(match: re.Match, chapter: str) -> str:
    href = match.group(1)
    if href.startswith(("http://", "https://", "mailto:")):
        return match.group(0)
    if href.startswith("#"):
        return f'href="#{slug(chapter)}-{href[1:]}"'
    target, _, anchor = href.partition("#")
    name = Path(target).name
    if (HERE / name).exists() and target == name:
        return f'href="#{slug(name)}"'
    # Anything outside the course lives on GitHub.
    resolved = (HERE / target).resolve().relative_to(HERE.parents[1])
    return f'href="{REPO}/{resolved}{"#" + anchor if anchor else ""}"'


def chapter_html(path: Path) -> str:
    md = markdown.Markdown(
        extensions=["extra", "codehilite", "toc", "sane_lists"],
        extension_configs={"codehilite": {"guess_lang": False, "css_class": "hl"}},
    )
    body = md.convert(keep_chapter_numbers(nest_lists(path.read_text())))
    prefix = slug(path.name)
    body = re.sub(r'id="([^"]+)"', lambda m: f'id="{prefix}-{m.group(1)}"', body)
    body = re.sub(r'href="([^"]+)"', lambda m: rewrite_link(m, path.name), body)
    return f'<section class="chapter" id="{prefix}">{body}</section>'


def title_of(path: Path) -> str:
    first = path.read_text().splitlines()[0]
    return first.lstrip("# ").strip().replace("`", "")


CSS = """
@page { size: A4; margin: 22mm 20mm 24mm 20mm;
        @bottom-center { content: counter(page); font: 9pt 'DejaVu Sans'; color: #777; } }
@page :first { @bottom-center { content: none; } }
body { font: 10.5pt/1.5 'DejaVu Serif', serif; color: #1d1d1f; }
h1, h2, h3, h4 { font-family: 'DejaVu Sans', sans-serif; color: #1f3a68; line-height: 1.25; }
h1 { font-size: 22pt; margin: 0 0 12pt; border-bottom: 2px solid #1f3a68; padding-bottom: 4pt;
     string-set: chapter content(); }
h2 { font-size: 14pt; margin-top: 18pt; }
h3 { font-size: 11.5pt; margin-top: 14pt; }
h2, h3, h4 { page-break-after: avoid; }
.chapter { page-break-before: always; }
a { color: #1f5fbf; text-decoration: none; }
code { font: 8.8pt 'DejaVu Sans Mono', monospace; background: #f1f3f6; padding: 0 2px;
       border-radius: 2px; }
pre, .hl { font: 7.6pt/1.4 'DejaVu Sans Mono', monospace; background: #f6f8fa;
           border: 1px solid #dde2e8; border-radius: 4px; padding: 7pt 9pt;
           white-space: pre-wrap; overflow-wrap: anywhere; page-break-inside: avoid; }
.hl pre { border: none; padding: 0; margin: 0; background: none; }
/* Pygments flags π, ±, · and friends as errors; they are prose in code. */
.hl .err { border: none !important; color: inherit !important; background: none !important; }
pre code, .hl code { background: none; padding: 0; }
table { border-collapse: collapse; width: 100%; margin: 8pt 0; font-size: 9pt;
        page-break-inside: avoid; }
th, td { border: 1px solid #d0d6de; padding: 3pt 6pt; vertical-align: top; text-align: left; }
th { background: #eef2f7; font-family: 'DejaVu Sans', sans-serif; }
blockquote { margin: 8pt 0; padding: 2pt 12pt; border-left: 3px solid #9fb3d1; color: #444; }
.cover { page-break-after: always; text-align: center; padding-top: 70mm; }
.cover h1 { border: none; font-size: 34pt; string-set: none; }
.cover p { font: 12pt 'DejaVu Sans', sans-serif; color: #555; }
.toc h1 { string-set: none; }
#ch-readme ul { list-style: none; padding-left: 8pt; }
.toc ol { list-style: none; padding: 0; font: 11pt/1.9 'DejaVu Sans', sans-serif; }
.toc a::after { content: leader('.') target-counter(attr(href), page); }
"""


def main() -> None:
    chapters = [HERE / p if isinstance(p, str) else p for p in ORDER]
    toc = "".join(
        f'<li><a href="#{slug(c.name)}">{title_of(c)}</a></li>' for c in chapters
    )
    html = f"""<!doctype html><html><head><meta charset="utf-8">
<style>{CSS}{HtmlFormatter(style="friendly").get_style_defs(".hl")}</style></head><body>
<div class="cover"><h1>2kHz, from the inside</h1>
<p>A course on every part of the app</p><p>written against v0.7.0 (eaa1322)</p></div>
<div class="toc"><h1>Contents</h1><ol>{toc}</ol></div>
{"".join(chapter_html(c) for c in chapters)}
</body></html>"""
    out = HERE / "2khz-course.pdf"
    HTML(string=html, base_url=str(HERE)).write_pdf(out)
    print(f"wrote {out} ({out.stat().st_size / 1e6:.1f} MB)")


if __name__ == "__main__":
    main()
