package dev.holodori.trackpad;

import java.net.Inet4Address;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.util.List;
import java.util.Locale;
import java.util.Set;
import java.util.zip.CRC32;

/**
 * Pure policy helpers for selecting and validating a USB-tether discovery peer.
 *
 * <p>RNDIS, NCM, and explicit USB-tether identities are preferred. Bare
 * {@code usbN} and {@code ethN} names are conservative OEM fallbacks: they are
 * eligible only after ConnectivityManager supplied a complete LinkProperties
 * snapshot and did not expose that interface as a normal Android network.</p>
 */
final class DiscoveryPolicy {
    static final int PORT = 42_825;
    static final int VERSION = 1;
    static final int HELLO = 1;
    static final int ACK = 2;
    static final int SIZE = 32;

    enum InterfaceMatch {
        EXPLICIT_USB_TETHER,
        USB_FALLBACK,
        ETH_FALLBACK,
        REJECTED
    }

    enum EndpointDecision {
        PIN,
        ACCEPT_PINNED,
        REJECT
    }

    private DiscoveryPolicy() {
    }

    static boolean hasDiscoveryMagic(byte[] bytes, int length) {
        return bytes != null
                && length >= 4
                && bytes.length >= length
                && bytes[0] == 'H'
                && bytes[1] == 'P'
                && bytes[2] == 'T'
                && bytes[3] == 'D';
    }

    static boolean isValidAck(
            byte[] bytes,
            int length,
            long expectedNonce,
            long expectedSessionId,
            CRC32 crc
    ) {
        if (bytes == null
                || crc == null
                || length != SIZE
                || bytes.length < SIZE
                || !hasDiscoveryMagic(bytes, length)
                || (bytes[4] & 0xFF) != VERSION
                || (bytes[5] & 0xFF) != ACK
                || readLongLittleEndian(bytes, 8) != expectedNonce
                || readLongLittleEndian(bytes, 16) != expectedSessionId) {
            return false;
        }

        int advertisedPort = readUnsignedShortLittleEndian(bytes, 24);
        if (advertisedPort != 0 && advertisedPort != PORT) {
            return false;
        }

        crc.reset();
        crc.update(bytes, 0, SIZE - 4);
        return crc.getValue() == readUnsignedIntLittleEndian(bytes, SIZE - 4);
    }

    static InterfaceMatch matchInterface(String name, String displayName) {
        String normalizedName = normalize(name);
        String normalizedDisplayName = normalize(displayName);
        if (isKnownNonUsbInterface(normalizedName)) {
            return InterfaceMatch.REJECTED;
        }
        if (containsFamilyToken(normalizedName, "rndis")
                || containsFamilyToken(normalizedDisplayName, "rndis")
                || containsFamilyToken(normalizedName, "ncm")
                || containsFamilyToken(normalizedDisplayName, "ncm")
                || containsUsbTetherPhrase(normalizedName)
                || containsUsbTetherPhrase(normalizedDisplayName)) {
            return InterfaceMatch.EXPLICIT_USB_TETHER;
        }
        if (isIndexedKernelName(normalizedName, "usb")
                || isIndexedKernelName(normalizedDisplayName, "usb")) {
            return InterfaceMatch.USB_FALLBACK;
        }
        if (isIndexedKernelName(normalizedName, "eth")) {
            return InterfaceMatch.ETH_FALLBACK;
        }
        return InterfaceMatch.REJECTED;
    }

    static boolean isCandidateInterface(
            String name,
            String displayName,
            Set<String> androidNetworkInterfaces,
            boolean linkPropertiesSnapshotComplete
    ) {
        return candidatePriority(
                name,
                displayName,
                androidNetworkInterfaces,
                linkPropertiesSnapshotComplete
        ) > 0;
    }

    static int candidatePriority(
            String name,
            String displayName,
            Set<String> androidNetworkInterfaces,
            boolean linkPropertiesSnapshotComplete
    ) {
        InterfaceMatch match = matchInterface(name, displayName);
        if (match == InterfaceMatch.REJECTED
                || isExposedAndroidNetwork(
                name,
                displayName,
                androidNetworkInterfaces
        )) {
            return 0;
        }
        if (match == InterfaceMatch.EXPLICIT_USB_TETHER) {
            return 3;
        }
        if (!linkPropertiesSnapshotComplete) {
            return 0;
        }
        return match == InterfaceMatch.USB_FALLBACK ? 2 : 1;
    }

    static EndpointDecision decideEndpoint(
            InetSocketAddress pinned,
            InetSocketAddress source,
            List<Ipv4Subnet> candidateSubnets
    ) {
        if (source == null
                || source.isUnresolved()
                || !(source.getAddress() instanceof Inet4Address)
                || source.getPort() != PORT) {
            return EndpointDecision.REJECT;
        }
        if (pinned != null) {
            return sameEndpoint(pinned, source)
                    ? EndpointDecision.ACCEPT_PINNED
                    : EndpointDecision.REJECT;
        }
        if (candidateSubnets == null) {
            return EndpointDecision.REJECT;
        }
        for (Ipv4Subnet subnet : candidateSubnets) {
            if (subnet != null && subnet.contains(source.getAddress())) {
                return EndpointDecision.PIN;
            }
        }
        return EndpointDecision.REJECT;
    }

    static String normalizeInterfaceName(String name) {
        return normalize(name);
    }

    private static boolean sameEndpoint(
            InetSocketAddress first,
            InetSocketAddress second
    ) {
        return !first.isUnresolved()
                && first.getPort() == second.getPort()
                && first.getAddress().equals(second.getAddress());
    }

