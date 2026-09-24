#!/usr/bin/env python3
"""Committed probe harness for the M1 and M2 exit criteria (SPEC §15).

Replaces the ad-hoc, uncommitted `m2_probes_*.json` transcripts that the M2
milestone document cited. Every probe asserts STATUS and BODY independently
(the old harness used an `elif`, so body assertions were never evaluated), and
the script exits non-zero on any failure — the tally printed is derived from the
recorded results, never typed by hand.

Run:  python3 scripts/probes.py [--json docs/evidence/m2_probes.json]

Prerequisites (checked, not assumed):
  * `cargo build --workspace` has produced target/debug/adjutant
  * PostgreSQL reachable at $ADJUTANT_DATABASE_URL; the name MUST end in `_test`,
    because this harness drops and recreates it (the same guard as `adjutant
    test-plugin`) and mutates it even under --no-reset
  * port 8787 free

Batches: m1_regression, middleware, lifecycle, tamper — matching the M2 doc's
"four batches". Phase 2 restarts the server with the rate limiter enabled and
JSON logging so the middleware claims are exercised on the same binary.
"""
import argparse
import json
import os
import re
import shutil
import socket
import signal
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def workspace_version() -> str:
    """The `[workspace.package]` version, so this probe tracks releases instead
    of hardcoding one (a version bump must not fail the ladder)."""
    text = (ROOT / "Cargo.toml").read_text()
    m = re.search(r'\[workspace\.package\][^\[]*?version\s*=\s*"([^"]+)"', text, re.S)
    return m.group(1) if m else ""


DSN = os.environ.get("ADJUTANT_DATABASE_URL", "postgres://adjutant@127.0.0.1:5433/adjutant_dev")
BIND = "127.0.0.1:8787"
BASE = f"http://{BIND}"
PLUGIN_DIR = ROOT / ".probe-plugins"
LOG = Path("/tmp/adjutant-probes.log")
CHIEF = {"x-dev-user": "christopher", "x-dev-role": "chief"}
SCOUT = {"x-dev-user": "beatrice", "x-dev-role": "scout"}
results = []


# --------------------------------------------------------------------------- io
def http(method, path, headers=None, body=None, timeout=10):
    req = urllib.request.Request(BASE + path, method=method)
    for k, v in (headers or {}).items():
        req.add_header(k, v)
    data = None
    if body is not None:
        data = json.dumps(body).encode()
        req.add_header("content-type", "application/json")
    try:
        with urllib.request.urlopen(req, data=data, timeout=timeout) as r:
            return r.status, r.read().decode(), dict(r.headers)
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode(), dict(e.headers)


def psql(sql, db=None):
    url = DSN if db is None else re.sub(r"/[^/]+$", "/" + db, DSN)
    p = subprocess.run(["psql", url, "-tAc", sql], capture_output=True, text=True, timeout=30)
    return p.returncode, p.stdout.strip(), p.stderr.strip()


def require_test_database():
    """Refuse to run against a non-`_test` database.

    Mirrors `adjutant test-plugin`'s guard: this harness drops, recreates, and
    mutates whatever `ADJUTANT_DATABASE_URL` names, so pointing it at the live
    dev database (the old default) is a data-loss footgun.
    """
    dbname = DSN.rsplit("/", 1)[-1].split("?")[0]
    if not dbname.endswith("_test"):
        sys.exit(
            f"refusing to run against {dbname!r}: ADJUTANT_DATABASE_URL must name a "
            f"database ending in _test (e.g. .../adjutant_dev_test)"
        )


def reset_db():
    dbname = DSN.rsplit("/", 1)[-1].split("?")[0]
    p = subprocess.run(
        ["psql", re.sub(r"/[^/]+$", "/postgres", DSN), "-tAc",
         f"SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '{dbname}' AND pid <> pg_backend_pid()"],
        capture_output=True, text=True)
    if p.returncode != 0:
        return False, p.stderr.strip()
    for stmt in (f'DROP DATABASE IF EXISTS "{dbname}"', f'CREATE DATABASE "{dbname}"'):
        p = subprocess.run(["psql", re.sub(r"/[^/]+$", "/postgres", DSN), "-c", stmt],
                           capture_output=True, text=True)
        if p.returncode != 0:
            return False, p.stderr.strip()
    return True, "database recreated"


# ------------------------------------------------------------------- recording
def check(batch, name, ok, detail):
    results.append({"batch": batch, "probe": name, "result": "PASS" if ok else "FAIL", "detail": detail})
    print(f"{'PASS' if ok else 'FAIL':4}  {batch:14} {name}", flush=True)


