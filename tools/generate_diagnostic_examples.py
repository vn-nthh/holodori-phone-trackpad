"""Generate reproducible healthy/degraded reports using both production analyzers."""
import argparse
import os
from pathlib import Path
import subprocess
from diagnostic_report import android_files, join_report, markdown
import json

ROOT = Path(__file__).resolve().parents[1]


def row(kind, at, seq=0, *fields, session=7):
    return [kind, at, session, seq, *fields, *([0] * (8 - len(fields)))]


def scenario(degraded):
    host, phone = [], []
    offset = 1_000_000_000
    for seq in range(1, 1001):
        at = seq * 8_000_000
        flags = 3 if seq % 16 == 0 else 1
        event = offset + at - 1_000_000
        callback, first = event + 200_000, event + 300_000
        if degraded and seq == 700:
            flags = 3
            event -= 20_000_000
        queued = callback + 20_000
        phone.append(row(20, queued, seq, event, callback, flags, 1, 64))
        for ordinal in [1, 2]:
            start = first + (ordinal - 1) * 20_000
            phone.append(row(21, start + 10_000, seq, start, queued, ordinal, 0, start + 10_000, 0, start - 1000, flags))
        receive, outcome, winning = at, at + 50_000, first
        ack = offset + at + 200_000
        if degraded and seq == 500:
            winning = first + 2_020_000
            phone.append(row(21, winning + 10_000, seq, winning, queued, 3, 0, winning + 10_000, 0, winning - 1000, flags))
            receive, outcome = at + 12_000_000, at + 12_050_000
            ack = offset + at + 12_200_000
        if degraded and seq == 501:
            host.append(row(3, at, seq, 500, 1))
            outcome = at + 4_100_000
            ack = offset + at + 4_200_000
        host.append(row(1, receive, seq, event, callback, winning, at - 1_000_000, offset + at - 900_000, flags, 1, 1))
        host.append(row(2, outcome, seq, max(receive + 20_000, outcome - 30_000), 0))
        host.append(row(13, max(receive + 60_000, outcome + 10_000), seq))
        host.append(row(6, outcome + 20_000, seq, 10_000, 1, 0, outcome + 10_000))
        phone.append(row(22, ack, seq, 1, ack - 8_000_000, 1, queued, 64, outcome + 10_000))
        phone.append(row(23, ack, seq, queued, event, callback, flags, first - 1000, 3 if degraded and seq == 500 else 2, 1, seq))
    if degraded:
        at, seq = 8_008_000_000, 1001
        phone += [row(20, offset + at - 800_000, seq, offset + at - 1_000_000, offset + at - 900_000, 1, 1, 64),
                  row(24, offset + at + 64_000_000, seq, offset + at - 800_000, offset + at - 1_000_000, offset + at - 900_000, 1, 0, 3, 1, 1000),
                  row(29, offset + at + 64_000_000, seq, 2, offset + at - 8_000_000, 1, offset + at - 800_000, 64),
                  row(26, offset + at + 64_000_001, seq, 1, 1)]
        host += [row(1, at, seq, offset + at - 1_000_000, offset + at - 900_000, offset + at - 700_000, at - 1_000_000, offset + at - 900_000, 1, 1, 1),
                 row(8, at + 8_000_000, seq, at + 20_000, 300, 123),
                 row(5, at + 8_100_000, seq, 1), row(11, at + 8_200_000, 0, 100_000, 0)]
        # Fresh authenticated session CANCEL and reconstructed held snapshot.
        fresh = at + 80_000_000
        host += [row(10, fresh, session=8),
                 row(1, fresh + 100_000, 0, offset + fresh, offset + fresh, offset + fresh, 0, 0, 8, 4, 0, session=8),
                 row(2, fresh + 150_000, 0, fresh + 110_000, session=8),
                 row(1, fresh + 8_100_000, 1, offset + fresh + 8_000_000, offset + fresh + 8_000_000, offset + fresh + 8_000_000, 0, 0, 4, 5, 1, session=8),
                 row(2, fresh + 8_150_000, 1, fresh + 8_110_000, session=8)]
        phone += [row(20, offset + fresh, 0, offset + fresh, offset + fresh, 8, 1, 64, session=8),
                  row(23, offset + fresh + 300_000, 0, offset + fresh, offset + fresh, offset + fresh, 8, offset + fresh, 2, 1, 0, session=8)]
    return sorted(host, key=lambda r: r[1]), sorted(phone, key=lambda r: r[1])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--java-home", type=Path, default=os.environ.get("JAVA_HOME"))
    args = parser.parse_args()
    scratch = ROOT / "build/diagnostic-examples"
    destination = ROOT / "docs/diagnostic-examples"
    scratch.mkdir(parents=True, exist_ok=True)
    destination.mkdir(parents=True, exist_ok=True)
    javac = str(args.java_home / "bin/javac") if args.java_home else "javac"
    java = str(args.java_home / "bin/java") if args.java_home else "java"
    subprocess.run([javac, "-d", str(scratch), str(ROOT / "android-app/app/src/main/java/dev/holodori/trackpad/DiagnosticRecorder.java"), str(ROOT / "tools/DiagnosticExamples.java")], check=True)
    for name, degraded in [("healthy", False), ("degraded", True)]:
        host, phone = scenario(degraded)
        for source, events in [("host", host), ("phone", phone)]:
            (scratch / f"{name}-{source}.csv").write_text("".join(",".join(map(str, r)) + "\n" for r in events), encoding="utf-8")
        subprocess.run(["cargo", "run", "--quiet", "--example", "diagnostic_fixture", "--", str(scratch / f"{name}-host.csv"), str(destination / f"{name}-host.txt")], cwd=ROOT / "native-host", check=True)
        # Production report names are unique. Fixtures deliberately reuse two
        # names; Windows renameTo cannot replace their prior generated outputs.
        for suffix in ("txt", "csv"):
            (destination / f"{name}-android.{suffix}").unlink(missing_ok=True)
        subprocess.run([java, "-cp", str(scratch), "dev.holodori.trackpad.DiagnosticExamples", str(scratch / f"{name}-phone.csv"), str(destination), f"{name}-android"], check=True)
        host_path = destination / f"{name}-host.json"
        phone_reports = {Path(filename).name: contents for filename, contents in android_files([destination / f"{name}-android.csv"]).items()}
        report = join_report([(host_path.name, json.loads(host_path.read_text()))], phone_reports)
        (destination / f"{name}-correlated.md").write_text(markdown(report), encoding="utf-8")


if __name__ == "__main__":
    main()
