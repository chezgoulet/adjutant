#!/usr/bin/env python3
"""M4 end-to-end proof: the Accords' mission and governance pathways, live.

**Precondition:** a server is already running against a bootstrapped database,
with the dev-header stub ON (this harness holds a `chief` identity through
`x-dev-user`/`x-dev-role`; M3's harness is the identity proof — this one is the
domain proof), rate limiting effectively off, and the missions + governance
plugins staged in its plugin directory:

    ADJUTANT_PLUGIN_DIR=plugins-built ADJUTANT_DEV_HEADERS=true \\
    ADJUTANT_RATE_MAX=0 ADJUTANT_LOG=warn ./target/debug/adjutant

Base URL: `ADJUTANT_E2E_BASE` (default http://127.0.0.1:8787).

What it proves, end to end and against a real PostgreSQL:

* the six-stage mission lifecycle (Art 8), including the guards that stop an
  out-of-order transition and the appeal a Lodge Commander's rejection can take
  to the Troop Council;
* mentor matching, milestones, execution progress, debrief, report, and the
  cumulative Impact Report;
* the motion lifecycle with a second, debate, per-voter votes recorded once,
  quorum, a tally, and implementation;
* friendly and formal amendments, and the fact that an accepted one changes the
  motion's text;
* minutes drafted from the record and adopted;
* Accords versioning from a **passed Congress motion** only;
* that `mission.completed` and `motion.passed` reached the event bus.

Exit code 0 only if every probe passed.
"""
import json
import os
import sys
import urllib.error
import urllib.request

BASE = os.environ.get("ADJUTANT_E2E_BASE", "http://127.0.0.1:8787")
results = []


def call(method, path, body=None, user="bea", role="chief"):
    """One request, as `user` holding `role`. Returns (status, decoded)."""
    req = urllib.request.Request(BASE + path, method=method)
    req.add_header("x-dev-user", user)
    req.add_header("x-dev-role", role)
    data = None
    if body is not None:
        data = json.dumps(body).encode()
        req.add_header("content-type", "application/json")
    try:
        with urllib.request.urlopen(req, data=data, timeout=15) as r:
            raw = r.read().decode()
            status = r.status
    except urllib.error.HTTPError as e:
        raw = e.read().decode()
        status = e.code
    try:
        return status, json.loads(raw)
    except json.JSONDecodeError:
        return status, {"raw": raw}


def probe(name, ok, detail=""):
    results.append((name, "PASS" if ok else "FAIL", detail))
    print(f"{'PASS' if ok else 'FAIL':4}  {name:62} {detail}", flush=True)
    return ok


def body_field(status, body, path):
    """Walk a dotted path in a response body; None when it is not there."""
    cur = body
    for part in path.split("."):
        if isinstance(cur, list):
            part_i = int(part)
            cur = cur[part_i] if 0 <= part_i < len(cur) else None
        elif isinstance(cur, dict):
            cur = cur.get(part)
        else:
            return None
    return cur


# ---------------------------------------------------------------------------
# Missions: the six-stage lifecycle (Accords Art 8)
# ---------------------------------------------------------------------------

status, health = call("GET", "/")
probe("00 server is up", status == 200 and health.get("status") == "ok", f"status={status}")

status, body = call("POST", "/api/missions/mission", {
    "title": "Coyote survey — transect week",
    "purpose": "count the local coyote population with the game warden",
    "objectives": "walk six transects, record tracks and scat",
    "expected_impact": "a baseline the town can plan against",
    "category": "conservation",
    "lodge_id": "3",
    "lodge_name": "Windsor",
    "tags": ["wildlife", "tracking"],
    "starts_on": "2026-12-13",
    "ends_on": "2026-12-20",
    "participant_count": 6,
})
mission_next = body_field(status, body, "id")
probe("01 propose a mission (request)", status == 201 and mission_next, f"status={status} id={mission_next}")

status, body = call("POST", "/api/missions/mission", {
    "title": "Broken proposal", "purpose": "", "objectives": "x", "expected_impact": "y"
})
probe("02 an incomplete proposal is refused", status == 400, f"status={status}")