def probe(batch, name, method, path, status=None, contains=None, headers=None, body=None):
    got, raw, _ = http(method, path, headers=headers, body=body)
    errs = []
    if status is not None and got != status:
        errs.append(f"status {got} != {status}")
    if contains is not None and contains not in raw:
        errs.append(f"body missing {contains!r}")
    check(batch, name, not errs, (f"{got} | {raw[:110]}" if not errs else "; ".join(errs) + f" | {raw[:110]}"))
    return got, raw


def probe_sql(batch, name, sql, want, db=None):
    rc, out, err = psql(sql, db=db)
    ok = rc == 0 and out == want
    check(batch, name, ok, f"rc={rc} out={out!r} err={err[:80]!r}")


def probe_sql_contains(batch, name, sql, needle, db=None):
    rc, out, err = psql(sql, db=db)
    ok = rc == 0 and needle in out
    check(batch, name, ok, f"rc={rc} out={out[:100]!r} err={err[:80]!r}")


# ------------------------------------------------------------------ server mgmt
def start_server(extra_env, log_path):
    env = dict(os.environ)
    env.update({
        "ADJUTANT_BIND": BIND,
        "ADJUTANT_PLUGIN_DIR": str(PLUGIN_DIR),
        "ADJUTANT_DATABASE_URL": DSN,
        "ADJUTANT_DEV_HEADERS": "true",
        "ADJUTANT_CORS": "*",
        "ADJUTANT_LOG": "info,adjutant_server=debug",
    })
    env.update(extra_env)
    fh = open(log_path, "w")
    proc = subprocess.Popen([str(ROOT / "target/debug/adjutant")], env=env, stdout=fh, stderr=fh)
    # Readiness via a raw TCP connect: an HTTP GET here would consume a
    # rate-limit slot and make the 429 probes off-by-one.
    host, port = BIND.split(":")
    for _ in range(100):
        if proc.poll() is not None:
            raise SystemExit(f"server exited early (code {proc.returncode}); see {log_path}")
        try:
            with socket.create_connection((host, int(port)), timeout=1):
                return proc
        except OSError:
            time.sleep(0.1)
    proc.kill()
    raise SystemExit(f"server did not become ready; see {log_path}")


def stop_server(proc):
    proc.send_signal(signal.SIGTERM)
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=5)


# ---------------------------------------------------------------------- batches
def batch_m1_regression():
    b = "m1_regression"
    probe(b, "1 health", "GET", "/", status=200, contains='"adjutant"')
    # /api/plugins is admin (SPEC §5.3): anonymous must be refused, chief allowed.
    probe(b, "2a registry anonymous refused", "GET", "/api/plugins", status=401)
    _, raw = probe(b, "2b registry as chief", "GET", "/api/plugins", status=200, contains='"hello"', headers=CHIEF)
    try:
        info = json.loads(raw)["plugins"][0]
        # Relational, not literal: the route count must agree with the route table
        # the registry reports, so adding a route cannot silently break the probe.
        ok = (
            info["routes"] == len(info["route_list"])
            and sorted(info["permissions"]) == ["hello:read", "hello:write"]
            and info["version"] == workspace_version()
        )
        check(b, "2c registry metadata", ok, json.dumps({k: info[k] for k in ("id", "version", "routes", "permissions")}))
    except Exception as e:  # noqa: BLE001
        check(b, "2c registry metadata", False, f"parse: {e}")
    probe(b, "3 open route", "GET", "/api/hello", status=200, contains="Hello, Adjutant!")
    probe(b, "4 protected no identity", "GET", "/api/hello/greetings", status=401, contains="authentication required")
    probe(b, "5 scout lacks permission", "GET", "/api/hello/greetings", status=403,
          contains="insufficient permissions", headers=SCOUT)
    probe(b, "6 chief reads", "GET", "/api/hello/greetings", status=200, contains="greetings", headers=CHIEF)
    probe(b, "7 chief writes", "POST", "/api/hello/greet", status=201, contains="first greeting",
          headers=CHIEF, body={"message": "first greeting"})
    _, raw = probe(b, "8 event persisted", "GET", "/api/events/recent", status=200,
                   contains="hello.greeted", headers=CHIEF)
    try:
        ev = json.loads(raw)
        check(b, "8b cursor present", isinstance(ev.get("cursor"), int) and ev["cursor"] >= 1, f"cursor={ev.get('cursor')}")
    except Exception as e:  # noqa: BLE001
        check(b, "8b cursor present", False, f"parse: {e}")
    probe(b, "9 greeting readable", "GET", "/api/hello/greetings", status=200,
          contains="first greeting", headers=CHIEF)
    # Path capture, end to end: template match -> param -> handler -> DB.
    probe(b, "9b path capture read", "GET", "/api/hello/greetings/1", status=200,
          contains="first greeting", headers=CHIEF)
    probe(b, "9b2 percent-encoded capture decodes", "GET", "/api/hello/greetings/%31", status=200,
          contains="first greeting", headers=CHIEF)
    probe(b, "9c path capture prefers literal", "GET", "/api/hello/greetings", status=200,
          contains="greetings", headers=CHIEF)
    probe(b, "9d non-numeric capture is a 400", "GET", "/api/hello/greetings/abc", status=400,
          contains="must be a number", headers=CHIEF)
    probe(b, "9e extra segment is 404", "GET", "/api/hello/greetings/1/extra", status=404,
          contains="route not found", headers=CHIEF)
    probe_sql(b, "10 event delivered to subscriber", "SELECT count(*) FROM hello.events_received WHERE event_type='hello.greeted'", "1")
    probe_sql(b, "audit logged the write", "SELECT count(*) FROM core.audit_log WHERE action='greet'", "1")


