package dev.holodori.trackpad;

import android.content.Context;
import android.net.ConnectivityManager;
import android.net.LinkAddress;
import android.net.LinkProperties;
import android.net.Network;
import android.net.NetworkCapabilities;
import android.net.wifi.WifiManager;
import android.os.Build;
import android.util.Log;

import java.io.IOException;
import java.net.DatagramPacket;
import java.net.DatagramSocket;
import java.net.Inet4Address;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.InterfaceAddress;
import java.net.NetworkInterface;
import java.net.SocketException;
import java.util.ArrayList;
import java.util.Collections;
import java.util.Enumeration;
import java.util.HashMap;
import java.util.HashSet;
import java.util.List;
import java.util.Map;
import java.util.Objects;
import java.util.Set;

/** One explicitly selected Android USB-tether or physical Wi-Fi UDP path. */
final class V5NetworkBinding implements AutoCloseable {
    private final ConnectivityManager connectivityManager;
    private final V5Protocol.TransportKind transport;
    private final Network androidNetwork;
    private final String interfaceName;
    private final List<String> localFingerprint;
    private final List<DiscoveryPolicy.Ipv4Subnet> subnets;
    private final List<InetAddress> destinations;
    private final DatagramSocket socket;
    private final WifiManager.WifiLock wifiLock;

    private volatile InetSocketAddress peer;

    private V5NetworkBinding(
            ConnectivityManager connectivityManager,
            V5Protocol.TransportKind transport,
            Network androidNetwork,
            String interfaceName,
            List<String> localFingerprint,
            List<DiscoveryPolicy.Ipv4Subnet> subnets,
            List<InetAddress> destinations,
            DatagramSocket socket,
            WifiManager.WifiLock wifiLock
    ) {
        this.connectivityManager = connectivityManager;
        this.transport = transport;
        this.androidNetwork = androidNetwork;
        this.interfaceName = interfaceName;
        this.localFingerprint = localFingerprint;
        this.subnets = subnets;
        this.destinations = destinations;
        this.socket = socket;
        this.wifiLock = wifiLock;
    }

    static V5NetworkBinding open(Context context, V5Protocol.TransportKind transport)
            throws IOException {
        Context application = context.getApplicationContext();
        Context owner = application == null ? context : application;
        ConnectivityManager manager =
                (ConnectivityManager) owner.getSystemService(Context.CONNECTIVITY_SERVICE);
        if (manager == null) throw new IOException("Android network service is unavailable");
        return transport == V5Protocol.TransportKind.WIFI
                ? openWifi(owner, manager)
                : openUsb(manager);
    }

    DatagramSocket socket() {
        return socket;
    }

    // Called only by the diagnostic worker. Held does not establish driver support.
    int diagnosticWifiLockHeld() {
        if (transport != V5Protocol.TransportKind.WIFI) return -1;
        try { return wifiLock != null && wifiLock.isHeld() ? 1 : 0; }
        catch (RuntimeException unavailable) { return -1; }
    }

    String diagnosticInterface() { return interfaceName; }

    void diagnosticSignal(long[] row) {
        if (transport != V5Protocol.TransportKind.WIFI || androidNetwork == null
                || Build.VERSION.SDK_INT < 29) return;
        NetworkCapabilities capabilities = connectivityManager.getNetworkCapabilities(androidNetwork);
        if (capabilities == null || !(capabilities.getTransportInfo() instanceof android.net.wifi.WifiInfo)) return;
        android.net.wifi.WifiInfo info = (android.net.wifi.WifiInfo) capabilities.getTransportInfo();
        int rssi = info.getRssi();
        row[9] = rssi > -127 && rssi <= 0 ? rssi : -1;
        row[10] = info.getFrequency() > 0 ? info.getFrequency() : -1;
    }

    InetSocketAddress peer() {
        return peer;
    }

    synchronized boolean acceptAndPin(InetSocketAddress source) {
        if (!isOnSelectedSubnet(source)) return false;
        if (peer == null) {
            peer = source;
            return true;
        }
        return peer.equals(source);
    }