status, body = call("POST", f"/api/missions/mission/{mission_next}/submit")
probe("03 submit: request → review", status == 200 and body.get("stage") == "review", f"status={status} stage={body.get('stage')}")

status, body = call("POST", f"/api/missions/mission/{mission_next}/decision", {"decision": "approved"})
probe("04 approval before review is refused", status == 409, f"status={status}")

status, body = call("POST", f"/api/missions/mission/{mission_next}/review", {
    "success_criteria": "six transects walked and logged",
    "scope_notes": "Windsor town forest, two weekends",
    "mentor": "ar",
})
probe("05 review with a mentor: review → approval", status == 200 and body.get("stage") == "approval", f"status={status}")

status, body = call("GET", f"/api/missions/mission/{mission_next}")
probe("06 the review assigned the mentor", status == 200 and body_field(status, body, "mentorships.0.mentor_member") == "ar",
      f"mentor={body_field(status, body, 'mentorships.0.mentor_member')}")

status, body = call("GET", f"/api/missions/mission/{mission_next}/mentor/suggestions")
suggest = body.get("candidates") or []
probe("07 mentor matching ranks candidates", status == 200 and isinstance(suggest, list), f"candidates={len(suggest)}")

status, body = call("POST", f"/api/missions/mission/{mission_next}/decision", {"decision": "rejected", "guidance": ""})
probe("08 a rejection without guidance is refused", status == 400, f"status={status}")

status, body = call("POST", f"/api/missions/mission/{mission_next}/decision", {"decision": "approved"})
probe("09 Lodge Commander approves: approval → execution", status == 200 and body.get("stage") == "execution", f"status={status}")

status, body = call("POST", f"/api/missions/mission/{mission_next}/milestone", {
    "title": "Transect 1-3 walked", "detail": "north half", "due_on": "2026-12-15", "position": 1
})
milestone = body_field(status, body, "milestone.id")
probe("10 add a milestone", status == 201 and milestone, f"status={status} id={milestone}")

status, body = call("PATCH", f"/api/missions/milestone/{milestone}", {"status": "done"})
probe("11 complete the milestone (done implies 100%)",
      status == 200 and body_field(status, body, "milestone.progress_pct") == 100,
      f"progress_pct={body_field(status, body, 'milestone.progress_pct')}")

status, body = call("POST", f"/api/missions/mission/{mission_next}/progress", {
    "note": "transects 1-3 done, 4 planned for Saturday", "progress_pct": 50,
    "service_hours": 6.5, "participant_count": 6
})
probe("12 record execution progress", status == 201, f"status={status}")

status, body = call("POST", f"/api/missions/mission/{mission_next}/debrief", {
    "notes": "all six transects walked",
    "goals_met": "yes — full coverage",
    "lessons": "start earlier in the day",
    "service_hours": 12.0,
})
probe("13 debrief: execution → debrief", status == 200 and body.get("stage") == "debrief", f"status={status}")

status, body = call("POST", f"/api/missions/mission/{mission_next}/complete")
probe("14 report is required before completing", status == 409, f"status={status}")

status, body = call("POST", f"/api/missions/mission/{mission_next}/report", {
    "summary": "six transects, 14 track sets, two dens located",
    "impact_metrics": {"transects": 6, "track_sets": 14, "dens": 2},
    "service_hours": 12.5,
})
probe("15 report: debrief → report", status == 200 and body.get("stage") == "report", f"status={status}")

status, body = call("POST", f"/api/missions/mission/{mission_next}/complete")
probe("16 complete: report → completed", status == 200 and body.get("state") == "completed", f"status={status} state={body.get('state')}")

# --- the Art 8 appeal -------------------------------------------------------

status, appeal_target = call("POST", "/api/missions/mission", {
    "title": "Ridge trail clearing", "purpose": "reopen the ridge trail",
    "objectives": "clear blowdowns over 2 miles", "expected_impact": "safe passage for the winter",
    "lodge_id": "3", "tags": ["trail"],
})
rid = appeal_target.get("id")
call("POST", f"/api/missions/mission/{rid}/submit")
call("POST", f"/api/missions/mission/{rid}/review", {"success_criteria": "two miles cleared"})
status, body = call("POST", f"/api/missions/mission/{rid}/decision",
                    {"decision": "rejected", "guidance": "not this quarter"})
