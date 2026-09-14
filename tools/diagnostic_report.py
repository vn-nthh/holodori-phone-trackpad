"""Join stopped Doritrack schema-2 reports. Python standard library only.

No connection to a running host/phone, no clock-origin subtraction, no game access.
Session IDs plus logical sequences correlate evidence; phone attempt timestamps
identify the copy that first reached the host. Absence from retained evidence is
never equated with packet loss. Inputs and output sizes are bounded.
"""
import argparse
import csv
import io
import json
from pathlib import Path
import re
import zipfile

MASK = (1 << 64) - 1
MAX_BYTES = 64 * 1024 * 1024
MAX_EVENTS = 300_000
BUDGET = 8_333_333
ANDROID_REASONS = {
    1: "Android dispatch timestamp age", 2: "V5 repair attempted (forward or ACK loss possible)",
    4: "Android queue/send/repair deadline budget", 8: "Android seal/socket send failed",
    16: "sender event-to-ACK budget (return path included)", 32: "invalid future ACK",
    64: "frame discarded/not queued or watchdog expired", 128: "authenticated session failed",
    256: "control datagram discarded", 512: "environment constraint observed; causality unproven",
}


def key(row):
    return row[2] & MASK, row[3] & MASK


def read_text(path):
    if path.stat().st_size > MAX_BYTES:
        raise ValueError(f"Report exceeds {MAX_BYTES} bytes: {path}")
    return path.read_text(encoding="utf-8-sig")


def android_files(paths):
    files = {}
    total = 0
    for path in paths:
        if path.suffix.lower() == ".zip":
            with zipfile.ZipFile(path) as archive:
                for member in archive.infolist():
                    if not member.filename.endswith((".csv", ".txt")):
                        continue
                    total += member.file_size
                    if total > MAX_BYTES:
                        raise ValueError("Android reports exceed decompressed size bound")
                    # Read in memory; never extract ZIP paths to the filesystem.
                    files[f"{path.name}/{member.filename}"] = archive.read(member).decode("utf-8-sig")
        else:
            candidates = [path] if path.suffix == ".txt" else [path, path.with_suffix(".txt")]
            for candidate in candidates:
                if candidate.exists():
                    content = read_text(candidate)
                    total += len(content.encode("utf-8"))
                    if total > MAX_BYTES:
                        raise ValueError("Android reports exceed size bound")
                    files[str(candidate)] = content
    return files


def parse_android(files):
    events, summaries, incidents = [], [], []
    for name, text in files.items():
        if name.endswith(".csv"):
            lines = [line for line in text.splitlines() if line and not line.startswith("#")]
            reader = csv.reader(lines)
            if next(reader, []) != ["kind", "at_ns", "session", "sequence", "a", "b", "c", "d", "e", "f", "g", "h"]:
                raise ValueError(f"Unknown Android event schema: {name}")
            for row in reader:
                if len(row) != 12:
                    raise ValueError(f"Malformed numeric event: {name}")
                values = tuple(int(value) for value in row)
                events.append(values)
                if len(events) > MAX_EVENTS:
                    raise ValueError("Too many retained Android records")
        elif name.endswith(".txt"):
            if "schema=2" not in text:
                raise ValueError(f"Unknown Android summary schema: {name}")
            counts = {int(k): int(v) for k, v in re.findall(r"^event_(\d+)=(\d+)$", text, re.M)}
            quality = {k: int(v) for k, v in re.findall(r"\b(dropped|omitted_incident_triggers|evidence_omitted|writer_generations_unavailable)=(\d+)", text)}
            summaries.append({"file": name, "counts_by_event_kind": counts, "quality": quality, "text": text})
            for line in text.splitlines():
                if not re.fullmatch(r"-?\d+(,-?\d+){11}", line):
                    continue
                r = [int(v) for v in line.split(",")]
                incidents.append({"source": "android", "id": f"{name}:{len(incidents) + 1}",
                                  "at_phone_ns": r[0], "last_anomaly_phone_ns": r[1],
                                  "session": r[2] & MASK, "first_sequence": r[3] & MASK,
                                  "last_sequence": r[4] & MASK,
                                  "causes": [label for bit, label in ANDROID_REASONS.items() if r[5] & bit],
                                  "recovery_to_later_ack_ns": r[6] - r[0] if r[6] else None,
                                  "evidence_omitted": r[11], "post_context_complete": bool(r[10])})
    # Shared pre/post contexts contain repeated rows. Do not double count them.
    return sorted(set(events), key=lambda r: r[1]), summaries, incidents


