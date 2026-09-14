package dev.holodori.trackpad;

import java.io.File;
import java.io.IOException;
import java.io.PrintWriter;
import java.util.concurrent.atomic.AtomicBoolean;

/** Numeric flight recorder. No Android APIs, logging, allocation or new locks on publication.
 * Queue-domain producers already hold V5Transport.queueLock; writer/control have separate rings.
 * Only the diagnostic worker reads rings or analyzes events. Report I/O is after producer exit.
 */
final class DiagnosticRecorder {
    static final int WIDTH = 12, CAPACITY = 8192, CONTEXT = 128;
    static final int MAX_INCIDENTS = 256, MAX_EVIDENCE = 32768, PER_INCIDENT = 512;
    static final long BUDGET = 8_333_333L, QUIET = 32_000_000L;
    static final int QUEUED = 20, ATTEMPT = 21, ACK = 22, RETIRED = 23,
            DISCARDED = 24, NOT_QUEUED = 25, BOUNDARY = 26, ENVIRONMENT = 27,
            CONTROL_DROP = 28, WATCHDOG = 29;

    static final class Ring {
        final long[] data = new long[CAPACITY * WIDTH];
        volatile long read, write, dropped;
        long firstDropNs, lastDropNs;
        // Claimed once per writer lifetime, never per event. If an old socket
        // writer overlaps reconnection, new writer diagnostics are explicitly absent.
        final AtomicBoolean claimed = new AtomicBoolean();

        void offer(long kind, long at, long session, long sequence, long a, long b,
                   long c, long d, long e, long f, long g, long h) {
            long next = write;
            if (next - read == CAPACITY) {
                if (dropped == 0) firstDropNs = at;
                lastDropNs = at;
                dropped++; return;
            }
            int i = (int) (next % CAPACITY) * WIDTH;
            data[i] = kind; data[i + 1] = at; data[i + 2] = session; data[i + 3] = sequence;
            data[i + 4] = a; data[i + 5] = b; data[i + 6] = c; data[i + 7] = d;
            data[i + 8] = e; data[i + 9] = f; data[i + 10] = g; data[i + 11] = h;
            write = next + 1; // volatile publication after every field
        }

        long nextTime() { return read == write ? Long.MAX_VALUE : data[(int) (read % CAPACITY) * WIDTH + 1]; }
        boolean poll(long[] destination) {
            long next = read;
            if (next == write) return false;
            System.arraycopy(data, (int) (next % CAPACITY) * WIDTH, destination, 0, WIDTH);
            read = next + 1;
            return true;
        }
    }

    final Ring queue = new Ring(), writer = new Ring();
    final long[] counts = new long[40];
    final long[][] histograms = new long[10][2049];
    final long[][] statistics = new long[10][5]; // n, sum, max, strict >budget, invalid
    final long[] history = new long[CONTEXT * WIDTH];
    final long[] evidence = new long[MAX_EVIDENCE * WIDTH];
    final long[] incident = new long[MAX_INCIDENTS * WIDTH];
    final long[] row = new long[WIDTH];
    final long[] latestEnvironment = new long[WIDTH];
    int historySize, historyHead, evidenceSize, incidents, active = -1;
    long omittedIncidents, omittedEvidence, observed, lastAt;
    volatile long writerCoverageUnavailable;
    long pendingHighWater, oldestPendingMaxNs, repairLatenessMaxNs;
    private long lastBadSession, lastBadSequence = -1;
    private int recovering = -1;

    void drain() {
        // Merge currently published records. A producer may publish a timestamp late;
        // raw timestamps survive and the offline join sorts them. No causal claim from order alone.
        for (;;) {
            Ring next = queue;
            if (writer.nextTime() < next.nextTime()) next = writer;
            if (!next.poll(row)) return;
            analyze(row);
        }
    }