def batch_middleware_phase1():
    b = "middleware"
    got, raw, hdrs = http("GET", "/api/hello")
    rid = next((v for k, v in hdrs.items() if k.lower() == "x-request-id"), None)
    check(b, "M1 x-request-id present", bool(rid) and bool(re.match(r"^[0-9a-f]+-[0-9a-f]+$", rid or "")), f"rid={rid}")
    cors = next((v for k, v in hdrs.items() if k.lower() == "access-control-allow-origin"), None)
    check(b, "M2 CORS allow-origin on 200", cors == "*", f"allow-origin={cors}")
    # replay cursor semantics
    _, raw = probe(b, "M4 replay since=0", "GET", "/api/events/recent?since=0&limit=1", status=200,
                   contains='"cursor"', headers=CHIEF)
    try:
        ev = json.loads(raw)
        ok = len(ev["events"]) == 1 and ev["cursor"] == ev["events"][0]["id"]
        check(b, "M4b cursor equals last id", ok, json.dumps(ev)[:140])
    except Exception as e:  # noqa: BLE001
        check(b, "M4b cursor equals last id", False, f"parse: {e}")
    probe(b, "M5 replay since=999 empty", "GET", "/api/events/recent?since=999", status=200,
          contains='"events": []'.replace(" ", ""), headers=CHIEF)
    probe(b, "M6 scout cannot disable plugins", "POST", "/api/plugins/hello/disable", status=403,
          contains="insufficient permissions", headers=SCOUT)
    probe(b, "M7 anonymous cannot disable plugins", "POST", "/api/plugins/hello/disable", status=401,
          contains="authentication required")


def plugins_json():
    _, raw, _ = http("GET", "/api/plugins", headers=CHIEF)
    try:
        return json.loads(raw)
    except Exception:  # noqa: BLE001
        return {}


