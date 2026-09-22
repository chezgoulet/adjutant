#!/usr/bin/env python3
"""M3 end-to-end proof. ADJUTANT_DEV_HEADERS=false → the x-dev-user stub is
OFF, so every identity below must come from the auth plugin's session provider.
Covers: bootstrap register, login/logout/me, admin user creation, role
enforcement, session-only access to another plugin's protected route,
membership CRUD + OSG CSV import, and a full OIDC flow against a mock IdP.
"""
import base64, hmac, hashlib, json, subprocess, sys, time, urllib.request, urllib.error
from http.server import BaseHTTPRequestHandler, HTTPServer
import threading

BASE = "http://127.0.0.1:8787"
IDP_PORT = 9099
ISSUER = f"http://127.0.0.1:{IDP_PORT}"
CLIENT_ID = "adjutant-e2e"
SECRET = "e2e-shared-secret"
results = []

def probe(name, method, path, body=None, headers=None, expect_status=None, expect_in=None, token=None):
    h = dict(headers or {})
    if token:
        h["Cookie"] = f"adjutant_session={token}"
    req = urllib.request.Request(BASE + path, method=method)
    for k, v in h.items():
        req.add_header(k, v)
    data = None
    if body is not None:
        data = json.dumps(body).encode()
        req.add_header("content-type", "application/json")
    try:
        with urllib.request.urlopen(req, data=data, timeout=10) as r:
            status, raw = r.status, r.read().decode()
    except urllib.error.HTTPError as e:
        status, raw = e.code, e.read().decode()
    except Exception as e:
        results.append((name, "FAIL", 0, f"transport: {e}"))
        return None
    ok, detail = True, raw[:150]
    if expect_status is not None and status != expect_status:
        ok, detail = False, f"status {status} != {expect_status} | {raw[:120]}"
    elif expect_in is not None and expect_in not in raw:
        ok, detail = False, f"missing {expect_in!r} | {raw[:120]}"
    results.append((name, "PASS" if ok else "FAIL", status, detail))
    print(f"{'PASS' if ok else 'FAIL':4} {status:>4}  {name}", flush=True)
    try:
        return json.loads(raw)
    except Exception:
        return None

def probe_raw(name, fn):
    """fn returns (ok, detail)."""
    try:
        ok, detail = fn()
    except Exception as e:
        ok, detail = False, f"exception: {e}"
    results.append((name, "PASS" if ok else "FAIL", "-", detail[:160]))
    print(f"{'PASS' if ok else 'FAIL':4}  -   {name}", flush=True)

# ---------------------------------------------------------------- mock IdP
def b64u(b):
    return base64.urlsafe_b64encode(b).rstrip(b"=").decode()

class IdP(BaseHTTPRequestHandler):
    def log_message(self, *args, **kwargs): pass
    def do_GET(self):
        if self.path.startswith("/.well-known/openid-configuration"):
            doc = {
                "issuer": ISSUER,
                "authorization_endpoint": f"{ISSUER}/authorize",
                "token_endpoint": f"{ISSUER}/token",
                "jwks_uri": f"{ISSUER}/jwks",
                "response_types_supported": ["code"],
                "id_token_signing_alg_values_supported": ["HS256"],
                "subject_types_supported": ["public"],
            }
            body = json.dumps(doc).encode()
            self.send_response(200)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        elif self.path.startswith("/jwks"):
            body = json.dumps({"keys": []}).encode()
            self.send_response(200)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        elif self.path.startswith("/authorize"):
            # browser leg: bounce to redirect_uri with code+state
            from urllib.parse import urlparse, parse_qs, urlencode
            q = parse_qs(urlparse(self.path).query)
            target = q.get("redirect_uri", [""])[0]
            sep = "&" if "?" in target else "?"
            loc = f"{target}{sep}" + urlencode({"code": "mock-auth-code", "state": q.get("state", [""])[0]})
            self.send_response(302)
            self.send_header("location", loc)
            self.end_headers()
        else:
            self.send_response(404); self.end_headers()
    def do_POST(self):
        if self.path.startswith("/token"):
            hdr = {"alg": "HS256", "typ": "JWT"}
            now = int(time.time())
            payload = {
                "iss": ISSUER, "sub": "idp-user-ada", "aud": CLIENT_ID,
                "exp": now + 3600, "iat": now,
                "email": "ada@idp.example.org", "name": "Ada from IdP",
            }
            signing = f"{b64u(json.dumps(hdr).encode())}.{b64u(json.dumps(payload).encode())}".encode()
            sig = b64u(hmac.new(SECRET.encode(), signing, hashlib.sha256).digest())
            id_token = f"{signing.decode()}.{sig}"
            body = json.dumps({"access_token": "at-123", "token_type": "Bearer", "id_token": id_token}).encode()
            self.send_response(200)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        else:
            self.send_response(404); self.end_headers()

