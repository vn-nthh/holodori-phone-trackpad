package dev.holodori.trackpad;

import android.app.ActivityManager;
import android.content.Context;
import android.os.Build;
import android.os.PowerManager;
import android.os.Process;
import android.os.SystemClock;
import android.util.Log;

import java.io.File;
import java.io.IOException;
import java.util.Arrays;
import java.util.Locale;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLongArray;

/** Background analysis for one Start..Stop run. Environment sampling and report
 * I/O happen here, never on input threads; the report waits for every producer.
 */
final class DiagnosticWorker {
    interface BindingSource {
        V5NetworkBinding current();
    }

    private static final String TAG = "HolodoriUDP5";
    private static final long ENVIRONMENT_INTERVAL_NANOS = 2_000_000_000L;
    private static final int MAX_REPORT_FILES = 16;

    final DiagnosticRecorder recorder = new DiagnosticRecorder();
    /** Discarded control datagrams: bad tag, packet replay, malformed. */
    final AtomicLongArray controlDrops = new AtomicLongArray(3);
    private final AtomicInteger producers = new AtomicInteger();
    private final Context context;
    private final V5Protocol.TransportKind transport;
    private final BindingSource binding;
    private Thread thread;
    private volatile boolean stopping;

    DiagnosticWorker(Context context, V5Protocol.TransportKind transport, BindingSource binding) {
        this.context = context;
        this.transport = transport;
        this.binding = binding;
    }

    static File directory(Context context) {
        return new File(context.getFilesDir(), "diagnostics");
    }

    static boolean isReport(String name) {
        return name.startsWith("android-") && (name.endsWith(".txt") || name.endsWith(".csv"));
    }

    /** Wrap a transport thread so the report is written only after its last record. */
    Runnable producer(Runnable work) {
        producers.incrementAndGet();
        return () -> {
            try { work.run(); }
            finally { producers.decrementAndGet(); }
        };
    }

    void start() {
        if (thread != null) return;
        stopping = false;
        thread = new Thread(this::run, "V5 diagnostic analysis");
        thread.start();
    }

    void stop() {
        stopping = true;
    }

    private void run() {
        Process.setThreadPriority(Process.THREAD_PRIORITY_BACKGROUND);
        PowerManager power = (PowerManager) context.getSystemService(Context.POWER_SERVICE);
        ActivityManager.RunningAppProcessInfo process = new ActivityManager.RunningAppProcessInfo();
        long[] environment = new long[DiagnosticRecorder.WIDTH];
        long[] dropTotals = new long[controlDrops.length()];
        long nextEnvironment = 0;
        String selectedInterface = "unavailable";
        int trafficClass = -1;
        try {
            for (;;) {
                recorder.drain();
                long now = System.nanoTime();
                if (now >= nextEnvironment) {
                    nextEnvironment = now + ENVIRONMENT_INTERVAL_NANOS;
                    V5NetworkBinding current = binding.current();
                    Arrays.fill(environment, -1L);
                    environment[0] = DiagnosticRecorder.ENVIRONMENT;
                    environment[1] = now;
                    environment[2] = 0; environment[3] = 0;
                    try {
                        if (power != null) {
                            environment[4] = Build.VERSION.SDK_INT >= 29 ? power.getCurrentThermalStatus() : -1;
                            environment[5] = power.isPowerSaveMode() ? 1 : 0;
                            environment[6] = power.isInteractive() ? 1 : 0;
                        }
                        ActivityManager.getMyMemoryState(process);
                        environment[7] = process.importance
                                <= ActivityManager.RunningAppProcessInfo.IMPORTANCE_FOREGROUND ? 1 : 0;
                        if (current != null) {
                            environment[8] = current.diagnosticWifiLockHeld();
                            selectedInterface = current.diagnosticInterface();
                            trafficClass = current.socket().getTrafficClass();
                            current.diagnosticSignal(environment);
                        }
                        // Clock-domain/suspend evidence; no subtraction from event ages.
                        environment[11] = SystemClock.elapsedRealtimeNanos() - now;
                    } catch (RuntimeException | IOException unavailable) {
                        // Numeric -1 fields retain unavailable status; never fabricate healthy values.
                    }
                    recorder.analyze(environment);
                    for (int i = 0; i < dropTotals.length; i++) {
                        long total = controlDrops.get(i);
                        if (total == dropTotals[i]) continue;
                        Arrays.fill(environment, 0);
                        environment[0] = DiagnosticRecorder.CONTROL_DROP; environment[1] = now;
                        environment[4] = i; environment[5] = total - dropTotals[i];
                        recorder.analyze(environment);
                        dropTotals[i] = total;
                    }
                }
                if (stopping && producers.get() == 0) break;
                Thread.sleep(4);
            }
            recorder.drain();
            writeReport(selectedInterface, trafficClass);
        } catch (IOException error) {
            Log.e(TAG, "Diagnostic report could not be saved after Stop", error);
        } catch (InterruptedException ignored) {
            Thread.currentThread().interrupt();
        }
    }

    private void writeReport(String selectedInterface, int trafficClass) throws IOException {
        File directory = directory(context);
        recorder.write(directory, "android-" + System.currentTimeMillis(),
                "transport=" + transport.name().toLowerCase(Locale.ROOT)
                + " android_sdk=" + Build.VERSION.SDK_INT
                + " motion_event_precision_ns=" + (Build.VERSION.SDK_INT >= 34 ? 1 : 1_000_000)
                + " driver_low_latency=unverified ap_wmm=unverified"
                + " interface=" + selectedInterface
                + " socket_traffic_class_readback=" + trafficClass
                + " qos_request=none control_bad_tag=" + controlDrops.get(0)
                + " control_packet_replay=" + controlDrops.get(1)
                + " control_malformed=" + controlDrops.get(2));
        // Bound disk storage after Stop only; never remove unrelated files.
        File[] reports = directory.listFiles((dir, name) -> isReport(name));
        if (reports == null) return;
        Arrays.sort(reports, (left, right) -> left.getName().compareTo(right.getName()));
        for (int i = 0; i < reports.length - MAX_REPORT_FILES; i++) {
            if (!reports[i].delete()) Log.w(TAG, "Could not expire old diagnostic report");
        }
    }
}
