package dev.holodori.trackpad;

import android.content.Context;
import android.net.ConnectivityManager;
import android.net.LinkAddress;
import android.net.LinkProperties;
import android.net.Network;
import android.net.NetworkCapabilities;

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
import java.util.HashSet;
import java.util.List;
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

    private volatile InetSocketAddress peer;

    private V5NetworkBinding(
            ConnectivityManager connectivityManager,
            V5Protocol.TransportKind transport,
            Network androidNetwork,
            String interfaceName,
            List<String> localFingerprint,
            List<DiscoveryPolicy.Ipv4Subnet> subnets,
            List<InetAddress> destinations,
            DatagramSocket socket
    ) {
        this.connectivityManager = connectivityManager;
        this.transport = transport;
        this.androidNetwork = androidNetwork;
        this.interfaceName = interfaceName;
        this.localFingerprint = localFingerprint;
        this.subnets = subnets;
        this.destinations = destinations;
        this.socket = socket;
    }

    static V5NetworkBinding open(Context context, V5Protocol.TransportKind transport)
            throws IOException {
        Context application = context.getApplicationContext();
        ConnectivityManager manager = (ConnectivityManager) (application == null
                ? context
                : application).getSystemService(Context.CONNECTIVITY_SERVICE);
        if (manager == null) throw new IOException("Android network service is unavailable");
        return transport == V5Protocol.TransportKind.WIFI
                ? openWifi(manager)
                : openUsb(manager);
    }

    DatagramSocket socket() {
        return socket;
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
    public void close() {
        socket.close();
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

    private static V5NetworkBinding openWifi(ConnectivityManager manager) throws IOException {
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
        boolean success = false;
        try {
            selected.bindSocket(socket);
            socket.setBroadcast(true);
            socket.setSoTimeout(4);
            success = true;
            return new V5NetworkBinding(
                    manager,
                    V5Protocol.TransportKind.WIFI,
                    selected,
                    properties.getInterfaceName(),
                    wifiFingerprint(properties),
                    subnets,
                    deduplicate(broadcasts),
                    socket
            );
        } finally {
            if (!success) socket.close();
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
            socket.setBroadcast(true);
            socket.setSoTimeout(4);
            success = true;
            return new V5NetworkBinding(
                    manager,
                    V5Protocol.TransportKind.USB,
                    null,
                    candidate.interfaceName,
                    candidate.localFingerprint,
                    candidate.subnets,
                    candidate.broadcasts,
                    socket
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
                    if (!name.isEmpty()) androidInterfaces.add(name);
                }
            }
        } catch (RuntimeException ignored) {
            snapshotComplete = false;
            androidInterfaces.clear();
        }

        int bestPriority = 0;
        ArrayList<Candidate> candidates = new ArrayList<>();
        Enumeration<NetworkInterface> interfaces = NetworkInterface.getNetworkInterfaces();
        while (interfaces != null && interfaces.hasMoreElements()) {
            NetworkInterface network = interfaces.nextElement();
            if (!network.isUp() || network.isLoopback()) continue;
            int priority = DiscoveryPolicy.candidatePriority(
                    network.getName(),
                    network.getDisplayName(),
                    androidInterfaces,
                    snapshotComplete
            );
            if (priority == 0 || priority < bestPriority) continue;
            Candidate candidate = candidateFromInterface(network);
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

    private static Candidate candidateFromInterface(NetworkInterface network) {
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
        final String interfaceName;
        final InetAddress bindAddress;
        final List<String> localFingerprint;
        final List<DiscoveryPolicy.Ipv4Subnet> subnets;
        final List<InetAddress> broadcasts;

        Candidate(
                String interfaceName,
                InetAddress bindAddress,
                List<String> localFingerprint,
                List<DiscoveryPolicy.Ipv4Subnet> subnets,
                List<InetAddress> broadcasts
        ) {
            this.interfaceName = interfaceName;
            this.bindAddress = bindAddress;
            this.localFingerprint = localFingerprint;
            this.subnets = subnets;
            this.broadcasts = broadcasts;
        }
    }
}
