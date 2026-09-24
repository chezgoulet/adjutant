# OSG interoperability — deferred, and why

**Status:** deferred by the owner (2026-09-24). Not day-one, not milestone work.
**Trigger to revisit:** Adjutant runs independently and reliably for the troop, at which
point we approach OSG with a working system and ask whether they want a sync.
**Shape when it returns:** a **plugin**, not core work.

## The decision

> "We don't actually need sync and interop with OSG as day-one functionality. The data on the
> form does not get wholly entered into the OSG dashboard — they only keep a subset there. I
> say we revisit this much later once Adjutant works independently and approach OSG with a
> completely independently running system and then see whether they are interested in opening
> up some sort of syncing, which we would then develop as a plug-in." — owner, 2026-09-24

So: maintain the working CSV import as it stands, build nothing else against OSG for now, and
do not let OSG's shape influence the core. When the time comes, the integration is a plugin
with its own lifecycle — which is exactly what the plugin architecture is for.

## What was learned in the reconnaissance (kept so it is not redone)

Verified from public sources only — no login, no probing beyond public pages:

- **The dashboard exists and is the member account surface:**
  `outdoorserviceguides.org/dashboard` — a **Yii2/PHP** application behind **openresty**,
  session-cookie auth (`PHPSESSID` + Yii's `_csrf`), self-service sign-up and password reset,
  jQuery/Bootstrap. A separate app from their WordPress marketing site.
- **No public API surface**: no documented API, no OpenAPI, and it is **not an OIDC provider**.
  Identity federation with OSG is therefore not available; OSG can only ever be a *data*
  source. Local accounts stay.
- **Background checks are legally sealed.** OSG requires a yearly independent check on every
  adult and states it will "never reveal or make public the contents" of a criminal history
  check. The ceiling for any future sync is a *status*, never contents — and that is their
  legal obligation, not a preference.
- **The dashboard holds a subset** of the group's own registration data (owner's knowledge —
  the form is broader than the system). The public group registration form covers DOB,
  parent/guardian contacts, a separate emergency contact, dietary needs, medical
  considerations, medication authorisations, and liability/medical/COVID/photography waivers.
  That is a gap in Adjutant's own member model, independent of OSG.
- `robots.txt` is fully open and the marketing site is a normal WordPress install with a
  sitemap; nothing there exposes structured membership data.

## What this does not change

- The CSV importer stays as the baseline (SPEC §10), and remains a manual, operator-driven
  path.
- SPEC §10's "upgrade: API integration (when OSG provides programmatic access)" stays
  aspirational and is now explicitly **not scheduled**.
- The membership model gaps above (guardian contacts, emergency contact, dietary/medical,
  waivers) are real Adjutant work for its own sake — a troop needs them at a campout whether
  or not OSG ever syncs. Track them as domain work, not as interop.

## Why it stays documented rather than deleted

Because the reconnaissance cost real effort and the legal ceiling on background checks is a
constraint any future attempt must respect; and because "we looked, here is what we found, we
decided to wait" is a decision worth being able to point at when someone asks — including OSG.
