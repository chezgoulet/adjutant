# Localization: multiple languages, decided now

**Status:** decided (owner, 2026-09-24). Not yet implemented; this note fixes the shape so the
plugins and the client are built for it rather than retrofitted.
**Decides:** how language reaches the server, the plugins, the data and the client.

---

## 1. The decision

> "We should definitely support localization for multiple languages."

The troop is French-named and French-speaking in places, the lodge and newsletter are French, the
investiture carries Québécois ceremony, and the owner is moving to Quebec. English-only would be
a retrofit across every screen, every enum and every document template. So: **localization is a
first-class requirement, not a later feature.** English and French are the two we must support;
the design must not assume there will only ever be two.

Note the SPEC's existing "two primary languages" line is about *Rust and Dart*, not about human
languages — this document is the first decision on human language in the repo.

## 2. Where a string lives decides who translates it

| Kind of string | Owner | Mechanism |
|---|---|---|
| Client chrome (buttons, labels, empty states, errors) | Client | the client's own translation catalogue, shipped with the app |
| Plugin-supplied strings (screen titles, field labels, option names, error messages) | **Plugin** | the plugin serves translations with its metadata, keyed by locale |
| User data (names, lodge names, a mission's title, minutes) | The user | stored as written; never machine-translated |
| Governance documents (Accords, mission orders, templates) | The troop | edited per language as documents; not a translation catalogue problem |

That split keeps translation out of the core entirely, and it makes the plugin author responsible
for their own words — which is the only place they can be translated accurately.

## 3. What this changes in the client/plugin metadata contract

`client-and-plugin-ui.md` §3 already says the plugin supplies display strings and the client
supplies chrome. This decision adds the missing half:

- The client sends its locale (an `Accept-Language` header on metadata and data requests).
- A plugin's metadata response is **localized**: given a locale, the plugin returns that language,
  falling back to its default for anything untranslated.
- The client never translates a plugin's string itself, and never renders a missing translation as
  an empty label — fall back to the plugin's default language and, for an entirely unknown locale,
  to the client's.

## 4. What this changes on the server

- **Locale is a request property, not a server setting.** Each request carries the caller's
  preference; nothing about the server's language is global.
- **Enums and codes are stable identifiers, never display text.** A status stored as
  `bg_check_expired` is rendered in the user's language; a status stored as `"Expired"` is a
  translation bug waiting to happen. This applies to every plugin added from here.
- **Dates, names and formats** follow the caller's locale in the client; the server keeps ISO-8601
  and UTC. One timezone decision already exists in deployment; language does not change it.
- **Notifications** (see `background-check-tracking.md`) carry a locale so the delivery channel can
  render the message in the recipient's language.

## 5. What it costs, honestly

- Every screen and every plugin gets a translation file from its first version. Cheap now,
  expensive later — which is exactly why this note precedes the plugins.
- The client must ship two catalogues and a locale setting (plus "follow the device"), which is a
  small amount of work in the MVP and unbounded work if deferred.
- Governance documents are the real cost: the Accords exist in one language, and parallel texts are
  an editorial task for the troop, not a software one. The software should store them per language
  and never imply one is more authoritative than the other until the troop says which is.

## 6. Open questions

1. Which language is the *fallback* when a plugin has no translation for the caller? (Likely
   English for third-party plugins, French for the troop's own documents — but state it.)
2. Does the troop want **bilingual document storage** (both languages on the same record) or
   separate documents per language? The Accords' own text should settle this, not a developer.
3. Who authors the French for the first-party plugins — the owner, or a volunteer translator via a
   reviewable file? It changes whether translation is a build step or an editorial one.
