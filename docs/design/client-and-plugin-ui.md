# Client and plugin UI: generic screens from metadata

**Status:** decision taken (owner, 2026-09-24). Written before the first domain plugin so the
contract exists before anything depends on it.
**Decides:** how plugin-specific screens reach a user without plugin code reaching the client.

---

## 1. The decision

**The client renders generic screens from plugin-supplied metadata. Plugin code never executes
in the client.**

The widget vocabulary — tables, forms, field types, buttons, badges, detail cards — is
compiled into the Flutter app. A plugin declares *data* that selects and configures those
widgets. There is no dynamic code loading, no downloaded Dart, no webview.

Owner's summary, which is the rule:

> "The UI elements are already compiled into our Flutter app and the plugin's API metadata
> tells it what to render safely."

Why this and not the alternatives: Flutter compiles ahead of time, so a third-party or WASM
plugin cannot ship screens that the client loads at runtime. Hand-building every plugin's
screens in the client means a client release per plugin — workable for first-party plugins and
impossible for the community plugins the roadmap wants. A metadata vocabulary is the only
option that lets someone else's plugin have a usable interface without shipping a new app.

---

## 2. Division of responsibility

| Concern | Owner |
|---|---|
| Which widgets exist, how they look, theming, accessibility | **Client** (compiled in) |
| Which widgets appear on a screen, with what fields and labels | **Plugin** (metadata) |
| Whether an action is permitted | **Server** — always |
| Validation that matters | **Server** — always |
| Validation for a nicer experience (required, format) | Client, as a courtesy only |

