#!/usr/bin/env python3
"""Regenerate the measured section of docs/benchmarks.md from scripts/bench.py JSON reports.

Everything between the `<!-- bench:begin -->` and `<!-- bench:end -->` markers is replaced,
so the published numbers are always the ones in docs/benchmarks/*.json.

Usage:
    scripts/bench-report.py                     # all docs/benchmarks/*.json
    scripts/bench-report.py results/*.json      # specific reports
    scripts/bench-report.py --check             # exit 1 if the doc is out of date
"""

import argparse
import json
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
DOC = REPO / "docs" / "benchmarks.md"
BEGIN = "<!-- bench:begin -->"
END = "<!-- bench:end -->"
PHASE_COMMANDS = ("new", "merge (one file)", "ls (5 workspaces)")


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("reports", nargs="*", type=Path)
    parser.add_argument("--doc", type=Path, default=DOC)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()

    paths = args.reports or sorted((REPO / "docs" / "benchmarks").glob("*.json"))
    reports = [json.loads(path.read_text()) for path in paths]
    reports.sort(key=lambda report: (report["host"]["os"], report["measured_at"]))
    generated = render(reports)

    text = args.doc.read_text()
    if BEGIN not in text or END not in text:
        sys.exit(f"{args.doc} has no {BEGIN} ... {END} block")
    head, rest = text.split(BEGIN, 1)
    _, tail = rest.split(END, 1)
    updated = f"{head}{BEGIN}\n{generated}{END}{tail}"
    if args.check:
        if updated != text:
            sys.exit(f"{args.doc} is out of date; run scripts/bench-report.py")
        return
    args.doc.write_text(updated)


def render(reports):
    if not reports:
        return "\nNo benchmark reports are recorded yet.\n\n"
    out = []
    for report in reports:
        out.append(render_report(report))
    return "".join(out)


def render_report(report):
    host = report["host"]
    corpus = report["corpus"]
    date = report["measured_at"][:10]
    system = "macOS " + host.get("os_version", "") if host["os"] == "Darwin" else f"Linux {host['os_release']}"
    lines = [
        "",
        f"### {system.strip()}, {host.get('filesystem') or 'unknown fs'}: {corpus['name']} ({date})",
        "",
        f"{report['forkspace']}; {host['cpu']}, {host['cpus']} CPUs, {gib(host.get('memory_bytes'))} RAM"
        + ("; shared CI runner" if host.get("ci") else "")
        + f". Corpus: {corpus['entries']:,} entries, {mib(corpus['allocated_bytes'])} allocated. "
        + f"{report['runs']} runs per backend after {report['warmup']} warm-up, a fresh "
        + "`FORKSPACE_HOME` each run; times are wall-clock for the whole command, p50 / p95.",
        "",
    ]
    backends = [backend for backend in report["backends"] if backend.get("summary")]
    if backends:
        labels = []
        for backend in backends:
            for label in backend["summary"]:
                if label not in labels:
                    labels.append(label)
        header = "| Command | " + " | ".join(f"`{b['kind']}`" for b in backends) + " |"
        lines += [header, "| :-- |" + " --: |" * len(backends)]
        for label in labels:
            cells = [cell(backend["summary"].get(label)) for backend in backends]
            lines.append(f"| `{label}` | " + " | ".join(cells) + " |")
        lines.append("")
        lines += render_phases(backends)
        lines += render_disk_and_freeze(backends)
    for backend in report["backends"]:
        for error in backend.get("errors", []):
            lines.append(f"- `{backend['requested']}` failed: {error}")
    if any(backend.get("errors") for backend in report["backends"]):
        lines.append("")
    return "\n".join(lines) + "\n"


def render_phases(backends):
    lines = []
    for backend in backends:
        parts = []
        for command in PHASE_COMMANDS:
            phases = backend.get("phases", {}).get(command)
            if not phases:
                continue
            inner = ", ".join(
                f"{phase} {ms(summary['p50'])}" for phase, summary in phases.items() if phase != "command"
            )
            if inner:
                parts.append(f"`{command}`: {inner}")
        if parts:
            lines.append(f"- Phases on `{backend['kind']}` (p50): " + "; ".join(parts) + ".")
    return lines + ([""] if lines else [])


def render_disk_and_freeze(backends):
    lines = []
    for backend in backends:
        disk = backend.get("disk")
        if disk:
            lines.append(
                f"- Disk after one fork on `{backend['kind']}`: forkspace's own allocation "
                f"{mib(disk['home_allocated_bytes'])}; volume free space changed by "
                f"{mib(disk['volume_free_delta_bytes'])} (noisy)."
            )
        freeze = backend.get("freeze")
        if freeze and freeze.get("during_fork_write_ms") and freeze.get("baseline_write_ms"):
            during, idle = freeze["during_fork_write_ms"], freeze["baseline_write_ms"]
            lines.append(
                f"- Project writes while `new` forked it on `{backend['kind']}` "
                f"({during['n']} writes over {ms(freeze['fork_ms'])}): p99 {ms(during['p99'])}, "
                f"max {ms(during['max'])}; idle p99 {ms(idle['p99'])}, max {ms(idle['max'])}."
            )
    return lines + ([""] if lines else [])


def cell(summary):
    if not summary:
        return "-"
    return f"{ms(summary['p50'])} / {ms(summary['p95'])}"


def ms(value):
    if value >= 1000:
        return f"{value / 1000:.2f} s"
    if value >= 100:
        return f"{value:.0f} ms"
    return f"{value:.1f} ms"


def mib(value):
    if value is None:
        return "?"
    return f"{value / 2**20:.1f} MiB"


def gib(value):
    if not value:
        return "?"
    return f"{value / 2**30:.0f} GiB"


if __name__ == "__main__":
    main()