srv = HTTPServer(("127.0.0.1", IDP_PORT), IdP)
threading.Thread(target=srv.serve_forever, daemon=True).start()
print(f"[e2e] mock IdP on {ISSUER}")

# ---------------------------------------------------------------- 0. reset
# The server must stay up (it owns the pool), so reset data instead of
# dropping the DB. TRUNCATE ... CASCADE clears sessions/user_roles too.
reset = subprocess.run(
    ["psql", "-h", "127.0.0.1", "-p", "5433", "-U", "adjutant", "-d",
     "adjutant_dev", "-c",
     "TRUNCATE core.users, core.audit_log, core.events, auth.oidc_states, "\
     "hello.greetings, hello.events_received, membership.members, "\
     "membership.lodges, membership.patrols, membership.proficiencies, "\
     "membership.member_proficiencies, membership.stewards RESTART IDENTITY CASCADE;"],
    capture_output=True, text=True)
if reset.returncode != 0:
    print(f"[e2e] reset failed: {reset.stderr.strip()}")
    sys.exit(2)
print("[e2e] data reset — starting from bootstrap state")

# ---------------------------------------------------------------- 1. bootstrap
r = probe("1 register bootstrap chief", "POST", "/api/auth/register",
          body={"username": "christopher", "password": "correct-horse-battery"},
          expect_status=201, expect_in='"bootstrap_role":"chief"')
chief_token = r and r.get("token")
assert chief_token, "bootstrap register failed — aborting"

r2 = probe("2 register closed after first user", "POST", "/api/auth/register",
           body={"username": "mallory", "password": "another-pass-9"}, expect_status=403)

# ---------------------------------------------------------------- 2. login / me / logout
probe("3 wrong password rejected", "POST", "/api/auth/login",
      body={"username": "christopher", "password": "WRONG"}, expect_status=400)
lr = probe("4 login ok", "POST", "/api/auth/login",
           body={"username": "christopher", "password": "correct-horse-battery"},
           expect_status=200, expect_in="token")
login_token = lr and lr.get("token")
assert login_token, "login failed"
probe("5 /me via session", "GET", "/api/auth/me", token=login_token,
      expect_status=200, expect_in='"roles":["chief"]')
probe("6 /me without session -> 401", "GET", "/api/auth/me", expect_status=401)

# ---------------------------------------------------------------- 3. session-only cross-plugin access
# No dev headers anywhere: identity must come from the session provider.
probe("7 session reaches hello:read (other plugin)", "GET", "/api/hello/greetings",
      token=login_token, expect_status=200, expect_in="greetings")
probe("8 anonymous denied on same route", "GET", "/api/hello/greetings", expect_status=401)

# ---------------------------------------------------------------- 4. admin user management
probe("9 chief creates scout", "POST", "/api/auth/users", token=chief_token,
      body={"username": "beatrice", "password": "shannons-scout-pass", "roles": ["scout"]},
      expect_status=201)
sr = probe("10 scout logs in", "POST", "/api/auth/login",
           body={"username": "beatrice", "password": "shannons-scout-pass"},
           expect_status=200, expect_in="token")
scout_token = sr and sr.get("token")
probe("11 scout /me shows scout role", "GET", "/api/auth/me", token=scout_token,
      expect_status=200, expect_in='"roles":["scout"]')
