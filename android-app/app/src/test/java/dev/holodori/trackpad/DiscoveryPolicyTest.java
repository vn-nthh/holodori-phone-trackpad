package dev.holodori.trackpad;

import static org.junit.Assert.assertEquals;
import static org.junit.Assert.assertFalse;
import static org.junit.Assert.assertNull;
import static org.junit.Assert.assertTrue;

import org.junit.Test;

import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.util.Arrays;
import java.util.Collections;
import java.util.HashSet;
import java.util.Set;
import java.util.zip.CRC32;

public final class DiscoveryPolicyTest {
    @Test
    public void matcherPrefersExplicitUsbTetherIdentities() {
        assertEquals(
                DiscoveryPolicy.InterfaceMatch.EXPLICIT_USB_TETHER,
                DiscoveryPolicy.matchInterface("rndis0", "rndis0")
        );
        assertEquals(
                DiscoveryPolicy.InterfaceMatch.EXPLICIT_USB_TETHER,
                DiscoveryPolicy.matchInterface("ncm0", "CDC NCM")
        );
        assertEquals(
                DiscoveryPolicy.InterfaceMatch.EXPLICIT_USB_TETHER,
                DiscoveryPolicy.matchInterface("usb_tether0", "USB tethering")
        );
        assertEquals(
                DiscoveryPolicy.InterfaceMatch.USB_FALLBACK,
                DiscoveryPolicy.matchInterface("usb0", "usb0")
        );
        assertEquals(
                DiscoveryPolicy.InterfaceMatch.ETH_FALLBACK,
                DiscoveryPolicy.matchInterface("eth0", "Ethernet")
        );
    }

    @Test
    public void matcherRejectsNormalUpstreamFamilies() {
        for (String name : Arrays.asList(
                "wlan0",
                "swlan0",
                "ap0",
                "p2p0",
                "rmnet_data0",
                "ccmni0",
                "pdp0",
                "tun0",
                "ppp0",
                "ipsec0",
                "wg0",
                "en0"
        )) {
            assertEquals(
                    name,
                    DiscoveryPolicy.InterfaceMatch.REJECTED,
                    DiscoveryPolicy.matchInterface(name, name)
            );
        }
        assertEquals(
                DiscoveryPolicy.InterfaceMatch.REJECTED,
                DiscoveryPolicy.matchInterface("en0", "Ethernet")
        );
    }

    @Test
    public void candidatesExcludeInterfacesExposedByAndroidNetworks() {
        Set<String> exposed = new HashSet<>(Arrays.asList(
                "rndis0",
                "usb0",
                "eth0"
        ));
        assertFalse(DiscoveryPolicy.isCandidateInterface(
                "rndis0",
                "rndis0",
                exposed,
                true
        ));
        assertFalse(DiscoveryPolicy.isCandidateInterface(
                "usb0",
                "usb0",
                exposed,
                true
        ));
        assertFalse(DiscoveryPolicy.isCandidateInterface(
                "eth0",
                "Ethernet",
                exposed,
                true
        ));
    }

    @Test
    public void genericFallbacksRequireCompleteAndroidNetworkSnapshot() {
        Set<String> noExposedInterfaces = Collections.emptySet();
        assertTrue(DiscoveryPolicy.isCandidateInterface(
                "rndis0",
                "RNDIS",
                noExposedInterfaces,
                false
        ));
        assertFalse(DiscoveryPolicy.isCandidateInterface(
                "usb0",
                "usb0",
                noExposedInterfaces,
                false
        ));
        assertFalse(DiscoveryPolicy.isCandidateInterface(
                "eth0",
                "Ethernet",
                noExposedInterfaces,
                false
        ));
        assertTrue(DiscoveryPolicy.isCandidateInterface(
                "usb0",
                "usb0",
                noExposedInterfaces,
                true
        ));
        assertTrue(DiscoveryPolicy.isCandidateInterface(
                "eth0",
                "Ethernet",
                noExposedInterfaces,
                true
        ));
    }

