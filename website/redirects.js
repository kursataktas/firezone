// Add all server-side redirects here. Will be loaded by next.config.mjs.

module.exports = [
  /*
   *
   * Windows GUI Client
   *
   */
  {
    source: "/dl/firezone-client-gui-windows/latest/x86_64",
    destination:
      // mark:current-gui-version
      "https://www.github.com/firezone/firezone/releases/download/gui-client-1.3.13/firezone-client-gui-windows_1.3.13_x86_64.msi",
    permanent: false,
  },
  /*
   *
   * Linux Clients
   *
   */
  {
    source: "/dl/firezone-client-gui-linux/latest/x86_64",
    destination:
      // mark:current-gui-version
      "https://www.github.com/firezone/firezone/releases/download/gui-client-1.3.13/firezone-client-gui-linux_1.3.13_x86_64.deb",
    permanent: false,
  },
  {
    source: "/dl/firezone-client-gui-linux/latest/aarch64",
    destination:
      // mark:current-gui-version
      "https://www.github.com/firezone/firezone/releases/download/gui-client-1.3.13/firezone-client-gui-linux_1.3.13_aarch64.deb",
    permanent: false,
  },
  {
    source: "/dl/firezone-client-headless-linux/latest/x86_64",
    destination:
      // mark:current-headless-version
      "https://www.github.com/firezone/firezone/releases/download/headless-client-1.3.7/firezone-client-headless-linux_1.3.7_x86_64",
    permanent: false,
  },
  {
    source: "/dl/firezone-client-headless-linux/latest/aarch64",
    destination:
      // mark:current-headless-version
      "https://www.github.com/firezone/firezone/releases/download/headless-client-1.3.7/firezone-client-headless-linux_1.3.7_aarch64",
    permanent: false,
  },
  {
    source: "/dl/firezone-client-headless-linux/latest/armv7",
    destination:
      // mark:current-headless-version
      "https://www.github.com/firezone/firezone/releases/download/headless-client-1.3.7/firezone-client-headless-linux_1.3.7_armv7",
    permanent: false,
  },
  /*
   *
   * Gateway
   *
   */
  {
    source: "/dl/firezone-gateway/latest/x86_64",
    destination:
      // mark:current-gateway-version
      "https://www.github.com/firezone/firezone/releases/download/gateway-1.4.1/firezone-gateway_1.4.1_x86_64",
    permanent: false,
  },
  {
    source: "/dl/firezone-gateway/latest/aarch64",
    destination:
      // mark:current-gateway-version
      "https://www.github.com/firezone/firezone/releases/download/gateway-1.4.1/firezone-gateway_1.4.1_aarch64",
    permanent: false,
  },
  {
    source: "/dl/firezone-gateway/latest/armv7",
    destination:
      // mark:current-gateway-version
      "https://www.github.com/firezone/firezone/releases/download/gateway-1.4.1/firezone-gateway_1.4.1_armv7",
    permanent: false,
  },
  /*
   * Redirects for old KB URLs
   *
   */
  {
    // TODO: Remove on or after 2024-10-21 after crawlers have re-indexed
    source: "/kb/user-guides/:path",
    destination: "/kb/client-apps/:path",
    permanent: true,
  },
  {
    // TODO: Remove on or after 2024-10-21 after crawlers have re-indexed
    source: "/kb/user-guides",
    destination: "/kb/client-apps",
    permanent: true,
  },
];