def batch_lifecycle():
    b = "lifecycle"
    probe(b, "L1 disable", "POST", "/api/plugins/hello/disable", status=200, contains='"enabled":false', headers=CHIEF)
    # Disable must stop the event handlers too, not just the routes.
    bound = plugins_json().get("bound_subscriptions", [])
    check(b, "L1b disable aborts subscriptions", "hello" not in bound, f"bound={bound}")
    probe(b, "L2 disabled route 404", "GET", "/api/hello", status=404, contains="plugin hello is disabled")
    probe(b, "L3 core routes unaffected", "GET", "/", status=200, contains='"ok"')
    probe_sql(b, "L4 db enabled=false", "SELECT enabled FROM core.plugins WHERE id='hello'", "f")
    probe(b, "L5 enable", "POST", "/api/plugins/hello/enable", status=200, contains='"enabled":true', headers=CHIEF)
    bound = plugins_json().get("bound_subscriptions", [])
    check(b, "L5b enable rebinds subscriptions", "hello" in bound, f"bound={bound}")
    probe(b, "L6 route works again", "GET", "/api/hello", status=200, contains="Hello, Adjutant!")
    probe_sql(b, "L7 data present before uninstall", "SELECT count(*) FROM hello.greetings", "1")
    probe(b, "L8 uninstall", "DELETE", "/api/plugins/hello", status=200,
          contains='"uninstalled":true', headers=CHIEF)
    probe(b, "L9 uninstalled route 404", "GET", "/api/hello", status=404, contains="route not found")
    probe_sql(b, "L10 rows archived", "SELECT count(*) FROM hello.greetings", "1")
    probe_sql_contains(b, "L11 schema archived", "SELECT count(*) FROM information_schema.schemata WHERE schema_name='hello'", "1")
    probe_sql(b, "L12 db uninstalled=true", "SELECT uninstalled FROM core.plugins WHERE id='hello'", "t")
    _, raw = probe(b, "L13 retired library tracked", "GET", "/api/plugins", status=200, headers=CHIEF)
    try:
        ok = json.loads(raw)["retired_libraries"] >= 1 and not any(p["id"] == "hello" for p in json.loads(raw)["plugins"])
        check(b, "L13b hello absent from live registry", ok, raw[:140])
    except Exception as e:  # noqa: BLE001
        check(b, "L13b hello absent from live registry", False, f"parse: {e}")
    _, raw = probe(b, "L14 reload skips uninstalled", "POST", "/api/plugins/reload", status=200, headers=CHIEF)
    try:
        ok = json.loads(raw).get("reloaded") == []
        check(b, "L14b reloaded list empty", ok, raw[:140])
    except Exception as e:  # noqa: BLE001
        check(b, "L14b reloaded list empty", False, f"parse: {e}")
    probe_sql(b, "L15 clear uninstalled flag", "UPDATE core.plugins SET uninstalled=false WHERE id='hello'", "UPDATE 1")
    _, raw = probe(b, "L16 reload reinstalls", "POST", "/api/plugins/reload", status=200, contains='"hello"', headers=CHIEF)
    probe(b, "L17 route works after reinstall", "GET", "/api/hello", status=200, contains="Hello, Adjutant!")
    probe_sql(b, "L18 data still archived", "SELECT count(*) FROM hello.greetings", "1")
    before = psql("SELECT count(*) FROM hello.events_received")[1]
    probe(b, "L19 subscriptions rebound (publish)", "POST", "/api/hello/greet", status=201,
          headers=CHIEF, body={"message": "post-reload"})
    deadline = time.time() + 5
    while time.time() < deadline:
        if psql("SELECT count(*) FROM hello.events_received")[1] != before:
            break
        time.sleep(0.1)
    after = psql("SELECT count(*) FROM hello.events_received")[1]
    check(b, "L19b subscriber received post-reload event", int(after) == int(before) + 1, f"{before} -> {after}")


def batch_tamper():
    b = "tamper"
    _, _, err = psql("UPDATE core.audit_log SET action='tampered' WHERE id=1")
    check(b, "T1 UPDATE rejected", "append-only" in err, err[:120])
    _, _, err = psql("DELETE FROM core.audit_log WHERE id=1")
    check(b, "T2 DELETE rejected", "append-only" in err, err[:120])
    # TRUNCATE does not fire row-level triggers; without the statement-level guard
    # the whole chain can be erased and audit_verify then reports a healthy empty log.
    _, _, err = psql("TRUNCATE core.audit_log")
    check(b, "T2b TRUNCATE rejected", "append-only" in err, err[:120])
    probe_sql(b, "T3 row untouched", "SELECT action FROM core.audit_log WHERE id=1", "greet")
    _, raw = probe(b, "T4 chain verifies", "GET", "/api/audit/verify", status=200,
                   contains='"ok":true', headers=CHIEF)
    try:
        ok = json.loads(raw)["rows_checked"] > 0
        check(b, "T4b rows actually checked", ok, raw[:140])
    except Exception as e:  # noqa: BLE001
        check(b, "T4b rows actually checked", False, f"parse: {e}")
    psql("ALTER TABLE core.audit_log DISABLE TRIGGER audit_append_only")
    rc, out, err = psql("UPDATE core.audit_log SET action='tampered' WHERE id=1")
    check(b, "T5 superuser bypass lands", rc == 0 and out == "UPDATE 1", f"rc={rc} err={err[:80]}")
    _, raw = probe(b, "T6 verifier DETECTS tamper", "GET", "/api/audit/verify", status=200,
                   contains='"ok":false', headers=CHIEF)
    try:
        ok = json.loads(raw)["first_bad"] == 1
        check(b, "T6b first_bad names row 1", ok, raw[:140])
    except Exception as e:  # noqa: BLE001
        check(b, "T6b first_bad names row 1", False, f"parse: {e}")
    probe_sql(b, "T7 restore", "UPDATE core.audit_log SET action='greet' WHERE id=1", "UPDATE 1")
    psql("ALTER TABLE core.audit_log ENABLE TRIGGER audit_append_only")
    probe(b, "T8 chain verifies again", "GET", "/api/audit/verify", status=200, contains='"ok":true', headers=CHIEF)
    _, _, err = psql("UPDATE core.audit_log SET action='tampered' WHERE id=1")
    check(b, "T9 guard re-armed", "append-only" in err, err[:120])


