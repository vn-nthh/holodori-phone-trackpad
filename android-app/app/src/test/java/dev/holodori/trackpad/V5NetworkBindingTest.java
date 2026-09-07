package dev.holodori.trackpad;

import static org.junit.Assert.assertEquals;

import org.junit.Test;

import java.util.Collections;

public final class V5NetworkBindingTest {
    @Test
    public void exposedTetherNetworkNoLongerFailsTheLegacyUpstreamExclusion() {
        // Previously every ConnectivityManager interface went into the exclusion
        // set, even when Android identified it as a downstream USB local network.
        assertEquals(0, DiscoveryPolicy.candidatePriority(
                "rndis0", "rndis0", Collections.singleton("rndis0"), true));
        assertEquals(4, V5NetworkBinding.usbCandidatePriority(
                "rndis0", "rndis0", Collections.emptySet(), true, true));
        // Platform identification also works with an OEM name outside the old
        // heuristic. Without that identification the unknown name stays rejected.
        assertEquals(4, V5NetworkBinding.usbCandidatePriority(
                "ecm0", "ecm0", Collections.emptySet(), true, true));
        assertEquals(0, V5NetworkBinding.usbCandidatePriority(
                "ecm0", "ecm0", Collections.emptySet(), true, false));
    }

    @Test
    public void conflictingUpstreamNetworkStillBlocksTheTetherInterface() {
        assertEquals(0, V5NetworkBinding.usbCandidatePriority(
                "rndis0", "rndis0", Collections.singleton("rndis0"), true, true));
        assertEquals(0, V5NetworkBinding.usbCandidatePriority(
                "usb0", "usb0", Collections.singleton("usb0"), true, false));
    }

    @Test
    public void olderPhonesKeepConservativeInterfaceDiscovery() {
        assertEquals(3, V5NetworkBinding.usbCandidatePriority(
                "rndis0", "rndis0", Collections.emptySet(), false, false));
        assertEquals(2, V5NetworkBinding.usbCandidatePriority(
                "usb0", "usb0", Collections.emptySet(), true, false));
        assertEquals(0, V5NetworkBinding.usbCandidatePriority(
                "usb0", "usb0", Collections.emptySet(), false, false));
        assertEquals(0, V5NetworkBinding.usbCandidatePriority(
                "wlan0", "wlan0", Collections.emptySet(), true, false));
    }
}
