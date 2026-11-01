#!/usr/bin/env python3
"""Build the Stage-8 packet from results8.json + the per-check artifacts.

Nothing here is typed in: every claim's `ok` comes from the harness check of the
same id, and every excerpt is read out of the artifact that check wrote.

usage: make_packet.py            -> /srv/storage/minion-verification/matrix-stage8/packet.json
"""
import json
import os
import time

ART = os.environ.get("STAGE8_ART", "/srv/storage/minion-verification/matrix-stage8/artifacts")
OUT = os.environ.get("STAGE8_PACKET", "/srv/storage/minion-verification/matrix-stage8/packet.json")
REPO = "/srv/storage/work/the-matrix"

CLAIMS = {
    "S8-A1": {
        "claim": "Zero-warning release rebuild of the workspace, the S3 (10), S4 (10), S5 (9) and S6 (10) suites re-run green in this session from their own harnesses against these binaries, and the S7 suite re-executed in this session (its A1/A6/A7/A8 live, its A2/A3/A5 re-verifying the Stage-7 evidence artifacts; the 62-minute soak body was not re-run).",
        "command": "ssh main 'touch crates/*/src/*.rs && cargo build --release'; python3 run_tests.py all (s3); run_tests4.py all; run_tests5.py all; run_tests6.py all; STAGE7_ART=…/s7 python3 run_tests7.py all",
        "artifacts": ["a1.json", "build.log", "prior/s3/results.json", "prior/s4/results.json",
                      "prior/s5/results.json", "prior/results.json", "s7/results7.json",
                      "chain-blocked/s3-attempt1.json", "chain-blocked/s4-retry.json"],
    },
    "S8-A2": {
        "claim": "matrix-collect and matrix-app run as enabled systemd *user* units with the proven ExecStart, the proven After= ordering and a Wayland environment inherited from uwsm's graphical-session.target; three stop/start cycles each restore a healthy pair without a reboot. Activation at login is honestly pending.",
        "command": "systemctl --user is-enabled/show matrix-{collect,app}; readlink graphical-session.target.wants/matrix-app.service; 3x stop/start + health.py",
        "artifacts": ["units.json"],
    },
    "S8-A3": {
        "claim": "kill -9 on the collector is repaired by the unit (NRestarts+1, new MainPID) and the app reconnects with a new meta run_id, replay from the collector's store and reconnects_with_replay incremented; kill -9 on the app is repaired the same way and it reattaches with meta over the socket and the residency estimate repainted (2675). A fresh independent subscriber receives meta first, then replay.",
        "command": "kill -9 $(systemctl --user show matrix-collect -p MainPID --value); kill -9 $(systemctl --user show matrix-app -p MainPID --value); health.py; /tmp/sub8.py",
        "artifacts": ["recovery.json", "sub8.py"],
    },
    "S8-A4": {
        "claim": "Deploy, previous-binary rollback and uninstall were executed for real: the previous compatible release was put back, the committed build was deployed over it through the health gate (installed pair == the pair just built, sha changed), rolled back to that release with its unit files restored, then normalised to the reference build; and a throwaway prefix was installed, upgraded, rolled back, uninstalled keeping its data, refused a wrong purge confirmation and purged with the exact one. The live database was never written by these scripts (same inode and same data-dir listing before/after).",
        "command": "install.sh --bin-src <prev-pair>; deploy/deploy.sh --from local --src … --build --auto-rollback; rollback.sh --to <prev>; install.sh --bin-src <reference>; install.sh/deploy.sh/rollback.sh/db-rotate.sh/uninstall.sh on ~/matrix-tgt",
        "artifacts": ["deploy.json"],
    },
    "S8-A5": {
        "claim": "Logs and runtime state are bounded and measured: journald caps (512M total, 64M per file, daily rotation, 14-day retention) are installed and enforced (disk usage before/after in the evidence), rotation observed, the pair's own log volume measured over a fixed 3 h window, per-unit rate limiting tested and reported as unsupported for user units (man systemd.exec) rather than assumed; the 1-day retention is a real ceiling proven on a shifted copy (routing/passes/events expire, unfinished sessions survive, integrity ok, replay floor reported), the --max-db-mb cap evicts oldest-first until the file fits, and the live unit advertises both.",
        "command": "deploy/journald-caps.sh install; journalctl --disk-usage; SIGUSR2 rotation; 3000-line LogRateLimit probe; journalctl --user -u … --output=json | wc -c",
        "artifacts": ["log-bounds.json"],
    },
    "S8-A6": {
        "claim": "The ops runbook exists in the repo, on the host and in the vault with health, restart, DB restore, backup policy, upgrade, rollback, uninstall, logs and units sections, and every command it marks verifiable was executed successfully, including a database snapshot/restore round-trip on a copy (the live DB was only read).",
        "command": "python3 run_tests8.py a6 (executes every '# verify:' line); sqlite3 .backup + integrity_check round-trip",
        "artifacts": ["runbook.json"],
    },
    "S8-A7": {
        "claim": "The Strata engine is untouched (MainPID, NRestarts, ActiveEnterTimestamp, config sha/mtime, 7900-file inventory count and sha identical before/after); the repo is clean with zero remotes and the units, deploy scripts, runbook and this suite are committed.",
        "command": "engine_state.py before/after; git status --porcelain; git remote -v; git ls-tree -r HEAD deploy verification/stage8",
        "artifacts": ["engine-before.json", "engine-after.json", "engine-fingerprint.json"],
    },
    "S8-A8": {
        "claim": "This packet is complete (every claim has an artifact that exists and ok=true) and the live pair is healthy at close: connected, meta over the socket, residency painted 2675 == slots, no forced drops.",
        "command": "make_packet.py; health.py --wait 30",
        "artifacts": ["final-health.json"],
    },
}