    void sample(int metric, long later, long earlier) {
        if (earlier <= 0 || later < earlier) { statistics[metric][4]++; return; }
        long value = later - earlier;
        int exponent = 63 - Long.numberOfLeadingZeros(Math.max(1, value));
        int shift = Math.max(0, exponent - 5);
        int index = value < 32 ? (int) value : (exponent - 4) * 32 + (int) (value >> shift) - 32;
        histograms[metric][index]++;
        long[] stats = statistics[metric];
        stats[0]++;
        stats[1] = stats[1] > Long.MAX_VALUE - value ? Long.MAX_VALUE : stats[1] + value;
        stats[2] = Math.max(stats[2], value);
        if (value > BUDGET) stats[3]++;
    }

    void analyze(long[] r) {
        long at = r[1];
        observed++;
        lastAt = Math.max(lastAt, at);
        if (active >= 0 && at - incident[active * WIDTH + 1] > QUIET) {
            incident[active * WIDTH + 10] = 1;
            active = -1;
        }
        int kind = (int) r[0];
        if (kind >= 0 && kind < counts.length) counts[kind]++;
        long reasons = 0;
        if (kind == QUEUED) {
            pendingHighWater = Math.max(pendingHighWater, r[7]);
            // a event, b callback, c flags, d pending depth, e host window.
            if ((r[6] & 1) != 0) {
                sample((r[6] & 2) != 0 ? 1 : 0, r[5], r[4]);
                if (r[5] - r[4] > BUDGET) reasons |= 1;
            }
        } else if (kind == ATTEMPT) {
            // a encryption start, b queue time, c attempt ordinal, d seal+send result,
            // e send-return time, f repair deadline lateness, g selection time, h event flags.
            sample(2, r[4], r[5]);
            sample(3, r[8], r[4]);
            repairLatenessMaxNs = Math.max(repairLatenessMaxNs, r[9]);
            if (r[6] >= 3) { counts[0]++; reasons |= 2; }
            if (r[4] - r[5] > BUDGET || r[8] - r[4] > BUDGET || r[9] > BUDGET) reasons |= 4;
            if (r[7] != 0) reasons |= 8;
        } else if (kind == RETIRED) {
            // a queued, b event, c callback, d flags, e selection-first, f attempts,
            // g queue depth before retirement, h cumulative ACK id.
            sample(4, at, r[4]);
            if ((r[7] & 1) != 0) {
                sample((r[7] & 2) != 0 ? 6 : 5, at, r[5]);
                if (at - r[5] > BUDGET) reasons |= 16;
            }
        } else if (kind == ACK) {
            if (r[7] > 0) oldestPendingMaxNs = Math.max(oldestPendingMaxNs, at - r[7]);
            // a progress 0/1/2(unsent-invalid), b prior progress time, c pending depth,
            // d oldest queue time, e receive window, f echoed host send ns.
            if (r[4] == 1 && r[6] > 0) sample(7, at, r[5]);
            if (r[4] == 0) counts[1]++;
            if (r[4] == 2) { counts[2]++; reasons |= 32; }
        } else if (kind == DISCARDED || kind == NOT_QUEUED || kind == WATCHDOG) {
            reasons |= 64;
        } else if (kind == BOUNDARY && r[4] != 0) {
            reasons |= 128;
        } else if (kind == CONTROL_DROP) {
            reasons |= 256;
        } else if (kind == ENVIRONMENT) {
            System.arraycopy(r, 0, latestEnvironment, 0, WIDTH);
            // Android API unavailable values are -1; never treat unavailable as verified.
            if (r[4] >= 3 || r[5] == 1 || r[6] == 0 || r[7] == 0 || r[8] == 0) reasons |= 512;
        }
        if (reasons != 0) trigger(r, reasons);
        if ((kind == BOUNDARY && r[4] != 0) || kind == WATCHDOG) recovering = active;
        if (kind == RETIRED) {
            if (active >= 0 && incident[active * WIDTH + 6] == 0) incident[active * WIDTH + 6] = at;
            if (recovering >= 0) {
                incident[recovering * WIDTH + 6] = at;
                if (active != recovering && evidenceSize < MAX_EVIDENCE) {
                    // A recovery after the quiet window is retained in recent context
                    // and its timestamp remains on the original incident summary.
                    System.arraycopy(r, 0, evidence, evidenceSize++ * WIDTH, WIDTH);
                }
                recovering = -1;
            }
        }
        retain(r);
        System.arraycopy(r, 0, history, historyHead * WIDTH, WIDTH);
        historyHead = (historyHead + 1) % CONTEXT;
        historySize = Math.min(CONTEXT, historySize + 1);
    }