probe("12 scout denied auth:manage_users", "GET", "/api/auth/users",
      token=scout_token, expect_status=403)
probe("13 scout denied membership:read_all", "GET", "/api/membership/members",
      token=scout_token, expect_status=403)
probe("14 logout kills the session", "POST", "/api/auth/logout", token=login_token,
      expect_status=200)
probe("15 dead token rejected", "GET", "/api/auth/me", token=login_token, expect_status=401)

# ---------------------------------------------------------------- 5. membership (chief)
probe("16 create lodge", "POST", "/api/membership/lodge", token=chief_token,
      body={"name": "Tiguidou Lodge"}, expect_status=201, expect_in="Tiguidou")
probe("17 create patrol", "POST", "/api/membership/patrol", token=chief_token,
      body={"name": "Wolves", "lodge": "Tiguidou Lodge"}, expect_status=201, expect_in="Wolves")
probe("18 upsert member", "POST", "/api/membership/member", token=chief_token,
      body={"username": "ada", "display_name": "Ada Scout", "trail_name": "Spark",
            "patrol": "Wolves", "osg_id": "OSG-771", "bg_check": "clear"},
      expect_status=201)
probe("19 list members shows patrol+lodge", "GET", "/api/membership/members",
      token=chief_token, expect_status=200, expect_in="Wolves")
probe("20 member detail", "GET", "/api/membership/member?id=1", token=chief_token,
      expect_status=200, expect_in="Spark")
probe("21 create proficiency", "POST", "/api/membership/proficiency", token=chief_token,
      body={"code": "wilderness", "title": "Wilderness Survival"}, expect_status=201)
probe("22 complete proficiency", "POST", "/api/membership/proficiency/complete",
      token=chief_token, body={"member_id": 1, "proficiency_id": 1,
                               "signed_off_by": "christopher"},
      expect_status=201)
probe("23 detail shows completed proficiency", "GET", "/api/membership/member?id=1",
      token=chief_token, expect_status=200, expect_in="WILDERNESS")
probe("24 appoint steward", "POST", "/api/membership/steward", token=chief_token,
      body={"member_id": 1, "position": "Lodge Commander", "lodge": "Tiguidou Lodge"},
      expect_status=201)
probe("25 stewards list", "GET", "/api/membership/stewards", token=chief_token,
      expect_status=200, expect_in="Lodge Commander")

# ---------------------------------------------------------------- 6. OSG CSV import
csv = ("username,display_name,trail_name,patrol,email,roles\n"
       "sam,Sam Rivers,River,Wolves,sam@example.org,\n"
       "\"kai, jr\",Kai Junior,Kai,Wolves,kai@example.org,\n"
       ",No Username,,,\n"
       "lena,Lena Peak,Peak,Summit,lena@example.org,scout\n")
def csv_probe():
    req = urllib.request.Request(BASE + "/api/membership/import", method="POST",
                                 data=csv.encode())
    req.add_header("Cookie", f"adjutant_session={chief_token}")
    with urllib.request.urlopen(req, timeout=10) as r:
        raw = r.read().decode()
        body = json.loads(raw)
        ok = r.status == 200 and body.get("created") == 3 and len(body.get("skipped", [])) == 1
        return ok, f"status={r.status} body={raw[:140]}"
probe_raw("26 CSV import (3 created, 1 skipped)", csv_probe)
probe("27 imported patrol auto-created", "GET", "/api/membership/members",
      token=chief_token, expect_status=200, expect_in="Summit")
def csv_reimport():
    req = urllib.request.Request(BASE + "/api/membership/import", method="POST",
                                 data=csv.encode())
    req.add_header("Cookie", f"adjutant_session={chief_token}")
    with urllib.request.urlopen(req, timeout=10) as r:
        body = json.loads(r.read().decode())
        return r.status == 200 and body.get("updated", 0) + body.get("created", 0) == 3, \
               f"status={r.status} body={json.dumps(body)[:130]}"
