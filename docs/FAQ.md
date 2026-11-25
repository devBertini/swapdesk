# FAQ

## The other computer does not show up

Swapdesk finds the other computer with DNS-SD (mDNS) on the local network, and with a direct search on UDP 47819 when it opens and when you press **Buscar**; it talks to it over UDP ports 47820 and 47821 (on a Windows computer with several accounts, each account has its own pair from there up, to 47882 and 47883). It works on any network (D93). Check these, in this order:

1. **Swapdesk is open on the other computer**, or its service is installed and running (Windows: `SwapdeskService` in Services).
2. **Both run an up-to-date Swapdesk.** A computer on 1.1.0 or older does not answer the direct search and refuses peers from another network.
3. **The router is not isolating devices.** Guest networks and the setting called "client isolation", "AP isolation" or "wireless isolation" stop devices from seeing each other on purpose. Use the main network, or turn the setting off in the router.
4. **It is on another network the search does not reach.** The search asks your own subnet, the common home ranges (192.168.0.x, 192.168.1.x, 10.0.0.x, 10.0.1.x) and the addresses you typed or paired with. For anything else use **Adicionar por endereço** with its IP once; from then on its network is searched too.
5. **A VPN is not blocking it.** A VPN can block local traffic. Swapdesk itself works over VPN and virtual interfaces too.
6. **The firewall lets it in.** The rules the Windows installer adds cover every network profile; another firewall (an antivirus suite, a company policy) may still block UDP 47819 to 47883.
7. **No other firewall blocks it.** Third-party security suites often have their own firewall: allow UDP 47820 to 47883 and 5353 on the local network for the Swapdesk programs.
8. **mDNS is not blocked.** Some routers filter multicast. If discovery fails but the computers can ping each other, use "Add by address" with the other computer's IPv4 address (Windows: `ipconfig`; macOS: System Settings > Wi-Fi > Details).
9. **macOS allowed it.** On a Mac, the first run asks for Local Network access: allow it in System Settings > Privacy & Security > Local Network.

## Wake-on-LAN does not wake the computer

Wake-on-LAN must be on in the computer's firmware (BIOS or UEFI) and in the network adapter's settings (Windows: Device Manager > the adapter > Power Management and Advanced > Wake on Magic Packet). It works reliably over a cable; over Wi-Fi most computers cannot wake. A computer that was shut down (not asleep) wakes only when the firmware supports it.

## Does anything leave my network?

Your sessions, screen, input and files never do: there is no account, no relay, no cloud and no telemetry. The only request beyond the LAN is the update check, once a day to github.com, with no identifier of you or the computer; turn it off in Settings > Updates. See [THREAT-MODEL.md](THREAT-MODEL.md).

## Can I use it over the internet?

Not in 1.x, not even through a VPN: Swapdesk accepts only peers on a physical Ethernet or Wi-Fi interface in the same subnet. Access from outside the LAN is a possible later direction (D36 in [DECISIONS.md](DECISIONS.md)), off by default if it comes. Until then, for access from elsewhere, use a tool built for that case.