    boolean isPinnedPeer(InetSocketAddress source) {
        InetSocketAddress pinned = peer;
        return pinned != null && pinned.equals(source);
    }

    boolean isPinnedPeer(DatagramPacket source) {
        InetSocketAddress pinned = peer;
        // DatagramPacket.getSocketAddress() constructs a new object on every ACK.
        return pinned != null && pinned.getPort() == source.getPort()
                && pinned.getAddress().equals(source.getAddress());
    }

    void sendDiscovery(byte[] bytes) throws IOException {
        DatagramPacket packet = new DatagramPacket(bytes, bytes.length);
        for (InetAddress destination : destinations) {
            packet.setSocketAddress(new InetSocketAddress(destination, V5Protocol.PORT));
            socket.send(packet);
            socket.send(packet);
        }
    }

    void sendToPeer(byte[] bytes) throws IOException {
        InetSocketAddress destination = peer();
        if (destination == null) throw new IOException("V5 peer is not pinned");
        DatagramPacket packet = new DatagramPacket(bytes, bytes.length, destination);
        socket.send(packet);
    }

    void sendToPeer(DatagramPacket packet) throws IOException {
        InetSocketAddress destination = peer();
        if (destination == null) throw new IOException("V5 peer is not pinned");
        packet.setSocketAddress(destination);
        socket.send(packet);
    }

    boolean revalidate() {
        try {
            if (transport == V5Protocol.TransportKind.WIFI) {
                NetworkCapabilities capabilities =
                        connectivityManager.getNetworkCapabilities(androidNetwork);
                LinkProperties properties = connectivityManager.getLinkProperties(androidNetwork);
                return isPhysicalWifi(capabilities)
                        && properties != null
                        && interfaceName.equals(properties.getInterfaceName())
                        && localFingerprint.equals(wifiFingerprint(properties));
            }
            Candidate usb = findUsbCandidate(connectivityManager);
            return usb != null
                    && Objects.equals(androidNetwork, usb.androidNetwork)
                    && interfaceName.equals(usb.interfaceName)
                    && localFingerprint.equals(usb.localFingerprint);
        } catch (RuntimeException | SocketException ignored) {
            return false;
        }
    }

    String transportLabel() {
        return transport == V5Protocol.TransportKind.WIFI ? "Wi-Fi / local network" : "USB tethering";
    }

    @Override
    public synchronized void close() {
        socket.close();
        releaseWifiLock(wifiLock);
    }

    private boolean isOnSelectedSubnet(InetSocketAddress source) {
        if (source == null || source.isUnresolved()
                || source.getPort() != V5Protocol.PORT
                || !(source.getAddress() instanceof Inet4Address)) {
            return false;
        }
        for (DiscoveryPolicy.Ipv4Subnet subnet : subnets) {
            if (subnet.contains(source.getAddress())) return true;
        }
        return false;
    }

    private static V5NetworkBinding openWifi(Context context, ConnectivityManager manager)
            throws IOException {
        Network selected = selectWifiNetwork(manager);
        if (selected == null) {
            throw new IOException("Connect Android to a physical Wi-Fi network first");
        }
        LinkProperties properties = manager.getLinkProperties(selected);
        if (properties == null || properties.getInterfaceName() == null) {
            throw new IOException("Wi-Fi interface details are unavailable");
        }
        ArrayList<DiscoveryPolicy.Ipv4Subnet> subnets = new ArrayList<>();
        ArrayList<InetAddress> broadcasts = new ArrayList<>();
        for (LinkAddress link : properties.getLinkAddresses()) {
            InetAddress address = link.getAddress();
            int prefix = link.getPrefixLength();
            if (!(address instanceof Inet4Address) || prefix <= 0 || prefix > 32
                    || !isPrivateIpv4((Inet4Address) address)) {
                continue;
            }
            DiscoveryPolicy.Ipv4Subnet subnet = DiscoveryPolicy.Ipv4Subnet.from(address, prefix);
            if (subnet != null) subnets.add(subnet);
            broadcasts.add(directedBroadcast((Inet4Address) address, prefix));
        }
        if (subnets.isEmpty() || broadcasts.isEmpty()) {
            throw new IOException("Wi-Fi has no private IPv4 local subnet");
        }
        DatagramSocket socket = new DatagramSocket(0);
        WifiManager.WifiLock wifiLock = null;
        boolean success = false;
        try {
            selected.bindSocket(socket);
            socket.setBroadcast(true);
            socket.setSoTimeout(4);
            // Pair's quality probes and Start use the same radio policy. Ownership
            // follows this binding, including idle gaps and failed/replaced sessions.
            wifiLock = acquireLowLatencyWifiLock(context);
            V5NetworkBinding binding = new V5NetworkBinding(
                    manager,
                    V5Protocol.TransportKind.WIFI,
                    selected,
                    properties.getInterfaceName(),
                    wifiFingerprint(properties),
                    subnets,
                    deduplicate(broadcasts),
                    socket,
                    wifiLock
            );
            success = true;
            return binding;
        } finally {
            if (!success) {
                socket.close();
                releaseWifiLock(wifiLock);
            }
        }
    }