probe("17 a rejection carries guidance", status == 200 and body.get("state") == "rejected", f"status={status}")

status, body = call("POST", f"/api/missions/mission/{rid}/appeal", {"reason": "the quarter restriction is arbitrary"})
appeal_id = body.get("appeal_id")
probe("18 appeal a rejection to the Troop Council", status == 201 and appeal_id, f"status={status} appeal={appeal_id}")

status, body = call("POST", f"/api/missions/appeal/{appeal_id}/decide", {"seconded_by": "", "outcome": "overturned"}, user="cara")
probe("19 an appeal needs a seconding Council member", status == 400, f"status={status}")

status, body = call("POST", f"/api/missions/appeal/{appeal_id}/decide",
                    {"seconded_by": "cara", "outcome": "overturned", "votes_for": 5, "votes_against": 1})
probe("20 TC overturns: the mission proceeds", status == 200 and body.get("mission_stage") == "execution",
      f"status={status} stage={body.get('mission_stage')}")

status, body = call("GET", "/api/missions/impact")
probe("21 the Impact Report rolls completed missions up",
      status == 200 and (body_field(status, body, "totals.completed") or 0) >= 1
      and (body_field(status, body, "totals.service_hours") or 0) >= 12.5,
      f"completed={body_field(status, body, 'totals.completed')} hours={body_field(status, body, 'totals.service_hours')}")

# ---------------------------------------------------------------------------
# Governance: motions, votes, quorum, minutes, Accords (Art 5/9/17)
# ---------------------------------------------------------------------------

status, body = call("POST", "/api/governance/meeting", {
    "body": "tc", "title": "Troop Council — December", "scheduled_for": "2026-12-06T18:00:00Z",
    "quorum_basis": "majority_members", "expected_voters": 5, "location": "Windsor",
})
meeting = body_field(status, body, "meeting.id")
probe("22 create a Troop Council meeting", status == 201 and meeting, f"status={status} meeting={meeting}")

for member in ("bea", "sara", "ira"):
    status, body = call("POST", f"/api/governance/meeting/{meeting}/attendance",
                        {"member_id": member, "present": True, "method": "present"})
probe("23 record attendance (3 of 5 present)", status == 200, f"status={status}")

status, body = call("GET", f"/api/governance/meeting/{meeting}/quorum")
probe("24 quorum is met in real time",
      status == 200 and body_field(status, body, "quorum.met") is True
      and body_field(status, body, "quorum.required") == 3,
      f"required={body_field(status, body, 'quorum.required')} present={body_field(status, body, 'quorum.present')}")

status, body = call("POST", f"/api/governance/meeting/{meeting}/open")
probe("25 open the meeting", status == 200, f"status={status}")

status, body = call("POST", "/api/governance/motion", {
    "title": "Fund the trail-clearing tools", "text": "that the troop buy two crosscut saws",
    "body": "tc", "meeting_id": meeting, "category": "finance", "threshold": "simple_majority",
})
motion = body_field(status, body, "motion.id")
probe("26 propose a motion", status == 201 and motion, f"status={status} motion={motion}")

status, body = call("POST", f"/api/governance/motion/{motion}/second")
probe("27 the mover cannot second their own motion", status == 409, f"status={status}")

status, body = call("POST", f"/api/governance/motion/{motion}/second", user="sara")
probe("28 another member seconds it", status == 200 and body.get("stage") == "seconded", f"status={status} stage={body.get('stage')}")

status, body = call("POST", "/api/governance/motion", {
    "title": "Never seconded", "text": "that this go nowhere", "body": "tc",
    "meeting_id": meeting, "category": "general",
})
unseconded = body_field(status, body, "motion.id")
status, body = call("POST", f"/api/governance/motion/{unseconded}/close", {})
probe("29 closing an unseconded motion is refused",
      status == 409 and "proposed" in json.dumps(body), f"status={status} body={json.dumps(body)[:70]}")

