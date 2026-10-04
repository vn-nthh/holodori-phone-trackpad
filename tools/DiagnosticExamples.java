package dev.holodori.trackpad;

import java.io.File;
import java.nio.file.Files;
import java.nio.file.Paths;

/** Offline fixtures through the same Android analyzer/formatter used in play. */
public final class DiagnosticExamples {
    public static void main(String[] args) throws Exception {
        DiagnosticRecorder recorder = new DiagnosticRecorder();
        for (String line : Files.readAllLines(Paths.get(args[0]))) {
            String[] values = line.split(",");
            long[] row = new long[12];
            for (int i = 0; i < 12; i++) row[i] = Long.parseLong(values[i]);
            recorder.analyze(row);
        }
        recorder.write(new File(args[1]), args[2], "transport=synthetic-wifi synthetic=true motion_event_precision_ns=1000000 driver_low_latency=unverified ap_wmm=unverified");
    }
}