    private void trigger(long[] r, long reasons) {
        if (active < 0) {
            if (incidents == MAX_INCIDENTS) { omittedIncidents++; return; }
            active = incidents++;
            int i = active * WIDTH;
            incident[i] = r[1]; // first anomaly
            incident[i + 2] = r[2];
            incident[i + 3] = r[3];
            incident[i + 8] = evidenceSize;
            if (latestEnvironment[0] == ENVIRONMENT && evidenceSize < MAX_EVIDENCE) {
                System.arraycopy(latestEnvironment, 0, evidence, evidenceSize++ * WIDTH, WIDTH);
                incident[i + 9]++;
            }
            // Pre-context is bounded by record count, not a guaranteed time window.
            for (int n = 0; n < historySize; n++) {
                int src = ((historyHead - historySize + CONTEXT + n) % CONTEXT) * WIDTH;
                if (evidenceSize == MAX_EVIDENCE) { omittedEvidence++; incident[i + 11]++; continue; }
                System.arraycopy(history, src, evidence, evidenceSize++ * WIDTH, WIDTH);
                incident[i + 9]++;
            }
        }
        int i = active * WIDTH;
        incident[i + 1] = Math.max(incident[i + 1], r[1]); // last anomaly
        incident[i + 4] = r[3]; // last affected sequence
        incident[i + 5] |= reasons;
        incident[i + 6] = 0; // require a later advancing ACK for recovery
        if (lastBadSession != r[2] || lastBadSequence != r[3]) incident[i + 7]++;
        lastBadSession = r[2]; lastBadSequence = r[3];
    }

    private void retain(long[] r) {
        if (active < 0) return;
        int i = active * WIDTH;
        if (evidenceSize == MAX_EVIDENCE || incident[i + 9] == PER_INCIDENT) {
            omittedEvidence++; incident[i + 11]++; return;
        }
        System.arraycopy(r, 0, evidence, evidenceSize++ * WIDTH, WIDTH);
        incident[i + 9]++;
    }

    long dropped() { return queue.dropped + writer.dropped; }