    @Test
    public void candidatePriorityPrefersExplicitUsbNamesOverFallbacks() {
        Set<String> noExposedInterfaces = Collections.emptySet();
        int explicit = DiscoveryPolicy.candidatePriority(
                "rndis0",
                "RNDIS",
                noExposedInterfaces,
                true
        );
        int usbFallback = DiscoveryPolicy.candidatePriority(
                "usb0",
                "usb0",
                noExposedInterfaces,
                true
        );
        int ethFallback = DiscoveryPolicy.candidatePriority(
                "eth0",
                "Ethernet",
                noExposedInterfaces,
                true
        );
        assertTrue(explicit > usbFallback);
        assertTrue(usbFallback > ethFallback);
    }

    @Test
    public void ipv4SubnetChecksFullPrefix() throws Exception {
        DiscoveryPolicy.Ipv4Subnet subnet = DiscoveryPolicy.Ipv4Subnet.from(
                address("192.168.42.129"),
                24
        );
        assertTrue(subnet.contains(address("192.168.42.1")));
        assertTrue(subnet.contains(address("192.168.42.255")));
        assertFalse(subnet.contains(address("192.168.43.1")));
        assertFalse(subnet.contains(address("2001:db8::1")));
    }

    @Test
    public void ipv4SubnetRejectsInvalidInputs() throws Exception {
        assertNull(DiscoveryPolicy.Ipv4Subnet.from(address("2001:db8::1"), 64));
        assertNull(DiscoveryPolicy.Ipv4Subnet.from(address("192.168.42.1"), -1));
        assertNull(DiscoveryPolicy.Ipv4Subnet.from(address("192.168.42.1"), 0));
        assertNull(DiscoveryPolicy.Ipv4Subnet.from(address("192.168.42.1"), 33));
    }

    @Test
    public void discoveryAckAcceptsCurrentAndLegacyPayloadPorts() {
        long nonce = 0x1020304050607080L;
        long sessionId = 0x0102030405060708L;
        assertValidAck(discoveryAck(nonce, sessionId, 0), nonce, sessionId);
        assertValidAck(
                discoveryAck(nonce, sessionId, DiscoveryPolicy.PORT),
                nonce,
                sessionId
        );
        assertFalse(DiscoveryPolicy.isValidAck(
                discoveryAck(nonce, sessionId, 9),
                DiscoveryPolicy.SIZE,
                nonce,
                sessionId,
                new CRC32()
        ));
    }

    @Test
    public void discoveryAckValidatesEverySessionBindingField() {
        long nonce = 101;
        long sessionId = 202;
        byte[] valid = discoveryAck(nonce, sessionId, DiscoveryPolicy.PORT);

        assertFalse(DiscoveryPolicy.isValidAck(
                valid,
                DiscoveryPolicy.SIZE - 1,
                nonce,
                sessionId,
                new CRC32()
        ));
        assertInvalidAfterRewrite(valid, nonce, sessionId, 4, (byte) 2);
        assertInvalidAfterRewrite(
                valid,
                nonce,
                sessionId,
                5,
                (byte) DiscoveryPolicy.HELLO
        );
        assertInvalidAfterRewrite(valid, nonce + 1, sessionId, -1, (byte) 0);
        assertInvalidAfterRewrite(valid, nonce, sessionId + 1, -1, (byte) 0);

        byte[] badMagic = valid.clone();
        badMagic[0] = 'X';
        rewriteCrc(badMagic);
        assertFalse(isValidAck(badMagic, nonce, sessionId));

        byte[] badCrc = valid.clone();
        badCrc[DiscoveryPolicy.SIZE - 1] ^= 1;
        assertFalse(isValidAck(badCrc, nonce, sessionId));
    }

