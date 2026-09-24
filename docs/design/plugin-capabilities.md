# Plugin capabilities: declared, host-mediated I/O beyond HTTP

**Status:** decision taken (owner, 2026-09-24). Design note; nothing implemented.
**Decides:** how a plugin reaches non-HTTP systems — the field/edge category (§9 of
[`client-and-plugin-ui.md`](client-and-plugin-ui.md)) — without a second process and without
weakening the plugin boundary.
**Blocks:** Tier B (ATAK, Meshcore). Not M4.

---

## 1. The decision

**An in-process plugin declares the capabilities it needs; the core mediates every one of them
through host calls, and the operator grants or withholds them.**

Chosen over the external-bridge alternative, and the owner's reason is the stronger one:
SPEC §2.6 says one server, one database, one binary. A bridge is a second thing to deploy,
monitor and update, and a troop with no IT volunteer should not have to run one. The
trade-off is a larger privilege surface inside the server process, which §5 addresses
honestly rather than hiding.

The technical dividend is the part that makes this more than a preference: **because the host
owns the socket, the same capability can be offered to a sandboxed guest as a host call.**
Mesh and TAK work becomes possible for a WASM plugin without giving it a socket — which no
external bridge can offer, and which keeps the isolation work intact.

---

## 2. The capabilities, derived from the two systems that need them

Not invented. MeshCore reaches its radio over **BLE, USB serial, or TCP** (a Wi-Fi companion);
ATAK speaks **CoT over TCP** and over **UDP multicast**.

| Capability | Shape | Needed by |
|---|---|---|
| `net.http.egress` | the existing `ctx.http`, now with a declared allowlist | everything; closes the review's SSRF gap |
| `net.tcp.connect` | outbound TCP to declared hosts/ports | TAK server; MeshCore Wi-Fi companion |
| `net.udp.multicast` | join a declared multicast group/port, send and receive | CoT over multicast |
| `serial.usb` | read/write a declared device path | MeshCore USB companion |
| `ble.central` | scan/connect to declared devices | MeshCore BLE companion |

Two of these are worth naming plainly because they are the sharp edges:

- **`serial.usb` is a filesystem capability in disguise.** A device path is a file; the
  allowlist is what keeps "write to /dev/ttyACM0" from becoming "write anywhere".
- **Multicast and LAN targets are legitimate.** A TAK server or a companion radio may be on a
  private range, so an egress policy cannot simply refuse internal addresses — it must be
  explicit per plugin, which is exactly what a declared capability is for.

---

## 3. The rules

1. **Declared in the manifest, before load.** A plugin states the capabilities it needs, like
   it states permissions. Undeclared use is refused.
2. **Deny by default; the operator grants.** Capabilities are withheld unless granted per plugin
   in config (the same shape as the `core.*` allowlist). Withholding a *required* capability
   fails the load — fail closed, as isolation does. Withholding an optional one is visible at
   boot and in `PluginInfo`.
3. **Host-mediated, never raw.** The plugin receives host calls such as
   `ctx.net.tcp_connect/read/write`, `ctx.net.udp_send/recv`, `ctx.serial_read/write` — never a
   file descriptor. The host owns the socket, bounds each call (size, timeout), and refuses
   endpoints outside the declaration. This is the same boundary that already keeps `sqlx` and
   `tokio` out of plugins.
4. **Visible.** Declared and granted capabilities appear in `PluginInfo`, in the admin surface,
   and in the boot log — so an operator can see that a plugin wants to open a TCP connection
   before it does.
5. **Bounded.** Per-call size and timeout limits; a connection budget per plugin; no unbounded
   receive queue. A capability that lets a plugin consume the server's fds is a DoS the core
   should bound, not discover later.

---

## 4. What this does and does not enforce — the honest part

**For a WASM guest it is a real boundary.** Guests have no sockets at all, so a host call is
the only way to the network, and the host checks the declaration and the allowlist every time.
A sandboxed plugin can therefore do mesh work without being able to scan the LAN.

**For a native plugin it is governance, not containment.** Native code runs in-process; it can
call `std::net::TcpStream` and open whatever it likes, declaration or no. The capability model
gives visibility, mediation by convention, and a single place to bound and log — it does not
stop a native plugin that intends harm. That is the same limit the isolation work states
plainly: native means trusted. Writing anything stronger in this document would be a lie, and
the honest version is still worth having, because the alternative today is a plugin opening
sockets with nobody able to see that it does.

If native containment is ever wanted, that is a different project (process isolation, a
sandboxed supervisor, or moving native plugins to WASM), and it is not required for ATAK or
MeshCore to work.

---

## 5. Operational consequences to weigh before Tier B

- **The radio or peer must be reachable from the server.** A USB companion plugged into the
  troop server, a Wi-Fi companion on the LAN, or a reachable TAK server. If the radio lives in
  a scout's pack, this design does not reach it — and the bridge model returns by necessity,
  not by preference. Worth knowing now rather than at implementation time.
- **`serial.usb` and `ble.central` are host-dependent.** They need the device present and the
  permissions to open it; the deployment docs will need a section, and a plugin must fail with
  a clear message when the device is absent rather than retrying forever.
- **Capability grants belong in the same operator surface as isolation.** If an operator must
  grep logs to learn what a plugin can reach, the model has failed at its only job for native
  code.

---

## 6. Sequence

1. **Not now.** This is Tier B; the open core issues and M4 come first.
2. The capability vocabulary reaches the manifest and the SDK, which is an **ABI change** — so
   it should land as its own brief with the SDK compatibility note, before any Tier B plugin is
   written against it.
3. The client's plugin `kind` (`ui` | `integration` | `bridge`) should be settled in the same
   pass, so the app renders configuration and status for a capability plugin instead of waiting
   for screens that will never exist.
4. `net.http.egress` deserves to land earlier than the rest: it is the smallest piece, it closes
   an open finding from the v0.2.0 review, and everything else can follow the pattern it sets.
