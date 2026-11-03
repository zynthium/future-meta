"""One workflow owns preparation, bounded runner handoff and atomic publication.

The handoff is temporary input for this run, never a next-run bootstrap. The
public checkpoint contains only approved business state and bounded health.
"""

import hashlib
import json
import os
from pathlib import Path
import tempfile
from datetime import datetime, timedelta, timezone

import data_branch as branch
import publish_data
import update_official as update
from update_schedule import access_deadline


def now():
    return datetime.now(timezone.utc)


def digest(body):
    return hashlib.sha256(body).hexdigest()


def write(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(mode="w", dir=path.parent, delete=False) as handle:
        json.dump(value, handle, ensure_ascii=False, sort_keys=True)
        temporary = Path(handle.name)
    os.replace(temporary, path)


def emit(values):
    destination = os.environ.get("GITHUB_OUTPUT")
    if destination:
        with Path(destination).open("a") as handle:
            for key, value in values.items():
                if key in {"prepared", "primary_runner", "backup_runner", "target_through",
                           "collection_started", "needs_backup", "complete"}:
                    handle.write(f"{key}={str(value).lower() if isinstance(value, bool) else value}\n")
    return values


def owned(state):
    attempt = state.get("last_attempt") or {}
    return (
        attempt.get("status") == "running"
        and attempt.get("run_id") == os.environ.get("GITHUB_RUN_ID")
        and attempt.get("workflow_attempt") == os.environ.get("GITHUB_RUN_ATTEMPT", "1")
        and attempt.get("source_commit") == os.environ.get("GITHUB_SHA")
    )


def fail_owned(args, message, health=None):
    parent, manifest, state = branch.read(args.repo)
    if not owned(state):
        return False
    state = branch.finish_attempt(state, "failed", update.utcnow(), error=message)
    if health is not None:
        state["transport_health"] = health
    branch.commit(args.repo, parent, manifest, state, "data: record failed bounded runner check")
    return True


def prepare(args):
    smoke = bool(getattr(args, "offline_smoke", False))
    gate = None if smoke else update.scheduled_decision()
    if gate is not None and not gate["should_update"]:
        return emit({"prepared": False, "reason": gate["reason"]})
    parent, manifest, state = branch.read(args.repo)
    prior = next(iter(state["checked_through"].values()))
    if smoke:
        moment = now()
        slot = "offline-smoke:" + os.environ["GITHUB_RUN_ID"]
        days = [prior]
        deadline = (moment + timedelta(minutes=25)).isoformat()
    else:
        from incremental_inputs import trading_dates

        moment = datetime.fromisoformat(gate["evaluated_at"])
        slot = f"{moment.date().isoformat()}:{gate['slot']}"
        if (state.get("last_attempt") or {}).get("slot") == slot:
            return emit({"prepared": False, "reason": "slot_already_attempted"})
        start = (datetime.fromisoformat(prior).date() + timedelta(days=1)).isoformat()
        days = trading_dates(start, update.completed_china_date(moment).isoformat())
        if not days:
            return emit({"prepared": False, "reason": "already_checked"})
        deadline = access_deadline(gate)
    from collection_transport import preferred_runners

    primary, backup = ("ubuntu-latest", "macos-15-intel") if smoke else preferred_runners(state.get("transport_health"))
    args.work_dir.mkdir(parents=True, exist_ok=False)
    run_id = os.environ["GITHUB_RUN_ID"]
    started = branch.begin_attempt(state, slot, update.utcnow(),
                                   f"https://github.com/{args.repo}/actions/runs/{run_id}", run_id)
    started["last_attempt"].update(
        workflow_attempt=os.environ.get("GITHUB_RUN_ATTEMPT", "1"),
        source_commit=os.environ["GITHUB_SHA"],
    )
    if not smoke:
        parent = branch.commit(args.repo, parent, manifest, started, "data: begin bounded runner check")
    session = {
        "schema": 1, "repo": args.repo, "run_id": run_id,
        "run_attempt": os.environ.get("GITHUB_RUN_ATTEMPT", "1"),
        "source_commit": os.environ["GITHUB_SHA"], "offline_smoke": smoke,
        "deadline": deadline, "rules_sha256": digest(args.rules.read_bytes()),
        "parent": parent, "manifest": manifest, "started": started,
        "days": days, "previous_through": prior,
        "primary_runner": primary, "backup_runner": backup,
    }
    if smoke:
        session["original_state"] = state
    write(args.work_dir / "session.json", session)
    emit({"prepared": True, "primary_runner": primary, "backup_runner": backup,
          "target_through": days[-1]})
    body = publish_data.download_asset(manifest["mirrors"][0], manifest["size"] + 1)
    if len(body) != manifest["size"] or digest(body) != manifest["sha256"]:
        raise ValueError("Handoff baseline archive checksum mismatch")
    (args.work_dir / "baseline.fmeta.zst").write_bytes(body)
    return emit({"prepared": True, "primary_runner": primary, "backup_runner": backup,
                 "target_through": days[-1]})


def load(args, check_window=False):
    session = json.loads((args.work_dir / "session.json").read_text())
    if (
        session.get("schema") != 1 or session.get("repo") != args.repo
        or session.get("run_id") != os.environ.get("GITHUB_RUN_ID")
        or session.get("run_attempt") != os.environ.get("GITHUB_RUN_ATTEMPT", "1")
        or session.get("source_commit") != os.environ.get("GITHUB_SHA")
        or session.get("offline_smoke") != bool(getattr(args, "offline_smoke", False))
        or session.get("rules_sha256") != digest(args.rules.read_bytes())
    ):
        raise ValueError("Runner handoff belongs to different code, rules or workflow attempt")
    body = (args.work_dir / "baseline.fmeta.zst").read_bytes()
    if len(body) != session["manifest"]["size"] or digest(body) != session["manifest"]["sha256"]:
        raise ValueError("Runner handoff baseline was modified")
    if not session["offline_smoke"]:
        actual = branch.read(args.repo)
        if actual != (session["parent"], session["manifest"], session["started"]):
            raise ValueError("Running publication baseline changed during handoff")
    if check_window:
        deadline = datetime.fromisoformat(session["deadline"])
        if deadline.tzinfo is None or now() >= deadline:
            raise ValueError("Official access window closed before runner continuation")
        if not session["offline_smoke"]:
            gate = update.scheduled_decision()
            if not gate["should_update"] or access_deadline(gate) != session["deadline"]:
                raise ValueError("Runner continuation has no matching official calendar window")
    return session


def session_digest(session):
    return digest(json.dumps(session, sort_keys=True).encode())


def collect(args):
    stage = args.phase
    if stage not in ("primary", "backup"):
        raise ValueError("Unknown collection stage")
    emit({"collection_started": True})
    report_path = args.work_dir / f"stage-{stage}.json"
    if report_path.exists():
        raise ValueError("Runner stage already entered; refusing replay")
    report = {"stage": stage, "complete": False, "needs_backup": False,
              "runner": args.runner, "started_at": update.utcnow()}
    write(report_path, report)
    old_deadline = os.environ.get("FUTURE_META_ACCESS_DEADLINE")
    try:
        session = load(args, check_window=True)
        if args.runner != session[f"{stage}_runner"]:
            raise ValueError("Collection ran on an unexpected runner")
        if stage == "backup":
            previous = args.work_dir / "stage-primary.json"
            if previous.exists():
                primary = json.loads(previous.read_text())
                if primary.get("complete"):
                    report.update(complete=True, reason="primary_already_complete")
                    return emit(report)
                if primary.get("error") and not primary.get("needs_backup"):
                    raise ValueError("Primary business validation failed; no transport retry")
                if not (args.work_dir / "transport.json").exists():
                    raise ValueError("Primary request ledger is missing; consumed budgets are unknown")
            elif os.environ.get("SAFE_SETUP_RECOVERY") != "true":
                raise ValueError("Primary handoff is missing; no proof collection never started")
        os.environ["FUTURE_META_ACCESS_DEADLINE"] = session["deadline"]
        from collection_transport import activate, TransportUnavailable

        with activate(args.work_dir / "transport.json", stage, args.runner,
                      history=session["started"].get("transport_health")):
            try:
                if session["offline_smoke"]:
                    from runner_smoke import collect as smoke_collect

                    collected = smoke_collect(args, session)
                else:
                    from incremental_inputs import collect_interval

                    update.daemon("inspect-archive", "--archive", args.work_dir / "baseline.fmeta.zst",
                                  "--out", args.work_dir / "baseline.json")
                    archive = json.loads((args.work_dir / "baseline.json").read_text())
                    snapshots, versions, exceptions = collect_interval(
                        archive, session["started"], json.loads(args.rules.read_text()),
                        session["days"], args.work_dir / "collected",
                    )
                    collected = {"snapshots": snapshots, "source_versions": versions, "exceptions": exceptions}
                collected["session_sha256"] = session_digest(session)
                write(args.work_dir / "collection.json", collected)
                report["complete"] = True
            except TransportUnavailable as error:
                report.update(error=f"{type(error).__name__}: {error}", needs_backup=stage == "primary")
    except Exception as error:
        report.update(error=f"{type(error).__name__}: {error}", needs_backup=False)
    finally:
        if old_deadline is None:
            os.environ.pop("FUTURE_META_ACCESS_DEADLINE", None)
        else:
            os.environ["FUTURE_META_ACCESS_DEADLINE"] = old_deadline
        report["finished_at"] = update.utcnow()
        write(report_path, report)
    return emit(report)


def finalize(args):
    smoke = bool(getattr(args, "offline_smoke", False))
    if not (args.work_dir / "session.json").exists():
        if not smoke:
            fail_owned(args, "Runner handoff is unavailable; no safe collection replay")
        raise RuntimeError("Runner handoff is unavailable")
    health = None
    try:
        session = load(args)
        from collection_transport import read_health

        health = read_health(args.work_dir / "transport.json", session["started"].get("transport_health"))
        result_path = args.work_dir / "collection.json"
        if not result_path.exists():
            failures = [json.loads(path.read_text()).get("error", "runner interrupted")
                        for path in args.work_dir.glob("stage-*.json")]
            raise RuntimeError("Official collection incomplete: " + "; ".join(failures))
        result = json.loads(result_path.read_text())
        if result.get("session_sha256") != session_digest(session):
            raise ValueError("Collection result belongs to another runner session")
        if smoke:
            from runner_smoke import verify

            return verify(args, session, result)
        batch = {
            "baseline_sha256": session["manifest"]["sha256"],
            "previous_through": session["previous_through"], "trading_days": session["days"],
            "generated_at": session["started"]["last_attempt"]["started_at"],
            "snapshots": result["snapshots"],
        }
        write(args.work_dir / "batch.json", batch)
        source = args.work_dir / "publication"
        update.daemon("update-archive", "--archive", args.work_dir / "baseline.fmeta.zst",
                      "--input", args.work_dir / "batch.json", "--out", source)
        report = json.loads((source / "update-report.json").read_text())
        state = branch.finish_attempt(session["started"], "changed" if report["changed"] else "unchanged",
                                      update.utcnow(), through=session["days"][-1])
        state.update(source_versions=result["source_versions"], exceptions=result["exceptions"], transport_health=health)
        parent = publish_data.publish(source if report["changed"] else None, args.repo,
                                      session["source_commit"], session["parent"],
                                      session["manifest"], session["started"], state)
        actual_parent, manifest, actual_state = branch.read(args.repo)
        if actual_parent != parent:
            raise ValueError("Data branch changed during publication readback")
        parent = update.cleanup_publication(args.repo, parent, manifest, actual_state)
        return {"publish": report["changed"], "checked_through": session["days"][-1], "commit": parent}
    except Exception as error:
        if not smoke:
            fail_owned(args, f"{type(error).__name__}: {error}", health)
        raise


def dispatch(args):
    if args.phase == "check":
        load(args, check_window=True)
        return {"admitted": True}
    return {"prepare": prepare, "primary": collect, "backup": collect, "finalize": finalize}[args.phase](args)
