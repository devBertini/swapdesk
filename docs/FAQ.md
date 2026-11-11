# FAQ

## The other computer does not show up

Swapdesk finds the other computer with DNS-SD (mDNS) on the local network and talks to it over UDP ports 47820 and 47821 (on a Windows computer with several accounts, each account has its own pair from there up, to 47882 and 47883). It only works when both computers are on the same network segment. Check these, in this order:

1. **Swapdesk is open on the other computer**, or its service is installed and running (Windows: `SwapdeskService` in Services).
2. **Both are on the same network.** The same Wi-Fi name, or a cable into the same router. A laptop on Wi-Fi and a desktop on cable work when the router bridges them, which home routers normally do.
3. **The router is not isolating devices.** Guest networks and the setting called "client isolation", "AP isolation" or "wireless isolation" stop devices from seeing each other on purpose. Use the main network, or turn the setting off in the router.
4. **There are not two routers.** A second router (or a mesh node in router mode) makes a second subnet. Swapdesk refuses peers that are not on-link, by design: it only accepts addresses in the subnet of the interface the packet arrived on, and sends with a hop limit of 1, so nothing can be routed in from outside the LAN. Put the second router in access point (bridge) mode.
5. **No VPN is on.** A VPN can move the default route or block local traffic. Swapdesk never listens on VPN or virtual interfaces (WSL, Hyper-V, Docker) at all.
6. **Windows treats the network as Private.** Settings > Network & internet > Wi-Fi (or Ethernet) > the network > Network profile type: Private. The firewall rules the installer adds cover Private and Domain networks only, never Public ones.
7. **No other firewall blocks it.** Third-party security suites often have their own firewall: allow UDP 47820 to 47883 and 5353 on the local network for the Swapdesk programs.
8. **mDNS is not blocked.** Some routers filter multicast. If discovery fails but the computers can ping each other, use "Add by address" with the other computer's IPv4 address (Windows: `ipconfig`; macOS: System Settings > Wi-Fi > Details).
9. **macOS allowed it.** On a Mac, the first run asks for Local Network access: allow it in System Settings > Privacy & Security > Local Network.

## Wake-on-LAN does not wake the computer

Wake-on-LAN must be on in the computer's firmware (BIOS or UEFI) and in the network adapter's settings (Windows: Device Manager > the adapter > Power Management and Advanced > Wake on Magic Packet). It works reliably over a cable; over Wi-Fi most computers cannot wake. A computer that was shut down (not asleep) wakes only when the firmware supports it.

## Does anything leave my network?

No. There is no account, no relay, no cloud, no telemetry and no update check over the internet. See [THREAT-MODEL.md](THREAT-MODEL.md).

## Can I use it over the internet?

Not in 1.x, not even through a VPN: Swapdesk accepts only peers on a physical Ethernet or Wi-Fi interface in the same subnet. Access from outside the LAN is a possible later direction (D36 in [DECISIONS.md](DECISIONS.md)), off by default if it comes. Until then, for access from elsewhere, use a tool built for that case.