probe_raw("28 re-import updates, not duplicates", csv_reimport)

# ---------------------------------------------------------------- 7. OIDC end-to-end
# Set per-plugin config, then hot-reload (M2 feature) so auth re-inits with it.
subprocess.run(["psql", "-h", "127.0.0.1", "-p", "5433", "-U", "adjutant",
                "-d", "adjutant_dev", "-tAc",
                f"UPDATE core.plugins SET config = "
                f"'{{\"oidc\": {{\"issuer\": \"{ISSUER}\", \"client_id\": \"{CLIENT_ID}\", "
                f"\"client_secret\": \"{SECRET}\", \"redirect_uri\": \"{BASE}/api/auth/oidc/callback\"}}}}' "
                f"WHERE id = 'auth';"], check=True, capture_output=True)
probe("29 hot-reload picks up oidc config", "POST", "/api/plugins/reload",
      token=chief_token, expect_status=200, expect_in='"auth"')

r = probe("30 oidc/login returns authorize URL", "GET",
          "/api/auth/oidc/login?redirect=0", expect_status=200, expect_in="authorize_url")
state = r and r.get("state")
assert state, f"no state from oidc/login — got: {r!r}"
assert ISSUER in (r or {}).get("authorize_url", "") and "/authorize" in r["authorize_url"], \
    f"authorize URL wrong: {r.get('authorize_url')}"

cb = probe("31 oidc/callback issues session", "GET",
           f"/api/auth/oidc/callback?code=mock-auth-code&state={state}",
           expect_status=200, expect_in="token")
oidc_token = cb and cb.get("token")
assert oidc_token, "no session from oidc callback"
oidc_me = probe("32 oidc identity in /me", "GET", "/api/auth/me", token=oidc_token,
      expect_status=200, expect_in="Ada from IdP")
probe("33 oidc user starts role-less -> denied", "GET", "/api/membership/members",
      token=oidc_token, expect_status=403)
# The IdP user must carry a local username, or username-keyed admin routes (role
# grants) can never reach them — read it back rather than assuming a value.
oidc_username = (oidc_me or {}).get("username", "")
assert oidc_username, f"IdP user has no local username: {oidc_me!r}"
probe("34 chief grants role to oidc user", "POST", "/api/auth/roles",
      token=chief_token, body={"username": oidc_username, "roles": ["chief"]},
      expect_status=200)
probe("35 granted role unlocks protected route", "GET", "/api/hello/greetings",
      token=oidc_token, expect_status=200, expect_in="greetings")

# replayed state must be single-use
probe("36 state is single-use", "GET",
      f"/api/auth/oidc/callback?code=mock-auth-code&state={state}",
      expect_status=400, expect_in="unknown or expired state")

# tamper-evident audit recorded the whole session
def audit_probe():
    req = urllib.request.Request(BASE + "/api/audit/verify")
    req.add_header("Cookie", f"adjutant_session={chief_token}")
    with urllib.request.urlopen(req, timeout=10) as r:
        body = json.loads(r.read().decode())
        return body.get("ok") is True, f"body={json.dumps(body)}"
probe_raw("37 audit chain intact after M3", audit_probe)

def audit_rows():
    out = subprocess.run(["psql", "-h", "127.0.0.1", "-p", "5433", "-U", "adjutant",
                          "-d", "adjutant_dev", "-tAc",
                          "SELECT action FROM core.audit_log ORDER BY id;"],
                         capture_output=True, text=True)
    acts = out.stdout.split()
    need = {"user.create", "auth.oidc.login", "steward.appoint", "membership.import"}
    missing = need - set(acts)
    return not missing, f"actions={acts} missing={sorted(missing)}"
probe_raw("38 audit captured admin actions", audit_rows)

srv.shutdown()
print()
npass = sum(1 for r in results if r[1] == "PASS")
for name, res, status, detail in results:
    print(f"{res:4} {status:>4}  {name:48} {detail}")
print(f"\n{npass}/{len(results)} E2E probes passed (dev headers OFF)")
sys.exit(0 if npass == len(results) else 1)
