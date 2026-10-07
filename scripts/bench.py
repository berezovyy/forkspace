#!/usr/bin/env python3
"""Benchmark forkspace's everyday commands on this host and write the results as JSON.

Every run starts from a fresh FORKSPACE_HOME and a private copy of the corpus, then times:
`new`, a clean `diff --stat`, `diff` after one edit, `merge --dry-run`, a one-file `merge`,
`ls` with one and five workspaces, and `drop`. Each command runs with FORKSPACE_TRACE=1 so
the per-phase timings (preflight, materialize, manifest, scan, plan, apply, ...) are kept too.

A freeze probe measures whether the project stays writable while `new` forks it, and the
disk section records what a fork costs in free space. Standard library only, so the same
script runs on Linux and macOS runners.

Usage:
    scripts/bench.py --synthetic 10000 --runs 5
    scripts/bench.py --corpus ~/.cache/forkspace-bench/next-15.5.4.tar.gz --backends auto,copy
    scripts/bench.py --short            # quick end-to-end check, as the test suite runs it
"""

import argparse
import datetime
import json
import os
import platform
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import threading
import time
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
TRACE_LINE = re.compile(r"^forkspace-trace: (\S+) ([0-9.]+)ms$")
EDIT_FILE = "src/bench-edit.txt"
SCHEMA = 1


def main():
    args = parse_args()
    if args.short:
        args.synthetic = args.synthetic or 200
        args.runs = min(args.runs, 2)
        args.freeze = False
    if not args.corpus and not args.synthetic:
        args.synthetic = 10000

    binary = Path(args.bin) if args.bin else build_release()
    work = Path(tempfile.mkdtemp(prefix="forkspace-bench-", dir=args.work))
    try:
        corpus = prepare_corpus(args, work / "corpus-src")
        report = {
            "schema": SCHEMA,
            "measured_at": datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds"),
            "forkspace": version(binary),
            "host": host_info(work),
            "corpus": describe_corpus(args, corpus),
            "runs": args.runs,
            "warmup": args.warmup,
            "backends": [],
        }
        for backend in args.backends.split(","):
            log(f"backend {backend}: {args.runs} runs")
            report["backends"].append(bench_backend(binary, backend, corpus, work, args))
    finally:
        if not args.keep:
            remove_tree(work)

    text = json.dumps(report, indent=2) + "\n"
    if args.out:
        Path(args.out).parent.mkdir(parents=True, exist_ok=True)
        Path(args.out).write_text(text)
        log(f"wrote {args.out}")
    if args.json:
        sys.stdout.write(text)
    else:
        print_summary(report)


def parse_args():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--bin", help="forkspace binary (default: build target/release/forkspace)")
    source = parser.add_mutually_exclusive_group()
    source.add_argument("--corpus", help="project directory or .tar/.tar.gz to fork")
    source.add_argument("--synthetic", type=int, help="generate a project with this many entries")
    parser.add_argument("--backends", default="auto", help="comma list: auto, copy, linux, linux-reflink, mac-clone")
    parser.add_argument("--runs", type=int, default=5)
    parser.add_argument("--warmup", type=int, default=1, help="untimed runs before measuring")
    parser.add_argument("--workspaces", type=int, default=5, help="workspaces alive for the second ls")
    parser.add_argument("--no-freeze", dest="freeze", action="store_false", help="skip the freeze probe")
    parser.add_argument("--short", action="store_true", help="small synthetic corpus, 2 runs, no freeze probe")
    parser.add_argument("--work", help="directory for temporary files (must be on the volume to measure)")
    parser.add_argument("--out", help="write the JSON report here")
    parser.add_argument("--json", action="store_true", help="print the JSON report instead of a summary")
    parser.add_argument("--keep", action="store_true", help="keep the work directory")
    return parser.parse_args()


def log(message):
    print(f"bench: {message}", file=sys.stderr, flush=True)


def build_release():
    subprocess.run(["cargo", "build", "--release", "--quiet", "--manifest-path", str(REPO / "Cargo.toml")], check=True)
    return REPO / "target" / "release" / "forkspace"