def duration(later, earlier):
    return later - earlier if earlier > 0 and later >= earlier else None


def frame_report(frame_key, phone, host):
    p = sorted(phone, key=lambda r: r[1])
    h = sorted(host, key=lambda r: r[1])
    queued = next((r for r in p if r[0] == 20), None)
    attempts = [r for r in p if r[0] == 21]
    retired = next((r for r in p if r[0] == 23), None)
    discarded = next((r for r in p if r[0] == 24), None)
    receive = next((r for r in h if r[0] == 1), None)
    accepted = next((r for r in h if r[0] == 2), None)
    failed = next((r for r in h if r[0] == 8), None)
    matching_attempts = [r for r in attempts if receive and r[4] == receive[6]]
    winner = matching_attempts[0] if len(matching_attempts) == 1 else None
    findings = []
    if failed:
        code = failed[6] if failed[6] < 1 << 63 else failed[6] - (1 << 64)
        findings.append(f"host OS sink failed: error={code}, retries={failed[5]}")
    if any(r[0] == 3 for r in h):
        findings.append("host received a future unique frame behind an ordering hole")
    if any(r[0] == 4 for r in h):
        findings.append("host discarded/rejected an observed frame")
    if discarded and accepted:
        findings.append("host committed, but sender abandoned before observing its ACK; input was not lost at host")
    elif discarded:
        findings.append("sender abandoned frame; host outcome requires retained host evidence")
    if winner and winner[6] >= 3:
        findings.append("first host delivery matched a repair attempt")
    elif attempts and any(r[6] >= 3 for r in attempts):
        findings.append("repair occurred; ACK loss or forward loss remains possible")
    if queued and duration(queued[5], queued[4]) is not None and queued[5] - queued[4] > BUDGET:
        findings.append("Android callback received an already-aged current/historical sample")
    if retired and attempts and retired[1] - attempts[0][4] > BUDGET and accepted:
        findings.append("sender completion tail includes ACK generation/return-path/sender scheduling; compare host outcome")
    flags = queued[6] if queued else receive[9] if receive else retired[7] if retired else None
    outcome = "OS accepted" if accepted else "OS sink failed" if failed else "sender ACK confirms prior OS acceptance" if retired else "sender discarded; host outcome unknown" if discarded else "unknown (bounded evidence)"
    return {"session": f"{frame_key[0]:016x}", "sequence": frame_key[1], "sample_flags": flags,
            "outcome": outcome, "findings": findings,
            "winning_copy": ("first" if winner[6] == 1 else "redundant" if winner[6] == 2 else "repair") if winner else "ambiguous timestamp" if matching_attempts else "unavailable",
            "timing_ns": {
                "phone_event": queued[4] if queued else receive[4] if receive else None,
                "phone_callback": queued[5] if queued else receive[5] if receive else None,
                "phone_first_attempt": attempts[0][4] if attempts else None,
                "phone_winning_attempt": winner[4] if winner else None,
                "host_receive": receive[1] if receive else None,
                "host_sink_ready": (accepted or failed)[4] if accepted or failed else None,
                "host_sink_outcome": (accepted or failed)[1] if accepted or failed else None,
                "phone_cumulative_ack": retired[1] if retired else None,
                "phone_discard": discarded[1] if discarded else None,
                "host_receive_to_ready": duration((accepted or failed)[4], receive[1]) if receive and (accepted or failed) else None,
                "host_sink_attempts": duration((accepted or failed)[1], (accepted or failed)[4]) if accepted or failed else None,
                "phone_event_to_ack_upper_bound": duration(retired[1], retired[5]) if retired else None,
                "phone_first_attempt_to_ack": duration(retired[1], attempts[0][4]) if retired and attempts else None},
            "phone_events": p, "host_events": h}