    private static boolean isExposedAndroidNetwork(
            String name,
            String displayName,
            Set<String> androidNetworkInterfaces
    ) {
        if (androidNetworkInterfaces == null || androidNetworkInterfaces.isEmpty()) {
            return false;
        }
        String normalizedName = normalize(name);
        String normalizedDisplayName = normalize(displayName);
        return (!normalizedName.isEmpty()
                && androidNetworkInterfaces.contains(normalizedName))
                || (!normalizedDisplayName.isEmpty()
                && androidNetworkInterfaces.contains(normalizedDisplayName));
    }

    private static boolean isKnownNonUsbInterface(String name) {
        return hasKernelFamily(name, "lo")
                || hasKernelFamily(name, "wlan")
                || hasKernelFamily(name, "swlan")
                || hasKernelFamily(name, "wifi")
                || hasKernelFamily(name, "ap")
                || hasKernelFamily(name, "p2p")
                || hasKernelFamily(name, "rmnet")
                || hasKernelFamily(name, "ccmni")
                || hasKernelFamily(name, "pdp")
                || hasKernelFamily(name, "cell")
                || hasKernelFamily(name, "tun")
                || hasKernelFamily(name, "tap")
                || hasKernelFamily(name, "ppp")
                || hasKernelFamily(name, "ipsec")
                || hasKernelFamily(name, "vpn")
                || hasKernelFamily(name, "wg");
    }

    private static boolean hasKernelFamily(String value, String family) {
        if (!value.startsWith(family)) {
            return false;
        }
        if (value.length() == family.length()) {
            return true;
        }
        char suffix = value.charAt(family.length());
        return Character.isDigit(suffix)
                || suffix == '.'
                || suffix == '-'
                || suffix == '_'
                || suffix == ':';
    }

    private static boolean isIndexedKernelName(String value, String family) {
        if (!value.startsWith(family)) {
            return false;
        }
        int index = family.length();
        while (index < value.length() && isInterfaceSeparator(value.charAt(index))) {
            index++;
        }
        return index < value.length() && Character.isDigit(value.charAt(index));
    }

    private static boolean containsUsbTetherPhrase(String value) {
        return containsFamilyToken(value, "usb")
                && (containsFamilyToken(value, "tether")
                || containsFamilyToken(value, "tethering"));
    }

    private static boolean containsFamilyToken(String value, String family) {
        int fromIndex = 0;
        while (fromIndex < value.length()) {
            int index = value.indexOf(family, fromIndex);
            if (index < 0) {
                return false;
            }
            boolean startsToken = index == 0
                    || !Character.isLetterOrDigit(value.charAt(index - 1));
            int end = index + family.length();
            while (end < value.length() && Character.isDigit(value.charAt(end))) {
                end++;
            }
            boolean endsToken = end == value.length()
                    || !Character.isLetterOrDigit(value.charAt(end));
            if (startsToken && endsToken) {
                return true;
            }
            fromIndex = index + 1;
        }
        return false;
    }

    private static boolean isInterfaceSeparator(char character) {
        return character == '.'
                || character == '-'
                || character == '_'
                || character == ':';
    }

    private static String normalize(String value) {
        return value == null ? "" : value.trim().toLowerCase(Locale.ROOT);
    }

    private static int readUnsignedShortLittleEndian(byte[] bytes, int offset) {
        return (bytes[offset] & 0xFF)
                | ((bytes[offset + 1] & 0xFF) << 8);
    }

    private static long readUnsignedIntLittleEndian(byte[] bytes, int offset) {
        return (bytes[offset] & 0xFFL)
                | ((bytes[offset + 1] & 0xFFL) << 8)
                | ((bytes[offset + 2] & 0xFFL) << 16)
                | ((bytes[offset + 3] & 0xFFL) << 24);
    }

    private static long readLongLittleEndian(byte[] bytes, int offset) {
        long value = 0;
        for (int index = 7; index >= 0; index--) {
            value = (value << 8) | (bytes[offset + index] & 0xFFL);
        }
        return value;
    }

    static final class Ipv4Subnet {
        private final int network;
        private final int mask;

        private Ipv4Subnet(int network, int mask) {
            this.network = network;
            this.mask = mask;
        }

        static Ipv4Subnet from(InetAddress localAddress, int prefixLength) {
            if (!(localAddress instanceof Inet4Address)
                    || prefixLength <= 0
                    || prefixLength > Integer.SIZE) {
                return null;
            }
            int mask = prefixLength == 0 ? 0 : -1 << (Integer.SIZE - prefixLength);
            return new Ipv4Subnet(toInt(localAddress) & mask, mask);
        }

        boolean contains(InetAddress address) {
            return address instanceof Inet4Address
                    && (toInt(address) & mask) == network;
        }

        @Override
        public boolean equals(Object other) {
            if (this == other) {
                return true;
            }
            if (!(other instanceof Ipv4Subnet)) {
                return false;
            }
            Ipv4Subnet subnet = (Ipv4Subnet) other;
            return network == subnet.network && mask == subnet.mask;
        }

        @Override
        public int hashCode() {
            return 31 * network + mask;
        }

        private static int toInt(InetAddress address) {
            byte[] bytes = address.getAddress();
            return ((bytes[0] & 0xFF) << 24)
                    | ((bytes[1] & 0xFF) << 16)
                    | ((bytes[2] & 0xFF) << 8)
                    | (bytes[3] & 0xFF);
        }
    }
}