def version(binary):
    out = subprocess.run([str(binary), "--version"], capture_output=True, text=True, check=True)
    return out.stdout.strip()


# ---------------------------------------------------------------- corpus


def prepare_corpus(args, dest):
    if args.synthetic:
        generate_synthetic(dest, args.synthetic)
    elif os.path.isfile(args.corpus) and tarfile.is_tarfile(args.corpus):
        dest.mkdir(parents=True)
        with tarfile.open(args.corpus) as archive:
            extract(archive, dest)
        entries = list(dest.iterdir())
        if len(entries) == 1 and entries[0].is_dir():
            return entries[0]
    else:
        copy_tree(Path(args.corpus), dest)
    return dest


def extract(archive, dest):
    if hasattr(tarfile, "data_filter"):
        archive.extractall(dest, filter="data")
    else:
        archive.extractall(dest)


def generate_synthetic(root, entries):
    """`entries` filesystem entries: a small Git-tracked source tree and an ignored
    dependency tree holding most of the files, like a JavaScript project."""
    per_dir = 100
    source_entries = max(entries // 5, 10)
    root.mkdir(parents=True)
    (root / ".gitignore").write_text("node_modules/\n.env\n")
    (root / ".env").write_text("BENCH=1\n")
    (root / "package.json").write_text('{"name": "bench", "private": true}\n')
    made = 3
    made += fill(root / "src", source_entries, per_dir)
    made += fill(root / "node_modules", max(entries - made, 0), per_dir)
    git(root, "init", "-q")
    git(root, "add", "-A")
    git(root, "-c", "user.name=bench", "-c", "user.email=bench@localhost", "commit", "-qm", "corpus")


def fill(root, entries, per_dir):
    """Create about `entries` entries (directories count) of one-byte files under `root`."""
    made = 0
    index = 0
    while made < entries:
        directory = root / f"d{index:05d}"
        directory.mkdir(parents=True)
        made += 1
        for number in range(min(per_dir, entries - made)):
            (directory / f"f{number:03d}.txt").write_bytes(b"x")
            made += 1
        index += 1
    return made


def git(root, *argv):
    subprocess.run(["git", "-C", str(root), *argv], check=True, capture_output=True)


def copy_tree(source, dest):
    """Copy a project, cloning on APFS when `cp -c` is available, keeping symlinks."""
    if platform.system() == "Darwin":
        if subprocess.run(["cp", "-cR", str(source), str(dest)], capture_output=True).returncode == 0:
            return
    elif subprocess.run(["cp", "-a", "--reflink=auto", str(source), str(dest)], capture_output=True).returncode == 0:
        return
    shutil.copytree(source, dest, symlinks=True)


def describe_corpus(args, corpus):
    files, dirs = 0, 0
    for _, dirnames, filenames in os.walk(corpus):
        dirs += len(dirnames)
        files += len(filenames)
    return {
        "kind": "synthetic" if args.synthetic else "project",
        "name": f"synthetic-{args.synthetic}" if args.synthetic else corpus_name(args.corpus),
        "entries": files + dirs,
        "files": files,
        "directories": dirs,
        "allocated_bytes": allocated(corpus),
    }


def corpus_name(path):
    name = Path(path).name
    for suffix in (".tar.gz", ".tgz", ".tar"):
        if name.endswith(suffix):
            return name[: -len(suffix)]
    return name


def allocated(root):
    """Allocated bytes under `root` (st_blocks), not crossing into other mounts."""
    total = 0
    root_dev = os.lstat(root).st_dev
    for dirpath, dirnames, filenames in os.walk(root):
        kept = []
        for name in dirnames:
            path = os.path.join(dirpath, name)
            try:
                st = os.lstat(path)
            except OSError:
                continue
            if st.st_dev == root_dev:
                total += st.st_blocks * 512
                kept.append(name)
        dirnames[:] = kept
        for name in filenames:
            try:
                total += os.lstat(os.path.join(dirpath, name)).st_blocks * 512
            except OSError:
                pass
    return total


# ---------------------------------------------------------------- host


def host_info(work):
    info = {
        "os": platform.system(),
        "os_release": platform.release(),
        "machine": platform.machine(),
        "cpu": cpu_model(),
        "cpus": os.cpu_count(),
        "memory_bytes": memory_bytes(),
        "filesystem": filesystem_type(work),
        "ci": bool(os.environ.get("CI")),
    }
    if platform.system() == "Linux":
        info["fuse_overlayfs"] = command_output(["fuse-overlayfs", "--version"]).split("\n")[0] or None
    else:
        info["os_version"] = platform.mac_ver()[0]
    return info


def command_output(argv):
    try:
        return subprocess.run(argv, capture_output=True, text=True, timeout=10).stdout.strip()
    except (OSError, subprocess.SubprocessError):
        return ""


def cpu_model():
    if platform.system() == "Darwin":
        return command_output(["sysctl", "-n", "machdep.cpu.brand_string"]) or platform.processor()
    try:
        for line in Path("/proc/cpuinfo").read_text().splitlines():
            if line.startswith("model name"):
                return line.split(":", 1)[1].strip()
    except OSError:
        pass
    return platform.processor()


def memory_bytes():
    if platform.system() == "Darwin":
        value = command_output(["sysctl", "-n", "hw.memsize"])
        return int(value) if value.isdigit() else None
    try:
        for line in Path("/proc/meminfo").read_text().splitlines():
            if line.startswith("MemTotal:"):
                return int(line.split()[1]) * 1024
    except OSError:
        pass
    return None


def filesystem_type(path):
    """The type of the filesystem holding `path`, from the longest matching mount point."""
    path = os.path.realpath(path)
    mounts = []
    if platform.system() == "Linux":
        try:
            for line in Path("/proc/self/mounts").read_text().splitlines():
                fields = line.split()
                mounts.append((fields[1].replace("\\040", " "), fields[2]))
        except OSError:
            return None
    else:
        for line in command_output(["mount"]).splitlines():
            match = re.match(r"^.* on (.*) \((\w+)", line)
            if match:
                mounts.append((match.group(1), match.group(2)))
    best = None
    for mount_point, kind in mounts:
        inside = path == mount_point or path.startswith(mount_point.rstrip("/") + "/")
        if inside and (best is None or len(mount_point) > len(best[0])):
            best = (mount_point, kind)
    return best[1] if best else None


# ---------------------------------------------------------------- runs


def bench_backend(binary, backend, corpus, work, args):
    env = dict(os.environ)
    env.pop("FORKSPACE_BACKEND", None)
    env["FORKSPACE_TRACE"] = "1"
    if backend != "auto":
        env["FORKSPACE_BACKEND"] = backend
    project = work / f"project-{backend}"
    copy_tree(corpus, project)

    result = {"requested": backend, "kind": None, "samples": {}, "phases": {}, "errors": []}
    for run in range(args.warmup + args.runs):
        measured = run >= args.warmup
        home = work / f"home-{backend}-{run}"
        home.mkdir()
        env["FORKSPACE_HOME"] = str(home)
        try:
            one_run(binary, env, project, home, args, result if measured else None)
        except BenchError as error:
            result["errors"].append(str(error))
            log(f"backend {backend} run {run}: {error}")
            cleanup_home(binary, env, home)
            break
        cleanup_home(binary, env, home)
    if args.freeze and not result["errors"]:
        home = work / f"home-{backend}-freeze"
        home.mkdir()
        env["FORKSPACE_HOME"] = str(home)
        result["freeze"] = freeze_probe(binary, env, project)
        cleanup_home(binary, env, home)
    remove_tree(project)

    result["summary"] = {name: stats(values) for name, values in result["samples"].items()}
    result["phases"] = {
        command: {phase: stats(values) for phase, values in phases.items()}
        for command, phases in result["phases"].items()
    }
    return result


class BenchError(Exception):
    pass


def one_run(binary, env, project, home, args, result):
    def timed(label, *argv):
        elapsed, phases, stdout = run(binary, env, argv)
        if result is not None:
            result["samples"].setdefault(label, []).append(elapsed)
            per_command = result["phases"].setdefault(label, {})
            for phase, ms in phases.items():
                per_command.setdefault(phase, []).append(ms)
        return stdout

    edit_target = project / EDIT_FILE
    edit_target.parent.mkdir(parents=True, exist_ok=True)
    edit_target.write_text("benchmark\n")

    free_before = shutil.disk_usage(home).free
    view = created_view(timed("new", "new", "ws1", "--from", str(project)))
    kind = workspace_kind(binary, env, "ws1")
    if kind == "mac-clone":
        ready_ms = wait_for_manifest(binary, env, "ws1")
        if result is not None:
            result["samples"].setdefault("manifest ready after new", []).append(ready_ms)
    if result is not None and "disk" not in result:
        result["disk"] = disk_after_fork(home, free_before)
    if result is not None:
        result["kind"] = kind

    timed("diff --stat (clean)", "diff", "ws1", "--stat")
    with open(view / EDIT_FILE, "a") as handle:
        handle.write("edited\n")
    timed("diff (one edit)", "diff", "ws1")
    timed("merge --dry-run", "merge", "ws1", "--dry-run")
    timed("merge (one file)", "merge", "ws1", "--quiet", "--no-install")
    edit_target.write_text("benchmark\n")
    timed("ls (1 workspace)", "ls")
    for number in range(2, args.workspaces + 1):
        run(binary, env, ("new", f"ws{number}", "--from", str(project)))
    timed(f"ls ({args.workspaces} workspaces)", "ls")
    timed("drop", "drop", "ws1")


def run(binary, env, argv):
    started = time.perf_counter()
    done = subprocess.run([str(binary), *argv], env=env, capture_output=True, text=True)
    elapsed = (time.perf_counter() - started) * 1000
    if done.returncode != 0:
        raise BenchError(f"`forkspace {' '.join(argv)}` exited {done.returncode}: {done.stderr.strip()}")
    phases = {}
    for line in done.stderr.splitlines():
        match = TRACE_LINE.match(line)
        if match:
            phases[match.group(1)] = phases.get(match.group(1), 0.0) + float(match.group(2))
    return elapsed, phases, done.stdout


def created_view(stdout):
    match = re.search(r"^created workspace '.*' at (.*)$", stdout, re.MULTILINE)
    if not match:
        raise BenchError(f"unexpected `new` output: {stdout.strip()}")
    return Path(match.group(1))


def wait_for_manifest(binary, env, name, timeout=600):
    """Milliseconds until a background-built fork manifest is ready (macOS clones)."""
    started = time.perf_counter()
    while time.perf_counter() - started < timeout:
        done = subprocess.run([str(binary), "diff", name, "--stat"], env=env, capture_output=True, text=True)
        if "pending manifest build" not in done.stderr:
            return (time.perf_counter() - started) * 1000
        time.sleep(0.05)
    raise BenchError(f"the fork manifest of {name} was not ready after {timeout} s")


def ls_json(binary, env):
    done = subprocess.run([str(binary), "ls", "--json"], env=env, capture_output=True, text=True, check=True)
    return json.loads(done.stdout)


def workspace_kind(binary, env, name):
    for entry in ls_json(binary, env):
        if entry["name"] == name:
            return entry["backend"]
    return None


def disk_after_fork(home, free_before):
    """Free-space change on the volume and forkspace's own allocation after one fork.

    Free space is the honest number for clones (shared blocks are not used twice); the
    allocated size of FORKSPACE_HOME, which stops at mount points, is the honest number for
    overlay (the private upper layer) and the copy backend (a full copy)."""
    return {
        "volume_free_delta_bytes": free_before - shutil.disk_usage(home).free,
        "home_allocated_bytes": allocated(home),
        "note": "volume free space is shared with other processes; treat small deltas as noise",
    }


def cleanup_home(binary, env, home):
    done = subprocess.run([str(binary), "drop", "--all"], env=env, capture_output=True, text=True)
    if done.returncode != 0:
        log(f"`drop --all` failed in {home}: {done.stderr.strip()}")
    remove_tree(home)


def remove_tree(path):
    if not os.path.lexists(path):
        return
    # Overlay views (<home>/<workspace>/merged) can still be mounted if a command failed.
    if platform.system() == "Linux":
        root = Path(path)
        for mounted in [*root.glob("*/merged"), *root.glob("home-*/*/merged")]:
            subprocess.run([fusermount(), "-u", "-z", str(mounted)], capture_output=True)
    subprocess.run(["chmod", "-R", "u+rwX", str(path)], capture_output=True)
    shutil.rmtree(path, ignore_errors=True)


def fusermount():
    """The setuid system fusermount, as forkspace uses; a copy earlier on PATH (Linuxbrew)
    may lack the setuid bit and fail with EPERM."""
    for candidate in ("/usr/bin/fusermount3", "/bin/fusermount3", "/usr/bin/fusermount"):
        if os.path.exists(candidate):
            return candidate
    return "fusermount3"


# ---------------------------------------------------------------- freeze probe


def freeze_probe(binary, env, project, interval=0.002):
    """Write a small file into the project every `interval` seconds, first with nothing else
    running and then while `new` forks the project, and compare the write latencies."""

    def probe(stop, latencies):
        target = project / ".forkspace-bench-probe"
        while not stop.is_set():
            started = time.perf_counter()
            with open(target, "w") as handle:
                handle.write("probe\n")
            latencies.append((time.perf_counter() - started) * 1000)
            time.sleep(interval)

    def measure(action):
        stop, latencies = threading.Event(), []
        thread = threading.Thread(target=probe, args=(stop, latencies))
        thread.start()
        elapsed = action()
        stop.set()
        thread.join()
        return latencies, elapsed

    def fork():
        return run(binary, env, ("new", "freeze", "--from", str(project)))[0]

    try:
        during, fork_ms = measure(fork)
        baseline, _ = measure(lambda: time.sleep(max(fork_ms / 1000, 0.2)))
    finally:
        try:
            (project / ".forkspace-bench-probe").unlink()
        except OSError:
            pass
    return {
        "interval_ms": interval * 1000,
        "fork_ms": fork_ms,
        "baseline_write_ms": stats(baseline),
        "during_fork_write_ms": stats(during),
    }


# ---------------------------------------------------------------- output


def stats(values):
    if not values:
        return None
    ordered = sorted(values)

    def rank(fraction):
        index = max(0, min(len(ordered) - 1, int(-(-fraction * len(ordered) // 1)) - 1))
        return round(ordered[index], 3)

    return {
        "n": len(ordered),
        "min": round(ordered[0], 3),
        "p50": rank(0.50),
        "p95": rank(0.95),
        "p99": rank(0.99),
        "max": round(ordered[-1], 3),
    }


def print_summary(report):
    host = report["host"]
    corpus = report["corpus"]
    print(f"{report['forkspace']} on {host['os']} {host['os_release']} ({host['cpu']}, {host['filesystem']})")
    print(f"corpus {corpus['name']}: {corpus['entries']} entries, {corpus['allocated_bytes'] / 2**20:.1f} MiB allocated")
    for backend in report["backends"]:
        print(f"\nbackend {backend['requested']} -> {backend['kind']}")
        for error in backend["errors"]:
            print(f"  error: {error}")
        width = max((len(name) for name in backend["summary"]), default=0)
        for name, summary in backend["summary"].items():
            print(f"  {name:<{width}}  p50 {summary['p50']:9.2f} ms  p95 {summary['p95']:9.2f} ms  max {summary['max']:9.2f} ms")
        disk = backend.get("disk")
        if disk:
            print(f"  disk after one fork: home {disk['home_allocated_bytes'] / 2**20:.2f} MiB, volume free -{disk['volume_free_delta_bytes'] / 2**20:.2f} MiB")
        freeze = backend.get("freeze")
        if freeze:
            base, during = freeze["baseline_write_ms"], freeze["during_fork_write_ms"]
            if base and during:
                print(f"  project writes during fork: p99 {during['p99']:.2f} ms, max {during['max']:.2f} ms (idle p99 {base['p99']:.2f} ms)")


if __name__ == "__main__":
    main()