status, body = call("POST", f"/api/governance/motion/{motion}/debate", {"open": True, "note": "saws vs. a service"})
probe("30 open debate", status == 200 and body.get("stage") == "debate", f"status={status}")

status, body = call("POST", f"/api/governance/motion/{motion}/amendment", {
    "kind": "friendly", "text": "buy one crosscut saw and borrow the second", "rationale": "cheaper"
}, user="ira")
amendment = body_field(status, body, "amendment.id")
probe("31 propose a friendly amendment", status == 201 and amendment, f"status={status} amendment={amendment}")

status, body = call("POST", f"/api/governance/amendment/{amendment}/accept", {"note": "accepted in the room"})
probe("32 the mover accepts the friendly amendment", status == 200, f"status={status}")

status, body = call("GET", f"/api/governance/motion/{motion}")
text = body_field(status, body, "motion.text") or ""
probe("33 the accepted amendment is in the motion's text",
      "Amendment" in text and "borrow the second" in text, f"text={text[:60]!r}")

status, body = call("POST", f"/api/governance/motion/{motion}/debate", {"open": False})
probe("34 close debate: debate → voting", status == 200 and body.get("stage") == "voting", f"status={status}")

for user, choice in (("bea", "yes"), ("sara", "yes"), ("ira", "no")):
    status, body = call("POST", f"/api/governance/motion/{motion}/vote",
                        {"choice": choice, "method": "roll_call"}, user=user)
    probe(f"35 vote {choice} recorded ({user})", status == 201, f"status={status}")

status, body = call("POST", f"/api/governance/motion/{motion}/vote", {"choice": "no"}, user="bea")
probe("36 a member votes once", status == 409, f"status={status}")

status, body = call("POST", f"/api/governance/motion/{motion}/vote", {"choice": "yes"}, user="mallory")
probe("37 a member not recorded present cannot vote", status == 403, f"status={status}")

status, body = call("POST", f"/api/governance/motion/{motion}/close", {})
probe("38 the motion carries (2 yes, 1 no)",
      status == 200 and body.get("result") == "passed" and body.get("votes_yes") == 2,
      f"status={status} result={body.get('result')} yes={body.get('votes_yes')}")

status, body = call("POST", f"/api/governance/motion/{motion}/implement", {"note": "ordered from the co-op"})
probe("39 implement the passed motion", status == 200 and body.get("stage") == "implemented", f"status={status}")

# --- quorum refuses a decision ---------------------------------------------

status, body = call("POST", "/api/governance/meeting", {
    "body": "congress", "title": "3rd Catamount Congress", "quorum_basis": "one_third_registered",
    "expected_voters": 27, "scheduled_for": "2026-12-13T15:00:00Z",
})
congress = body_field(status, body, "meeting.id")
call("POST", f"/api/governance/meeting/{congress}/attendance", {"member_id": "bea", "present": True})
status, body = call("GET", f"/api/governance/meeting/{congress}/quorum")
probe("40 a Congress quorum is one third of the registered scouts",
      body_field(status, body, "quorum.required") == 9 and body_field(status, body, "quorum.met") is False,
      f"required={body_field(status, body, 'quorum.required')} present={body_field(status, body, 'quorum.present')}")

status, body = call("POST", "/api/governance/motion", {
    "title": "Adopt the 3rd Catamount Accords", "text": "that the troop adopt the 3rd Accords as amended",
    "body": "congress", "meeting_id": congress, "category": "accords_amendment",
    "threshold": "two_thirds", "amends_accords": True,
})
accords_motion = body_field(status, body, "motion.id")

# A second needs the seconder in the room, exactly as a vote does.
status, body = call("POST", f"/api/governance/motion/{accords_motion}/second", user="sara")
probe("41 a second needs the seconder present", status == 403, f"status={status}")

call("POST", f"/api/governance/meeting/{congress}/attendance", {"member_id": "sara", "present": True})
status, body = call("POST", f"/api/governance/motion/{accords_motion}/second", user="sara")
probe("42 the motion is seconded once the seconder is recorded",
      status == 200 and body.get("stage") == "seconded", f"status={status}")

