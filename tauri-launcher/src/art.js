// Phone-to-PC link illustration shared by every screen. One SVG carries both
// the USB cable and the Wi-Fi waves so a transport change can cross-fade
// instead of swapping images. Motion is driven purely by CSS from the
// `data-transport` and `data-state` attributes (see styles.css), so this file
// never runs on a timer.

export const LINK_STATES = ["idle", "searching", "connected", "off"];

const CABLE = "M66 60 C90 60 96 72 108 72 S126 60 150 60";
const CABLE_BACK = "M150 60 C126 60 120 72 108 72 S90 60 66 60";

function arc(cx, r, sweep) {
  const dx = 0.788 * r;
  const dy = 0.616 * r;
  const x = sweep === 1 ? cx + dx : cx - dx;
  const y1 = (60 - dy).toFixed(1);
  const y2 = (60 + dy).toFixed(1);
  return `M${x.toFixed(1)} ${y1} A${r} ${r} 0 0 ${sweep} ${x.toFixed(1)} ${y2}`;
}

const WAVES = [15, 27, 38]
  .map(
    (r, index) =>
      `<path class="wave wave-phone" style="--i:${index}" d="${arc(66, r, 1)}" />` +
      `<path class="wave wave-pc" style="--i:${index}" d="${arc(150, r, 0)}" />`,
  )
  .join("");

export function linkArtMarkup(transport = "usb", state = "idle") {
  return (
    `<svg class="link-art" viewBox="0 0 216 120" data-transport="${transport}" data-state="${state}" aria-hidden="true" focusable="false">` +
    `<g class="device phone">` +
    `<rect class="body" x="22" y="24" width="44" height="72" rx="8" />` +
    `<rect class="glass" x="28" y="32" width="32" height="52" rx="3" />` +
    `<circle class="dot" cx="44" cy="90" r="1.8" />` +
    `</g>` +
    `<g class="device pc">` +
    `<rect class="body" x="150" y="28" width="66" height="46" rx="6" />` +
    `<rect class="glass" x="156" y="34" width="54" height="34" rx="2" />` +
    `<path class="stand" d="M183 74v12M167 88h32" />` +
    `</g>` +
    `<g class="cable">` +
    `<path class="cable-line" pathLength="100" d="${CABLE}" />` +
    `<path class="pulse pulse-out" pathLength="100" d="${CABLE}" />` +
    `<path class="pulse pulse-back" pathLength="100" d="${CABLE_BACK}" />` +
    `<rect class="plug" x="140" y="55" width="10" height="10" rx="2" />` +
    `</g>` +
    `<g class="waves">${WAVES}</g>` +
    `</svg>`
  );
}

export function mountLinkArt(container, transport = "usb", state = "idle") {
  container.innerHTML = linkArtMarkup(transport, state);
  return container.firstElementChild;
}

export function setLinkArt(svg, transport, state) {
  if (!svg) return;
  if (transport) svg.dataset.transport = transport;
  if (state && LINK_STATES.includes(state)) svg.dataset.state = state;
}