    private static WifiManager.WifiLock acquireLowLatencyWifiLock(Context context) {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.Q) return null;
        WifiManager.WifiLock lock = null;
        try {
            WifiManager manager = (WifiManager) context.getApplicationContext()
                    .getSystemService(Context.WIFI_SERVICE);
            if (manager == null) return null;
            lock = manager.createWifiLock(
                    WifiManager.WIFI_MODE_FULL_LOW_LATENCY, "Doritrack:V5WiFi");
            lock.setReferenceCounted(false);
            lock.acquire();
            return lock;
        } catch (RuntimeException error) {
            releaseWifiLock(lock);
            // Performance hints must not make an otherwise usable path unavailable.
            Log.w("HolodoriUDP5", "Wi-Fi low-latency lock unavailable", error);
            return null;
        }
    }

    private static void releaseWifiLock(WifiManager.WifiLock lock) {
        if (lock == null) return;
        try {
            if (lock.isHeld()) lock.release();
        } catch (RuntimeException error) {
            // Keep socket/channel cleanup working even if the Wi-Fi service failed.
            Log.w("HolodoriUDP5", "Wi-Fi low-latency lock release failed", error);
        }
    }

    private static V5NetworkBinding openUsb(ConnectivityManager manager) throws IOException {
        Candidate candidate = findUsbCandidate(manager);
        if (candidate == null) {
            throw new IOException("Enable Android USB tethering and connect the cable");
        }
        DatagramSocket socket = new DatagramSocket(
                new InetSocketAddress(candidate.bindAddress, 0)
        );
        boolean success = false;
        try {
            if (candidate.androidNetwork != null) candidate.androidNetwork.bindSocket(socket);
            socket.setBroadcast(true);
            socket.setSoTimeout(4);
            success = true;
            return new V5NetworkBinding(
                    manager,
                    V5Protocol.TransportKind.USB,
                    candidate.androidNetwork,
                    candidate.interfaceName,
                    candidate.localFingerprint,
                    candidate.subnets,
                    candidate.broadcasts,
                    socket,
                    null
            );
        } finally {
            if (!success) socket.close();
        }
    }

    private static Network selectWifiNetwork(ConnectivityManager manager) {
        Network active = manager.getActiveNetwork();
        if (active != null && isPhysicalWifi(manager.getNetworkCapabilities(active))) {
            return active;
        }
        Network selected = null;
        Network[] networks = manager.getAllNetworks();
        if (networks == null) return null;
        for (Network network : networks) {
            if (!isPhysicalWifi(manager.getNetworkCapabilities(network))) continue;
            if (selected != null) return null;
            selected = network;
        }
        return selected;
    }

    private static boolean isPhysicalWifi(NetworkCapabilities capabilities) {
        return capabilities != null
                && capabilities.hasTransport(NetworkCapabilities.TRANSPORT_WIFI)
                && !capabilities.hasTransport(NetworkCapabilities.TRANSPORT_VPN)
                && capabilities.hasCapability(NetworkCapabilities.NET_CAPABILITY_NOT_VPN);
    }

    private static Candidate findUsbCandidate(ConnectivityManager manager)
            throws SocketException {
        Set<String> androidInterfaces = new HashSet<>();
        Map<String, Network> usbNetworks = new HashMap<>();
        boolean snapshotComplete = true;
        try {
            Network[] networks = manager.getAllNetworks();
            if (networks == null) {
                snapshotComplete = false;
            } else {
                for (Network network : networks) {
                    LinkProperties properties = manager.getLinkProperties(network);
                    if (properties == null) {
                        snapshotComplete = false;
                        continue;
                    }
                    String name = DiscoveryPolicy.normalizeInterfaceName(
                            properties.getInterfaceName()
                    );
                    if (name.isEmpty()) {
                        snapshotComplete = false;
                        continue;
                    }
                    NetworkCapabilities capabilities = manager.getNetworkCapabilities(network);
                    // Android 15+ can expose the tether's downstream interface as
                    // a local Network. It is not an upstream interface to exclude.
                    if (isUsbTetherNetwork(capabilities)) {
                        Network previous = usbNetworks.put(name, network);
                        if (previous != null && !previous.equals(network)) {
                            androidInterfaces.add(name);
                        }
                    } else {
                        androidInterfaces.add(name);
                    }
                }
            }
        } catch (RuntimeException ignored) {
            snapshotComplete = false;
            androidInterfaces.clear();
            usbNetworks.clear();
        }

        int bestPriority = 0;
        ArrayList<Candidate> candidates = new ArrayList<>();
        Enumeration<NetworkInterface> interfaces = NetworkInterface.getNetworkInterfaces();
        while (interfaces != null && interfaces.hasMoreElements()) {
            NetworkInterface network = interfaces.nextElement();
            if (!network.isUp() || network.isLoopback()) continue;
            String name = DiscoveryPolicy.normalizeInterfaceName(network.getName());
            Network usbNetwork = usbNetworks.get(name);
            int priority = usbCandidatePriority(
                    network.getName(),
                    network.getDisplayName(),
                    androidInterfaces,
                    snapshotComplete,
                    usbNetwork != null
            );
            if (priority == 0 || priority < bestPriority) continue;
            Candidate candidate = candidateFromInterface(network, usbNetwork);
            if (candidate == null) continue;
            if (priority > bestPriority) {
                candidates.clear();
                bestPriority = priority;
            }
            candidates.add(candidate);
        }
        // Ambiguity is a hard boundary: never spray V5 discovery onto multiple
        // USB-like adapters and guess which one the user meant.
        return candidates.size() == 1 ? candidates.get(0) : null;
    }

    private static boolean isUsbTetherNetwork(NetworkCapabilities capabilities) {
        // LOCAL_NETWORK distinguishes tethering from reverse USB Internet access.
        return Build.VERSION.SDK_INT >= Build.VERSION_CODES.VANILLA_ICE_CREAM
                && capabilities != null
                && capabilities.hasTransport(NetworkCapabilities.TRANSPORT_USB)
                && capabilities.hasCapability(NetworkCapabilities.NET_CAPABILITY_LOCAL_NETWORK)
                && capabilities.hasCapability(NetworkCapabilities.NET_CAPABILITY_NOT_VPN)
                && !capabilities.hasTransport(NetworkCapabilities.TRANSPORT_VPN)
                && !capabilities.hasTransport(NetworkCapabilities.TRANSPORT_WIFI)
                && !capabilities.hasTransport(NetworkCapabilities.TRANSPORT_CELLULAR)
                && !capabilities.hasTransport(NetworkCapabilities.TRANSPORT_ETHERNET);
    }

    static int usbCandidatePriority(
            String name,
            String displayName,
            Set<String> upstreamInterfaces,
            boolean snapshotComplete,
            boolean androidUsbTether
    ) {
        if (upstreamInterfaces.contains(DiscoveryPolicy.normalizeInterfaceName(name))) return 0;
        return androidUsbTether ? 4 : DiscoveryPolicy.candidatePriority(
                name, displayName, upstreamInterfaces, snapshotComplete);
    }

    private static Candidate candidateFromInterface(
            NetworkInterface network,
            Network androidNetwork
    ) {
        ArrayList<DiscoveryPolicy.Ipv4Subnet> subnets = new ArrayList<>();
        ArrayList<InetAddress> broadcasts = new ArrayList<>();
        ArrayList<String> fingerprint = new ArrayList<>();
        InetAddress bindAddress = null;
        for (InterfaceAddress address : network.getInterfaceAddresses()) {
            InetAddress local = address.getAddress();
            InetAddress broadcast = address.getBroadcast();
            if (!(local instanceof Inet4Address) || !(broadcast instanceof Inet4Address)) continue;
            DiscoveryPolicy.Ipv4Subnet subnet = DiscoveryPolicy.Ipv4Subnet.from(
                    local,
                    address.getNetworkPrefixLength()
            );
            if (subnet == null) continue;
            if (bindAddress != null) return null;
            bindAddress = local;
            subnets.add(subnet);
            broadcasts.add(broadcast);
            fingerprint.add(local.getHostAddress() + "/" + address.getNetworkPrefixLength());
        }
        if (subnets.isEmpty() || bindAddress == null) return null;
        Collections.sort(fingerprint);
        return new Candidate(
                androidNetwork,
                network.getName(),
                bindAddress,
                fingerprint,
                subnets,
                deduplicate(broadcasts)
        );
    }

    private static List<String> wifiFingerprint(LinkProperties properties) {
        ArrayList<String> fingerprint = new ArrayList<>();
        for (LinkAddress address : properties.getLinkAddresses()) {
            if (address.getAddress() instanceof Inet4Address) {
                fingerprint.add(address.getAddress().getHostAddress()
                        + "/" + address.getPrefixLength());
            }
        }
        Collections.sort(fingerprint);
        return fingerprint;
    }

    private static Inet4Address directedBroadcast(Inet4Address address, int prefix)
            throws IOException {
        byte[] raw = address.getAddress();
        int value = ((raw[0] & 0xFF) << 24)
                | ((raw[1] & 0xFF) << 16)
                | ((raw[2] & 0xFF) << 8)
                | (raw[3] & 0xFF);
        int mask = prefix == 0 ? 0 : -1 << (32 - prefix);
        int broadcast = (value & mask) | ~mask;
        byte[] bytes = {
                (byte) (broadcast >>> 24),
                (byte) (broadcast >>> 16),
                (byte) (broadcast >>> 8),
                (byte) broadcast
        };
        return (Inet4Address) InetAddress.getByAddress(bytes);
    }

    private static boolean isPrivateIpv4(Inet4Address address) {
        byte[] bytes = address.getAddress();
        int first = bytes[0] & 0xFF;
        int second = bytes[1] & 0xFF;
        return first == 10
                || (first == 172 && second >= 16 && second <= 31)
                || (first == 192 && second == 168)
                || (first == 169 && second == 254);
    }

    private static List<InetAddress> deduplicate(List<InetAddress> addresses) {
        return new ArrayList<>(new HashSet<>(addresses));
    }

    private static final class Candidate {
        final Network androidNetwork;
        final String interfaceName;
        final InetAddress bindAddress;
        final List<String> localFingerprint;
        final List<DiscoveryPolicy.Ipv4Subnet> subnets;
        final List<InetAddress> broadcasts;

        Candidate(
                Network androidNetwork,
                String interfaceName,
                InetAddress bindAddress,
                List<String> localFingerprint,
                List<DiscoveryPolicy.Ipv4Subnet> subnets,
                List<InetAddress> broadcasts
        ) {
            this.androidNetwork = androidNetwork;
            this.interfaceName = interfaceName;
            this.bindAddress = bindAddress;
            this.localFingerprint = localFingerprint;
            this.subnets = subnets;
            this.broadcasts = broadcasts;
        }
    }
}