status, body = call("POST", f"/api/governance/motion/{accords_motion}/close", {})
probe("43 a motion is not decided while the meeting is below quorum",
      status == 409 and "quorum" in json.dumps(body).lower(), f"status={status} body={json.dumps(body)[:80]}")

# The floor fills to nine of twenty-seven — the Congress quorum.
for member in ("ira", "cara", "dan", "eve", "fay", "gil", "hal"):
    call("POST", f"/api/governance/meeting/{congress}/attendance", {"member_id": member, "present": True})
status, body = call("GET", f"/api/governance/meeting/{congress}/quorum")
probe("44 the Congress reaches quorum",
      body_field(status, body, "quorum.met") is True and body_field(status, body, "quorum.present") == 9,
      f"present={body_field(status, body, 'quorum.present')} required={body_field(status, body, 'quorum.required')}")

for user in ("bea", "ira"):
    call("POST", f"/api/governance/motion/{accords_motion}/vote",
         {"choice": "yes", "method": "ballot"}, user=user)
status, body = call("POST", f"/api/governance/motion/{accords_motion}/close", {})
probe("45 the Congress motion passes on a two-thirds threshold",
      status == 200 and body.get("result") == "passed", f"status={status} result={body.get('result')}")

status, body = call("POST", "/api/governance/motion", {
    "title": "A council opinion", "text": "that the Council note the season",
    "body": "tc", "meeting_id": meeting, "category": "general",
})
tc_motion = body_field(status, body, "motion.id")
call("POST", f"/api/governance/motion/{tc_motion}/second", user="sara")
call("POST", f"/api/governance/motion/{tc_motion}/debate", {"open": False})
call("POST", f"/api/governance/motion/{tc_motion}/vote", {"choice": "yes"}, user="bea")
status, body = call("POST", f"/api/governance/motion/{tc_motion}/close", {})
tc_result = body.get("result")
status, body = call("POST", "/api/governance/accords/adopt", {
    "motion_id": tc_motion, "title": "Not the Accords", "summary": "a council motion cannot adopt"
})
probe("only a Congress motion adopts the Accords", status == 400, f"status={status}")

status, body = call("POST", "/api/governance/accords/adopt", {
    "motion_id": accords_motion, "title": "3rd Catamount Accords",
    "summary": "as amended at the 3rd Congress", "body_md": "# The 3rd Catamount Accords",
    "adopted_on": "2026-12-13", "congress": "3rd Catamount Congress",
})
version = body_field(status, body, "accords.version")
probe("a passed Congress motion creates an Accords version", status == 201 and version, f"status={status} version={version}")

status, body = call("POST", "/api/governance/accords/adopt", {
    "motion_id": accords_motion, "title": "3rd Catamount Accords"
})
probe("the same motion cannot adopt twice", status == 409, f"status={status}")

status, body = call("GET", "/api/governance/accords")
probe("the Accords version list has it",
      status == 200 and any(v.get("version") == version for v in (body.get("accords_versions") or [])),
      f"versions={[v.get('version') for v in (body.get('accords_versions') or [])]}")

status, body = call("GET", f"/api/governance/accords/{version}")
probe("the version carries its text", status == 200 and "3rd Catamount Accords" in json.dumps(body),
      f"status={status}")

# --- amendments on a fresh motion, formal this time ------------------------

status, body = call("POST", "/api/governance/motion", {
    "title": "Set the winter meeting cadence", "text": "that the Council meet monthly",
    "body": "tc", "meeting_id": meeting, "category": "policy",
})
cadence = body_field(status, body, "motion.id")
call("POST", f"/api/governance/motion/{cadence}/second", user="sara")
call("POST", f"/api/governance/motion/{cadence}/debate", {"open": True})
status, body = call("POST", f"/api/governance/motion/{cadence}/amendment", {
    "kind": "formal", "text": "meet monthly except July and August", "rationale": "summer"
}, user="ira")
formal = body_field(status, body, "amendment.id")
status, body = call("POST", f"/api/governance/amendment/{formal}/accept", {})
probe("48 a formal amendment is not accepted by the mover", status == 409, f"status={status}")