**Rendering is not authorisation.** Hiding a button is UX; the route gate and the scope check
are the security boundary (#34). A client that rendered every button would change nothing about
what a user can actually do — and that is the property that makes metadata safe.

---

## 3. The vocabulary (v1 sketch)

Deliberately small. Everything is data; unknown keys are ignored, unknown widget *types* fall
back (see §5).

- **Screens:** `list` (collection with columns, filters, pagination), `detail` (field/value
  groups plus actions), `form` (create/edit), `dashboard` (cards of other screens).
- **Field types:** text, long text, number, boolean, date, datetime, select (with options),
  reference (another screen's list), badge (status with a colour), money, file/image.
- **Actions:** named, with a target **declared by the plugin itself** (see §4), a method, an
  optional confirm prompt, and the permission the caller must hold — declared so the client can
  hide what will be refused, never relied upon.
- **Labels:** the plugin supplies display strings; the client supplies chrome (buttons, empty
  states, errors) and its own translations. Localisation of plugin strings is the plugin's.

A plugin serves its metadata on its own route (e.g. `GET /api/<plugin>/_ui`), versioned.

---

## 4. Safety rules the client must enforce

1. **No arbitrary navigation.** An action may only target a route under the plugin's own
   registered namespace. Metadata cannot express an external URL, a `javascript:` target, or a
   path belonging to another plugin.
2. **No expression language.** Metadata is inert data. No formulas, no conditionals beyond
   simple visibility flags, no templates that the client evaluates.
3. **Unknown type → safe fallback.** An unrecognised widget renders as a labelled placeholder
   ("this screen needs a newer version of the app"), never as raw content and never as
   nothing. Metadata is untrusted input from the client's point of view — including from a
   buggy first-party plugin.
4. **Strings are strings.** No HTML rendering, no rich text from plugins in v1.
5. **The client sends the same requests a human would.** No backend-only shortcuts through
   metadata; every action is an ordinary API call subject to the gate.

---

## 5. Versioning — the part that is easy to get wrong

The server is self-hosted and updates whenever the operator updates it. The client is installed
on a phone and updates when the user updates it. **They will drift, in both directions.**

- The client advertises the **metadata vocabulary version** it supports on every request.
- The plugin (via the core) declares the **minimum version** its metadata needs.
- If the client is older than the metadata needs: the client renders the fallback for the parts
  it cannot express, and says so — never a half-rendered screen that looks broken.
- If the client is newer: it ignores unknown keys and renders what it understands.

This is the client-side analogue of the SDK ABI handshake, and it deserves the same rigour: an
unknown-widget fallback that is tested, not assumed.

---

## 6. Offline-first (SPEC §6.3) makes this *easier*

Metadata-driven screens and a local database fit together: the client caches the data *and* the
metadata, renders from cache, and queues mutations for the server to accept or refuse. Because
the plugin's logic never left the server, there is no client-side copy of business rules to
keep in sync — the client is a view and a queue. With arbitrary plugin code in the client, none
of this would hold.

---

## 7. Out of scope for the vocabulary (hand-built screens)

Anything genuinely custom stays a first-party client screen. The examples below illustrate the
*shape* of what a generic renderer cannot express — they are not a feature list, and at least
one of them (drag-to-reschedule) the owner has said we do not need:

- interfaces with continuous spatial state (map and situational views)
- flows whose interaction *is* the layout (drag-to-reorder, freehand annotation, device-first
  offline field logging)
- anything needing a sensor, a camera or a radio

Third-party plugins get the vocabulary, and that is the honest ceiling of a generic renderer —
say so in the plugin guide rather than implying otherwise. If a capability is genuinely needed
and does not fit, the answer is to add a **widget to the client**, not to let a plugin ship
code.

---

## 8. Open questions

1. **Media first or later?** Equipment photos and archive scans want file/image fields. If v1
   omits them, plugins will each invent an upload flow; if v1 includes them, the client MVP
   grows. Recommendation: include a minimal image field, defer anything richer.
2. **Do plugins need kinds?** A UI plugin declares screens. Some plugins in the inventory are
   not that at all — ATAK and Meshcore are device ecosystems we exchange *data* with, where the
   interaction happens in the other system and Adjutant's role is configuration, status and
   relay. See §9. If that category is real, the client's metadata should carry a
   `kind` (`ui` | `integration` | `bridge`) so the app renders configuration and status for a
   bridge instead of waiting for screens that will never come.
3. **Does a plugin ever need a custom view?** If yes, the only sandbox-compatible escape is a
   *widget the client already implements* being configured unusually — not plugin-supplied
   rendering. Confirm we are content with that ceiling.
4. **Who owns the vocabulary's evolution?** It is a compatibility surface with the same
   discipline as `SDK_ABI_VERSION`; the first client release should freeze v1 and treat
   additions as additive.

---

## 9. Field and edge systems need their own category (owner's observation, 2026-09-24)

> "Certain plugins such as ATAK and Meshcore might need to be in their own category. We'll have
> to think that through."

They do, and the reason is structural rather than editorial. The plugin model as built assumes
a **server-side capability behind host-mediated I/O**, and two of its rules are deliberately
unfriendly to field hardware:

- **All I/O is host-mediated and HTTP-shaped.** `ctx.http` is the only egress a plugin has, and
  the v0.2.0 review already noted it carries no allowlist. ATAK speaks the TAK protocol
  (CoT over TCP/multicast); Meshcore speaks a LoRa mesh. Neither is HTTP.
- **Sandboxed guests have no sockets at all** — that is a feature of the isolation work, not an
  oversight, and it should not be relaxed per-plugin on request.

Two shapes are available, and this is a decision, not an implementation detail:

- **A. In-process plugin with new declared capabilities** — the core grows a capability model
  (protocol plus host/network allowlist, operator-approved per plugin) and a native plugin uses
  it. Powerful, and it would also close the SSRF gap by giving `http.request` an egress policy.
  Cost: real privilege surface inside the server process.
- **B. External bridge process** — a small service at the edge (the Pi with the LoRa HAT, the
  tablet running ATAK) authenticates as a service identity and exchanges data over the HTTP API
  and the event bus. No new core capability, no new privilege, sandbox story untouched. Cost:
  something to deploy and monitor at the edge.

**Recommendation: B as the default, with A only if a protocol genuinely cannot be bridged.** It
keeps the server's privilege surface where it is, fits the event bus (mesh and TAK traffic is
event-shaped, not screen-shaped), and matches the reality that Meshcore is a device ecosystem
rather than a server library. This deserves its own design note before Tier B is briefed — and
the `kind` question in §8 #2 should be settled at the same time, since a bridge has no screens
and the client should not pretend otherwise.

---

## 9. What happens next

Missions is the first plugin written under this contract, so the practical sequence is:

1. The client MVP (M5 per the roadmap) implements a first version of the vocabulary.
2. Missions and the other M4 plugins serve metadata for their screens **from their first
   version**, rather than retrofitting it later across nine plugins.
3. The plugin guide gains the vocabulary and the safety rules in §4 as author-facing
   requirements.