def batch_middleware_phase2():
    b = "middleware2"
    codes = []
    for _ in range(9):
        s, raw, hdrs = http("GET", "/api/hello")
        codes.append(s)
    first_eight_ok = all(c == 200 for c in codes[:8])
    ninth = codes[8]
    check(b, "M3 ninth request 429", first_eight_ok and ninth == 429, f"codes={codes}")
    s, raw, hdrs = http("GET", "/api/hello")
    ra = next((v for k, v in hdrs.items() if k.lower() == "retry-after"), None)
    cors = next((v for k, v in hdrs.items() if k.lower() == "access-control-allow-origin"), None)
    rid = next((v for k, v in hdrs.items() if k.lower() == "x-request-id"), None)
    check(b, "M3b retry-after set", ra == "60", f"retry-after={ra}")
    check(b, "M3c CORS header survives 429", cors == "*", f"allow-origin={cors}")
    check(b, "M3d request-id on 429", bool(rid), f"rid={rid}")
    # JSON logging (server started with ADJUTANT_LOG_FORMAT=json)
    line = None
    for _ in range(50):
        txt = LOG.read_text() if LOG.exists() else ""
        for cand in reversed(txt.splitlines()):
            if '"target"' in cand and '"fields"' in cand:
                line = cand
                break
        if line:
            break
        time.sleep(0.1)
    if line is None:
        check(b, "M4 JSON log line", False, "no JSON log line found")
    else:
        try:
            doc = json.loads(line)
            ok = "timestamp" in doc and "level" in doc and "fields" in doc and "target" in doc
            check(b, "M4 JSON log line", ok, line[:140])
        except Exception as e:  # noqa: BLE001
            check(b, "M4 JSON log line", False, f"parse: {e}")


# -------------------------------------------------------------------------- main
def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--json", default="docs/evidence/m2_probes.json")
    ap.add_argument("--no-reset", action="store_true")
    args = ap.parse_args()

    if not (ROOT / "target/debug/adjutant").exists():
        sys.exit("target/debug/adjutant missing — run `cargo build --workspace` first")
    require_test_database()
    if not args.no_reset:
        ok, detail = reset_db()
        print(f"[probes] {detail}")
        if not ok:
            sys.exit(f"database reset failed: {detail}")

    if PLUGIN_DIR.exists():
        shutil.rmtree(PLUGIN_DIR)
    PLUGIN_DIR.mkdir()
    for so in ["libadjutant_hello.so"]:
        src = ROOT / "target/debug" / so
        if not src.exists():
            sys.exit(f"missing {src} — run `cargo build --workspace` first")
        shutil.copy2(src, PLUGIN_DIR / so)
    print(f"[probes] staged {len(list(PLUGIN_DIR.iterdir()))} plugin(s) into {PLUGIN_DIR}")

    phase1 = start_server({"ADJUTANT_RATE_MAX": "0"}, LOG)
    try:
        batch_m1_regression()
        batch_middleware_phase1()
        batch_lifecycle()
        batch_tamper()
    finally:
        stop_server(phase1)

    phase2 = start_server({"ADJUTANT_RATE_MAX": "8", "ADJUTANT_RATE_WINDOW": "60", "ADJUTANT_LOG_FORMAT": "json"}, LOG)
    try:
        batch_middleware_phase2()
    finally:
        stop_server(phase2)

    passed = sum(1 for r in results if r["result"] == "PASS")
    out = Path(args.json)
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps({
        "generated_by": "scripts/probes.py",
        "database": DSN,
        "batches": sorted({r["batch"] for r in results}),
        "passed": passed,
        "total": len(results),
        "results": results,
    }, indent=1) + "\n")
    print(f"\n{passed}/{len(results)} probes passed — transcript: {out}")
    for r in results:
        if r["result"] != "PASS":
            print(f"  FAIL {r['batch']}/{r['probe']}: {r['detail']}")
    sys.exit(0 if passed == len(results) else 1)


if __name__ == "__main__":
    main()