call("POST", f"/api/governance/amendment/{formal}/vote", {"choice": "yes", "method": "show_of_hands"}, user="bea")
call("POST", f"/api/governance/amendment/{formal}/vote", {"choice": "yes", "method": "show_of_hands"}, user="ira")
call("POST", f"/api/governance/amendment/{formal}/vote", {"choice": "no", "method": "show_of_hands"}, user="sara")
status, body = call("POST", f"/api/governance/amendment/{formal}/close", {})
probe("49 a formal amendment is tallied and applied",
      status == 200 and body.get("status") == "accepted", f"status={status} status={body.get('status')}")

status, body = call("GET", f"/api/governance/motion/{cadence}")
text = body_field(status, body, "motion.text") or ""
probe("50 the formal amendment is in the motion's text", "except July" in text, f"text={text[:70]!r}")

# --- minutes ----------------------------------------------------------------

status, body = call("POST", f"/api/governance/meeting/{meeting}/minutes/draft")
minutes = body.get("minutes") if isinstance(body, dict) else None
probe("51 minutes are drafted from the motion record",
      status == 201 and minutes and "Motion" in minutes and "Draft generated" in minutes,
      f"status={status}")

status, body = call("POST", f"/api/governance/meeting/{meeting}/minutes/adopt", {})
probe("52 adopt the minutes", status == 200 and body.get("minutes_status") == "adopted", f"status={status}")

status, body = call("POST", f"/api/governance/meeting/{meeting}/close", {})
probe("53 close the meeting (undecided motions are reported)",
      status == 200 and "undecided_motions" in body, f"status={status}")

# ---------------------------------------------------------------------------
# The events the milestone turns on
# ---------------------------------------------------------------------------

status, body = call("GET", "/api/events/recent?limit=500")
events = body.get("events") if isinstance(body, dict) else None
types = [e.get("event_type") for e in (events or [])]
probe("54 mission.completed reached the event bus", "mission.completed" in types, f"status={status}")
probe("55 motion.passed reached the event bus", "motion.passed" in types, f"types={[t for t in types if t and t.startswith(('mission', 'motion'))][:8]}")

completed = next((e for e in (events or [])
                   if e.get("event_type") == "mission.completed"
                   and e.get("payload", {}).get("mission_id") == mission_next), None)
probe("56 the completion payload carries the mission and its impact",
      completed is not None
      and completed["payload"].get("stage") == "report"
      and completed["payload"].get("lodge_id") == "3"
      and completed["payload"].get("impact", {}).get("service_hours") is not None
      and completed["payload"].get("impact", {}).get("metrics", {}).get("transects") == 6,
      f"payload={json.dumps(completed['payload'])[:120] if completed else 'missing'}")

passed = next((e for e in (events or [])
               if e.get("event_type") == "motion.passed"
               and e.get("payload", {}).get("motion_id") == motion), None)
probe("58 the motion.passed payload carries the tally",
      passed is not None and passed["payload"].get("votes_yes") == 2
      and passed["payload"].get("votes_no") == 1
      and passed["payload"].get("threshold") == "simple_majority"
      and passed["payload"].get("body") == "tc",
      f"payload={json.dumps(passed['payload'])[:110] if passed else 'missing'}")

# The stage trail is the "logged with TC" the Accords require.
status, body = call("GET", f"/api/missions/mission/{mission_next}")
stages = [s.get("to_stage") for s in (body_field(status, body, "stage_log") or [])]
probe("59 the lifecycle left a stage trail",
      stages[:6] == ["request", "review", "approval", "execution", "debrief", "report"],
      f"stages={stages}")

print()
npass = sum(1 for r in results if r[1] == "PASS")
for name, res, detail in results:
    if res != "PASS":
        print(f"FAIL {name:62} {detail}")
print(f"\n{npass}/{len(results)} M4 lifecycle probes passed")
sys.exit(0 if npass == len(results) else 1)