    void write(File directory, String name, String environment) throws IOException {
        if (!directory.isDirectory() && !directory.mkdirs()) throw new IOException("Cannot create diagnostic directory");
        String[] metrics = {"current_dispatch_ns", "historical_dispatch_ns", "queue_to_attempt_ns",
                "seal_and_socket_send_ns", "queued_to_cumulative_ack_ns", "current_event_to_ack_ns",
                "historical_event_to_ack_ns", "active_ack_progress_interval_ns", "unused8", "unused9"};
        File summary = new File(directory, name + ".txt.tmp");
        File trace = new File(directory, name + ".csv.tmp");
        try (PrintWriter out = new PrintWriter(summary, "UTF-8")) {
            out.println("Doritrack Android diagnostic report schema=2 protocol=5");
            out.println(environment);
            out.println("observed=" + observed + " dropped=" + dropped() + " incidents=" + incidents
                    + " omitted_incident_triggers=" + omittedIncidents + " evidence_omitted=" + omittedEvidence
                    + " writer_generations_unavailable=" + writerCoverageUnavailable);
            out.println("ACK completion is a sender-clock upper bound on OS acceptance, including return-path delay; not one-way or game-observed latency.");
            out.println("pending_depth_high_water=" + pendingHighWater + " oldest_pending_at_ack_max_ns=" + oldestPendingMaxNs
                    + " repair_deadline_lateness_max_ns=" + repairLatenessMaxNs);
            out.println("queue_diagnostic_drop_span_ns=" + queue.firstDropNs + "," + queue.lastDropNs
                    + " writer_diagnostic_drop_span_ns=" + writer.firstDropNs + "," + writer.lastDropNs);
            out.println("Attempt is seal/send invocation; socket return is local submission, not NIC transmission. Repairs do not prove forward loss; ACK loss can also cause repair.");
            out.println("Counters cover the full observed run. Percentile ranks use log histogram intervals; p99 requires n>=1000, p99.9 n>=10000. Samples are correlated; no confidence claim.");
            for (int i = 0; i < counts.length; i++) out.println("event_" + i + "=" + counts[i]);
            for (int i = 0; i < 8; i++) {
                long[] s = statistics[i];
                out.println(metrics[i] + " n=" + s[0] + " sum_ns=" + s[1] + " max_ns=" + s[2]
                        + " sum_saturated=" + (s[1] == Long.MAX_VALUE)
                        + " over_budget=" + s[3] + " invalid=" + s[4]
                        + " p50=" + quantile(i, 50, 100) + " p90=" + quantile(i, 90, 100)
                        + " p99=" + (s[0] < 1000 ? "insufficient" : quantile(i, 99, 100))
                        + " p99.9=" + (s[0] < 10000 ? "insufficient" : quantile(i, 999, 1000)));
            }
            out.println("Incident columns: first_ns,last_anomaly_ns,session,first_seq,last_seq,reasons,first_later_ack_ns,affected_observations,evidence_offset,evidence_count,post_complete,omitted");
            for (int i = 0; i < incidents; i++) writeRow(out, incident, i * WIDTH);
            if (out.checkError()) throw new IOException("Diagnostic summary write failed");
        }
        try (PrintWriter out = new PrintWriter(trace, "UTF-8")) {
            out.println("# doritrack schema=2 source=android " + environment);
            out.println("# dropped=" + dropped() + " omitted_incidents=" + omittedIncidents + " omitted_evidence=" + omittedEvidence);
            out.println("kind,at_ns,session,sequence,a,b,c,d,e,f,g,h");
            for (int i = 0; i < evidenceSize; i++) writeRow(out, evidence, i * WIDTH);
            // Last healthy context lets a host-only failure be correlated when the phone stops.
            for (int n = 0; n < historySize; n++) writeRow(out, history, ((historyHead - historySize + CONTEXT + n) % CONTEXT) * WIDTH);
            if (out.checkError()) throw new IOException("Diagnostic evidence write failed");
        }
        if (!summary.renameTo(new File(directory, name + ".txt"))
                || !trace.renameTo(new File(directory, name + ".csv"))) throw new IOException("Could not finalize diagnostic reports");
    }

    private String quantile(int metric, int numerator, int denominator) {
        long n = statistics[metric][0];
        if (n == 0) return "unavailable";
        long rank = n / denominator * numerator + (n % denominator * numerator + denominator - 1) / denominator;
        long cumulative = 0;
        for (int i = 0; i < histograms[metric].length; i++) {
            cumulative += histograms[metric][i];
            if (cumulative >= rank) {
                long low = i < 32 ? i : (32L + i % 32) << (i / 32 - 1);
                long high = i < 32 ? i : low + (1L << (i / 32 - 1)) - 1;
                return "[" + low + "," + Math.min(statistics[metric][2], high) + "]";
            }
        }
        return "unavailable";
    }

    private static void writeRow(PrintWriter out, long[] values, int offset) {
        for (int j = 0; j < WIDTH; j++) { if (j != 0) out.print(','); out.print(values[offset + j]); }
        out.println();
    }
}
