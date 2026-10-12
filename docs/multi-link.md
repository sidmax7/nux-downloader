# Using several connections at once

Nux Downloader splits a download across every network connection it can
use, so two connections can download up to twice as fast. This page is
for people setting that up.

## When it helps

Each connection must reach the internet **by its own route**:

- Home Wi-Fi **and** a phone sharing its mobile data over USB: two
  routes, the speeds add up.
- Wi-Fi **and** Ethernet to the **same** router: one route. The router's
  internet line is the limit, and two connections to it add little.
- Two different internet providers (say fibre and a 5G router): two
  routes.

The **Links** page shows every connection the app found, with its live
speed. Untick **Use this link** to leave one out (a metered phone plan,
for instance), or give it a speed limit.

Networks that exist only inside your computer (Docker, virtual-machine
bridges) are left out on their own.

## Sharing a phone's connection over USB

- **Android:** connect the cable, then Settings → Network & internet →
  Hotspot & tethering → **USB tethering**.
- **iPhone:** Settings → Personal Hotspot → **Allow Others to Join**,
  then connect the cable and trust the computer.

Keep the phone's Wi-Fi off when its mobile data is the route you want;
otherwise the phone may share the same Wi-Fi you already use.

The new connection appears on the Links page within a few seconds.

### Linux

USB tethering shows up as a new interface (`usb0`, `enp0s20f0u1`, or
similar), usually configured automatically by NetworkManager.

### macOS

Android tethering needs a driver on macOS (for example HoRNDIS); an
iPhone works over USB without one. The connection appears as a new
service in System Settings → Network.

### Windows

Windows normally sends all traffic over one connection and may even turn
Wi-Fi off when a wired connection appears. If the second connection
disappears from the Links page:

1. Open the Group Policy editor (`gpedit.msc`, Pro and above) →
   Computer Configuration → Administrative Templates → Network →
   Windows Connection Manager → **Minimize the number of simultaneous
   connections to the Internet or a Windows Domain**, set it to
   **Disabled**, and restart.
2. On Home editions, the same setting is the registry value
   `fMinimizeConnections` = `0` (DWORD) under
   `HKEY_LOCAL_MACHINE\SOFTWARE\Policies\Microsoft\Windows\WcmSvc\GroupPolicy`.

## If a connection is greyed out or keeps dropping

A connection whose transfers keep failing is paused for a short while
and tried again; the Links page says so. Common causes: a captive portal
(hotel or café Wi-Fi) that wants a login in the browser, or a phone that
stopped tethering when its screen locked.