    @Test
    public void endpointPinsFirstCandidateAndRejectsRetargets() throws Exception {
        DiscoveryPolicy.Ipv4Subnet subnet = DiscoveryPolicy.Ipv4Subnet.from(
                address("192.168.42.129"),
                24
        );
        InetSocketAddress first = endpoint("192.168.42.2", DiscoveryPolicy.PORT);

        assertEquals(
                DiscoveryPolicy.EndpointDecision.PIN,
                DiscoveryPolicy.decideEndpoint(
                        null,
                        first,
                        Collections.singletonList(subnet)
                )
        );
        assertEquals(
                DiscoveryPolicy.EndpointDecision.ACCEPT_PINNED,
                DiscoveryPolicy.decideEndpoint(
                        first,
                        endpoint("192.168.42.2", DiscoveryPolicy.PORT),
                        Collections.emptyList()
                )
        );
        assertEquals(
                DiscoveryPolicy.EndpointDecision.REJECT,
                DiscoveryPolicy.decideEndpoint(
                        null,
                        endpoint("192.168.42.2", DiscoveryPolicy.PORT + 1),
                        Collections.singletonList(subnet)
                )
        );
        assertEquals(
                DiscoveryPolicy.EndpointDecision.REJECT,
                DiscoveryPolicy.decideEndpoint(
                        first,
                        endpoint("192.168.42.2", DiscoveryPolicy.PORT + 1),
                        Collections.singletonList(subnet)
                )
        );
        assertEquals(
                DiscoveryPolicy.EndpointDecision.REJECT,
                DiscoveryPolicy.decideEndpoint(
                        first,
                        endpoint("192.168.42.3", DiscoveryPolicy.PORT),
                        Collections.singletonList(subnet)
                )
        );
        assertEquals(
                DiscoveryPolicy.EndpointDecision.REJECT,
                DiscoveryPolicy.decideEndpoint(
                        null,
                        endpoint("192.168.43.2", DiscoveryPolicy.PORT),
                        Collections.singletonList(subnet)
                )
        );
    }

    private static void assertInvalidAfterRewrite(
            byte[] valid,
            long expectedNonce,
            long expectedSessionId,
            int changedOffset,
            byte changedValue
    ) {
        byte[] changed = valid.clone();
        if (changedOffset >= 0) {
            changed[changedOffset] = changedValue;
        }
        rewriteCrc(changed);
        assertFalse(isValidAck(changed, expectedNonce, expectedSessionId));
    }

    private static void assertValidAck(
            byte[] ack,
            long expectedNonce,
            long expectedSessionId
    ) {
        assertTrue(isValidAck(ack, expectedNonce, expectedSessionId));
    }

    private static boolean isValidAck(
            byte[] ack,
            long expectedNonce,
            long expectedSessionId
    ) {
        return DiscoveryPolicy.isValidAck(
                ack,
                ack.length,
                expectedNonce,
                expectedSessionId,
                new CRC32()
        );
    }

    private static byte[] discoveryAck(long nonce, long sessionId, int port) {
        byte[] bytes = new byte[DiscoveryPolicy.SIZE];
        ByteBuffer packet = ByteBuffer.wrap(bytes).order(ByteOrder.LITTLE_ENDIAN);
        packet.put((byte) 'H');
        packet.put((byte) 'P');
        packet.put((byte) 'T');
        packet.put((byte) 'D');
        packet.put((byte) DiscoveryPolicy.VERSION);
        packet.put((byte) DiscoveryPolicy.ACK);
        packet.putShort((short) 0);
        packet.putLong(nonce);
        packet.putLong(sessionId);
        packet.putShort((short) port);
        packet.putShort((short) 0);
        rewriteCrc(bytes);
        return bytes;
    }

    private static void rewriteCrc(byte[] bytes) {
        CRC32 crc = new CRC32();
        crc.update(bytes, 0, DiscoveryPolicy.SIZE - 4);
        ByteBuffer.wrap(bytes)
                .order(ByteOrder.LITTLE_ENDIAN)
                .putInt(DiscoveryPolicy.SIZE - 4, (int) crc.getValue());
    }

    private static InetAddress address(String value) throws Exception {
        return InetAddress.getByName(value);
    }

    private static InetSocketAddress endpoint(String address, int port) throws Exception {
        return new InetSocketAddress(DiscoveryPolicyTest.address(address), port);
    }
}