def join_report(host_reports, files):
    phone_events, phone_summaries, candidates = parse_android(files)
    host_events, host_summaries = [], []
    for name, report in host_reports:
        analysis = report.get("analysis", {})
        if analysis.get("schema") != 2 or report.get("source") != "host":
            raise ValueError(f"Unknown host schema: {name}")
        host_summaries.append({"file": name, "transport": report.get("transport"), "environment": report.get("environment"),
                               "counts": analysis["counts"], "series": analysis["series"],
                               "diagnostic_records_dropped": analysis["diagnostic_records_dropped"],
                               "incidents_omitted": analysis["incidents_omitted"]})
        host_events.extend(report.get("recent_context", []))
        for incident in analysis["incidents"]:
            host_events.extend(incident["evidence"])
            candidates.append({"source": "host", "id": f"{name}:{incident['id']}",
                               "at_host_ns": incident["started_ns"], "last_anomaly_host_ns": incident["last_anomaly_ns"],
                               "session": incident["first_frame"][0], "first_sequence": incident["first_frame"][1],
                               "last_sequence": incident["last_frame"][1], "causes": incident["causes"],
                               "recovery_to_os_commit_ns": incident["recovery_ns"], "evidence_omitted": incident["evidence_omitted"],
                               "fresh_session_commit_ns": incident.get("fresh_session_commit_ns"),
                               "reconstructed_snapshot_ns": incident.get("reconstructed_snapshot_ns"),
                               "affected_frame_keys": incident.get("affected_frame_keys", []),
                               "post_context_complete": incident["post_context_complete"]})
    if len(host_events) + len(phone_events) > MAX_EVENTS:
        raise ValueError("Too many retained records to join")
    p_by_key, h_by_key = {}, {}
    for rows, table in [(phone_events, p_by_key), (sorted(set(map(tuple, host_events)), key=lambda r: r[1]), h_by_key)]:
        for r in rows:
            if len(r) != 12:
                raise ValueError("Malformed retained record")
            if r[2] == 0 or (r[3] & MASK) == MASK:
                continue
            if r[0] in (1, 2, 3, 4, 8, 13, 20, 21, 23, 24, 25):
                table.setdefault(key(r), []).append(r)
    frames = [frame_report(k, p_by_key.get(k, []), h_by_key.get(k, [])) for k in sorted(p_by_key.keys() | h_by_key.keys())]
    # Join cross-device incidents only when affected sequence ranges overlap in
    # the same session. Context overlap alone is not proof of one incident.
    clusters = []
    for candidate in candidates:
        first, last = sorted([candidate["first_sequence"], candidate["last_sequence"]])
        matches = [c for c in clusters if c["session"] == candidate["session"] != 0
                   and first <= c["last_sequence"] and last >= c["first_sequence"]]
        if matches:
            cluster = matches[0]
            for other in matches[1:]:
                cluster["sources"].extend(other["sources"])
                cluster["first_sequence"] = min(cluster["first_sequence"], other["first_sequence"])
                cluster["last_sequence"] = max(cluster["last_sequence"], other["last_sequence"])
                clusters.remove(other)
        else:
            cluster = {"session": candidate["session"], "first_sequence": first, "last_sequence": last, "sources": []}
            clusters.append(cluster)
        cluster["sources"].append(candidate)
        cluster["first_sequence"] = min(cluster["first_sequence"], first)
        cluster["last_sequence"] = max(cluster["last_sequence"], last)
    for i, cluster in enumerate(clusters):
        cluster["id"] = i + 1
        cluster["causes"] = sorted({cause for source in cluster["sources"] for cause in source["causes"]})
        cluster["frames"] = [f for f in frames if int(f["session"], 16) == cluster["session"]
                             and cluster["first_sequence"] <= f["sequence"] <= cluster["last_sequence"]]
    return {"schema": 2, "session_health": {"host": host_summaries, "android": phone_summaries},
            "incidents": clusters, "retained_frames": frames,
            "limitations": ["Counts/histograms are whole-run observations; retained frames are bounded forensic context, not the population.",
                            "No missing event proves packet loss. Distinguish absent evidence, sender discard and observed sink failure.",
                            "Phone and host timestamps retain independent clock origins. Match session/sequence and winning-attempt timestamp; do not subtract origins.",
                            "Transport bounds depend on stated drift and timestamp precision assumptions; environmental observations do not prove a root cause.",
                            "OS acceptance does not establish game recognition or physical input-to-display latency."]}