ACCEPTANCE = [
    "S8-A1 S3(10)+S4(10)+S5(9)+S6(10) re-run green same-session and the S7 suite re-executed, zero rustc warnings.",
    "S8-A2 systemd user units with proven ExecStart/After=/environment, enabled, three stop/start cycles proven, autostart choice justified, boot activation honestly pending.",
    "S8-A3 kill -9 collector -> unit restart + app replay/meta reconnect; kill -9 app -> unit restart; identity re-proven with counters after each kill.",
    "S8-A4 deploy, previous-binary rollback and clean uninstall executed with evidence; live DB never mutated by the scripts.",
    "S8-A5 journald caps/rotation measured and enforced; unenforceable knobs reported honestly.",
    "S8-A6 runbook complete and every documented command executed.",
    "S8-A7 engine untouched (full fingerprint), repo clean, zero remotes, units + deploy scripts committed under deploy/.",
    "S8-A8 packet complete and live pair healthy at close.",
]


def jload(name, default=None):
    p = name if os.path.isabs(name) else os.path.join(ART, name)
    try:
        with open(p) as f:
            return json.load(f)
    except Exception:
        return default if default is not None else {}


def main():
    res = jload("results8.json")
    checks = {c["name"]: c for c in (res.get("checks") or [])}
    a1 = jload("a1.json")
    units = jload("units.json")
    rec = jload("recovery.json")
    dep = jload("deploy.json")
    logs = jload("log-bounds.json")
    rb = jload("runbook.json")
    fp = jload("engine-fingerprint.json")
    close = jload("final-health.json")

    claims = []
    for cid, spec in CLAIMS.items():
        chk = checks.get(cid, {"ok": False, "detail": "missing check"})
        evidence = chk.get("detail", "")
        paths = [os.path.join(ART, a) if not os.path.isabs(a) else a for a in spec["artifacts"]]
        claims.append({
            "id": cid, "ok": bool(chk.get("ok")), "claim": spec["claim"],
            "command": spec["command"], "evidence": evidence[:4000],
            "artifact_paths": paths,
        })

    packet = {
        "job_id": "matrix-stage8",
        "task": ("Stage 8 (final) of THE MATRIX: operational hardening — systemd user units and an "
                 "evidence-justified autostart path, failure recovery, upgrade/rollback/uninstall, "
                 "bounded logs/state, and a vault-ready ops runbook."),
        "generated": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
        "session_started": res.get("started"),
        "session_finished": res.get("finished"),
        "acceptance_criteria": ACCEPTANCE,
        "claims": claims,
        "unit_choice": {
            "choice": "systemd user units (systemctl --user), not Hyprland exec-once",
            "why": ("wsdm autologin starts `uwsm start Hyprland`; uwsm imports WAYLAND_DISPLAY / "
                    "XDG_RUNTIME_DIR / HYPRLAND_INSTANCE_SIGNATURE into the systemd user manager and "
                    "activates graphical-session.target, so a unit WantedBy=graphical-session.target "
                    "gets a working Wayland env plus restart policy, journal, enable state and "
                    "ordering. exec-once gives a one-shot unsupervised child with none of those."),
            "evidence": (units.get("units") or {}).get("manager_env"),
            "target_active": (units.get("units") or {}).get("graphical_session_target"),
            "pending_boot": (units.get("units") or {}).get("pending_boot"),
        },
        "prior_suites": {
            "s3": {"n": (a1.get("s3") or {}).get("n"), "failed": (a1.get("s3") or {}).get("failed")},
            "s4": {"n": (a1.get("s4") or {}).get("n"), "failed": (a1.get("s4") or {}).get("failed")},
            "s5": {"n": (a1.get("s5") or {}).get("n"), "failed": (a1.get("s5") or {}).get("failed")},
            "s6": {"n": (a1.get("s6") or {}).get("n"), "failed": (a1.get("s6") or {}).get("failed")},
            "s7": a1.get("s7"),
        },
        "engine_identity": {
            "checks": fp.get("engine_checks"),
            "mainpid": fp.get("mainpid"), "nrestarts": fp.get("nrestarts"),
            "inventory": {"n": fp.get("inventory_n"), "sha": fp.get("inventory_sha")},
        },
        "recovery": {
            "collector_kill": {
                "restart_s": (rec.get("kill_collector") or {}).get("restart_s"),
                "nrestarts": (rec.get("kill_collector") or {}).get("nrestarts"),
                "app": (rec.get("kill_collector") or {}).get("app"),
            },
            "app_kill": {k: (rec.get("kill_app") or {}).get(k) for k in
                         ("restart_s", "nrestarts", "replay_seen", "painted", "meta_source")},
            "fresh_subscriber": rec.get("fresh_subscriber"),
        },
        "deploy": {
            "live_rollback": {"rc": (dep.get("rollback") or {}).get("rc"),
                              "health": dep.get("rollback_health"),
                              "sha_match_manifest": dep.get("rollback_sha_match_manifest")},
            "live_upgrade": {"rc": (dep.get("deploy") or {}).get("rc"),
                             "health": dep.get("deploy_health"),
                             "sha_changed": dep.get("deploy_sha_changed")},
            "throwaway": {"install_rc": (dep.get("tgt_install") or {}).get("rc"),
                          "start": dep.get("tgt_start"),
                          "upgrade_changed_sha": dep.get("tgt_upgrade_changed_sha"),
                          "rollback_restores": dep.get("tgt_rollback_restores"),
                          "purge_wrong_confirm_refused": (dep.get("tgt_purge_wrong_confirm") or {}).get("refused"),
                          "purged": (dep.get("tgt_purge") or {}).get("data_exists") == "no"},
            "live_db_untouched": {"same_inode": dep.get("live_db_same_inode"),
                                  "same_dir": dep.get("live_db_same_dir_listing"),
                                  "before": (dep.get("live_db_before") or {}).get("counts"),
                                  "after": (dep.get("live_db_after") or {}).get("counts")},
        },
        "log_bounds": {
            "caps": logs.get("caps"),
            "disk_usage": [logs.get("disk_usage_before"), logs.get("disk_usage_after")],
            "journal_files": logs.get("journal_files"),
            "unit_volume": {"window_hours": logs.get("window_hours"),
                            "collector": logs.get("collector"), "app": logs.get("app"),
                            "mb_per_day_measured": logs.get("mb_per_day_measured")},
            "rate_limit_probe": logs.get("rate_limit_probe"),
            "state": logs.get("state"),
        },
        "runbook": {
            "repo": os.path.join(REPO, "deploy/OPS-RUNBOOK.md"),
            "host": "/home/main/the-matrix/OPS-RUNBOOK.md",
            "vault": "/home/main/Vault/Infrastructure/THE MATRIX — Operations Runbook.md",
            "exists": rb.get("exists"), "sections": rb.get("sections"),
            "commands_executed": len(rb.get("executed") or []),
            "commands_ok": rb.get("all_commands_ok"),
            "restore_roundtrip": rb.get("restore_test"),
        },
        "live_at_close": close,
        "artifacts": {
            "artifacts_dir": ART,
            "results8": os.path.join(ART, "results8.json"),
            "harness": os.path.join(REPO, "verification/stage8/run_tests8.py"),
            "packet_builder": os.path.join(REPO, "verification/stage8/make_packet.py"),
            "unit_templates": [os.path.join(REPO, "deploy/units/matrix-collect.service.in"),
                               os.path.join(REPO, "deploy/units/matrix-app.service.in")],
        },
    }
    packet["complete"] = (all(c["ok"] for c in claims)
                          and all(os.path.exists(p) for c in claims for p in c["artifact_paths"]))
    os.makedirs(os.path.dirname(OUT), exist_ok=True)
    with open(OUT, "w") as f:
        json.dump(packet, f, indent=1)
    print(json.dumps({"packet": OUT, "claims": len(claims), "complete": packet["complete"],
                      "failed": [c["id"] for c in claims if not c["ok"]]}))
    return 0 if packet["complete"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