def markdown(report):
    out = ["# Doritrack correlated diagnostics", "", f"Clustered incidents: {len(report['incidents'])}. Retained frame records: {len(report['retained_frames'])}.", "",
           "## Session health", "", "| Source | Unique/queued | OS accepted/ACK retired | Failed/discarded | Duplicates/repairs | Diagnostic records lost |",
           "|---|---:|---:|---:|---:|---:|"]
    for summary in report["session_health"]["host"]:
        c = summary["counts"]
        out.append(f"| Host ({summary['transport']}) | {c.get('unique_logical_frames_observed', 0)} | {c.get('os_accepted', 0)} | {c.get('os_failed', 0)} failed; {c.get('observed_uncommitted_at_boundary', 0)} abandoned | {c.get('logical_duplicate_datagrams', 0)} duplicates | {summary['diagnostic_records_dropped']} |")
    for summary in report["session_health"]["android"]:
        c = summary["counts_by_event_kind"]
        out.append(f"| Android | {c.get(20, 0)} | {c.get(23, 0)} | {c.get(24, 0)} discarded; {c.get(25, 0)} not queued | {c.get(0, 0)} repairs | {summary['quality'].get('dropped', 'unavailable')} |")
    if not report["session_health"]["android"]:
        out += ["", "Android report unavailable: frames never received by the host and sender ACK progress cannot be assessed."]
    out.append("")
    for limitation in report["limitations"]:
        out.append(f"- {limitation}")
    for incident in report["incidents"]:
        out += ["", f"## Incident {incident['id']} — session {incident['session']:016x}, sequences {incident['first_sequence']}–{incident['last_sequence']}", "", "; ".join(incident["causes"]), ""]
        for source in incident["sources"]:
            out.append(f"- {source['source']}: {json.dumps({k: v for k, v in source.items() if k not in ('causes', 'source')}, ensure_ascii=False)}")
        out += ["", "| Sequence | Outcome | Winning copy | Evidence |", "|---|---|---|---|"]
        for frame in incident["frames"]:
            out.append(f"| {frame['sequence']} | {frame['outcome']} | {frame['winning_copy']} | {'; '.join(frame['findings'])} |")
    return "\n".join(out) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", type=Path, nargs="*", default=[])
    parser.add_argument("--android", type=Path, nargs="*", default=[])
    parser.add_argument("--output", type=Path, required=True, help="JSON output; readable .md written alongside")
    args = parser.parse_args()
    if not args.host and not args.android:
        parser.error("supply at least one host or Android report")
    report = join_report([(str(p), json.loads(read_text(p))) for p in args.host], android_files(args.android))
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    args.output.with_suffix(".md").write_text(markdown(report), encoding="utf-8")


if __name__ == "__main__":
    main()
