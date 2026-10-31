// SQLite persistence: WAL, normalized projections + canonical event log.
//
// Two layers live side by side on purpose:
//   * normalized tables (sessions, passes, routing, perf, hw, status, gaps) are
//     what counts get validated against the engine;
//   * events(seq, session, kind, line) stores the exact NDJSON line that went out
//     on the socket, which is what makes --replay byte-for-byte equal.
// Both are written in the same transaction, so a kill -9 can never leave a
// routing row that has no event (or vice versa).

use rusqlite::{params, Connection};
use std::path::Path;

pub const SCHEMA_VERSION: i64 = 1;

pub struct PassRow<'a> {
    pub session: &'a str,
    pub pass_index: u64,
    pub byte_start: u64,
    pub byte_end: u64,
    pub records: u64,
    pub positions: u64,
    pub layers: u64,
    pub k: i64,
    pub skipped_bytes: u64,
    pub resyncs: u64,
    pub seq: i64,
}

pub struct Store {
    pub conn: Connection,
    pub tx: bool,
    db_path: String,
}

impl Store {
    pub fn open(path: &str) -> Result<Store, String> {
        if let Some(dir) = Path::new(path).parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
        }
        let conn = Connection::open(path).map_err(|e| format!("open {path}: {e}"))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|e| e.to_string())?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA foreign_keys=ON;
             CREATE TABLE IF NOT EXISTS schema_meta(k TEXT PRIMARY KEY, v TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS events(
                seq INTEGER PRIMARY KEY, session TEXT, ts REAL NOT NULL,
                kind TEXT NOT NULL, line TEXT NOT NULL);
             CREATE INDEX IF NOT EXISTS events_session ON events(session, seq);
             CREATE INDEX IF NOT EXISTS events_kind ON events(kind, seq);
             CREATE TABLE IF NOT EXISTS sessions(
                id TEXT PRIMARY KEY, run_id TEXT, started_ts REAL NOT NULL, finished_ts REAL,
                requests_before INTEGER, requests_after INTEGER, request_delta INTEGER,
                tier TEXT NOT NULL, end_reason TEXT, passes INTEGER DEFAULT 0,
                records INTEGER DEFAULT 0, positions INTEGER DEFAULT 0,
                trace_byte_start INTEGER, trace_byte_end INTEGER,
                first_seq INTEGER, last_seq INTEGER, finished INTEGER DEFAULT 0);
             CREATE TABLE IF NOT EXISTS passes(
                row_id INTEGER PRIMARY KEY AUTOINCREMENT, session TEXT NOT NULL,
                pass_index INTEGER NOT NULL, byte_start INTEGER NOT NULL,
                byte_end INTEGER NOT NULL, records INTEGER NOT NULL, positions INTEGER NOT NULL,
                layers INTEGER NOT NULL, k INTEGER, skipped_bytes INTEGER DEFAULT 0,
                resyncs INTEGER DEFAULT 0, seq INTEGER, UNIQUE(session, pass_index));
             CREATE TABLE IF NOT EXISTS routing(
                row_id INTEGER PRIMARY KEY AUTOINCREMENT, session TEXT NOT NULL,
                pass_index INTEGER NOT NULL, position INTEGER NOT NULL, layer INTEGER NOT NULL,
                k INTEGER NOT NULL, expert_ids BLOB NOT NULL, weights BLOB NOT NULL,
                byte_offset INTEGER NOT NULL, seq INTEGER);
             CREATE INDEX IF NOT EXISTS routing_session ON routing(session, pass_index);
             CREATE TABLE IF NOT EXISTS perf(
                row_id INTEGER PRIMARY KEY AUTOINCREMENT, session TEXT, ts REAL NOT NULL,
                requests INTEGER, prompt_tokens INTEGER, output_tokens INTEGER,
                drafts_offered INTEGER, drafts_accepted INTEGER, state TEXT, tok_s REAL);
             CREATE TABLE IF NOT EXISTS hw(
                row_id INTEGER PRIMARY KEY AUTOINCREMENT, ts REAL NOT NULL,
                gpu_util REAL, gpu_mem_used_mib INTEGER, gpu_temp_c REAL, gpu_power_w REAL,
                load1 REAL, mem_avail_mib INTEGER);
             CREATE TABLE IF NOT EXISTS status(
                row_id INTEGER PRIMARY KEY AUTOINCREMENT, ts REAL NOT NULL,
                events_total INTEGER, subscribers INTEGER, drops_total INTEGER,
                skipped_bytes INTEGER, db_ok INTEGER, queue_max INTEGER);
             CREATE TABLE IF NOT EXISTS gaps(
                row_id INTEGER PRIMARY KEY AUTOINCREMENT, ts REAL NOT NULL, session TEXT,
                reason TEXT NOT NULL, byte_start INTEGER, byte_end INTEGER,
                bytes INTEGER NOT NULL, records_unattributed INTEGER DEFAULT 0,
                detail TEXT);
             CREATE TABLE IF NOT EXISTS cursor(
                id INTEGER PRIMARY KEY CHECK(id=1), trace_path TEXT, offset INTEGER NOT NULL,
                anchor INTEGER NOT NULL, run_id TEXT, updated_ts REAL);
             CREATE TABLE IF NOT EXISTS runs(
                run_id TEXT PRIMARY KEY, started_ts REAL, pid INTEGER, argv TEXT,
                start_offset INTEGER, end_offset INTEGER, end_reason TEXT);",
        )
        .map_err(|e| format!("schema: {e}"))?;
        let s = Store {
            conn,
            tx: false,
            db_path: path.to_string(),
        };
        s.conn
            .execute(
                "INSERT INTO schema_meta(k,v) VALUES('schema_version',?1)
                 ON CONFLICT(k) DO UPDATE SET v=excluded.v",
                params![SCHEMA_VERSION.to_string()],
            )
            .map_err(|e| e.to_string())?;
        Ok(s)
    }

    pub fn begin(&mut self) -> Result<(), String> {
        if !self.tx {
            self.conn
                .execute_batch("BEGIN IMMEDIATE")
                .map_err(|e| format!("begin: {e}"))?;
            self.tx = true;
        }
        Ok(())
    }

    pub fn commit(&mut self) -> Result<(), String> {
        if self.tx {
            self.conn.execute_batch("COMMIT").map_err(|e| {
                self.tx = false;
                format!("commit: {e}")
            })?;
            self.tx = false;
        }
        Ok(())
    }

    pub fn insert_event(
        &mut self,
        seq: i64,
        session: Option<&str>,
        ts: f64,
        kind: &str,
        line: &str,
    ) -> Result<(), String> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO events(seq,session,ts,kind,line) VALUES(?1,?2,?3,?4,?5)",
                params![seq, session, ts, kind, line],
            )
            .map_err(|e| format!("insert_event: {e}"))?;
        Ok(())
    }

    pub fn session_start(
        &mut self,
        id: &str,
        run_id: &str,
        ts: f64,
        req_before: i64,
        tier: &str,
        byte_start: u64,
        first_seq: i64,
    ) -> Result<(), String> {
        self.conn
            .execute(
                "INSERT INTO sessions(id,run_id,started_ts,requests_before,tier,trace_byte_start,first_seq,finished)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,0)
                 ON CONFLICT(id) DO UPDATE SET started_ts=excluded.started_ts",
                params![id, run_id, ts, req_before, tier, byte_start as i64, first_seq],
            )
            .map_err(|e| format!("session_start: {e}"))?;
        Ok(())
    }

    pub fn session_touch(
        &mut self,
        id: &str,
        passes: u64,
        records: u64,
        positions: u64,
        byte_end: u64,
        last_seq: i64,
    ) -> Result<(), String> {
        self.conn
            .execute(
                "UPDATE sessions SET passes=?2, records=?3, positions=?4, trace_byte_end=?5, last_seq=?6
                 WHERE id=?1",
                params![
                    id,
                    passes as i64,
                    records as i64,
                    positions as i64,
                    byte_end as i64,
                    last_seq
                ],
            )
            .map_err(|e| format!("session_touch: {e}"))?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn session_finish(
        &mut self,
        id: &str,
        ts: f64,
        req_after: i64,
        delta: i64,
        tier: &str,
        end_reason: &str,
    ) -> Result<(), String> {
        self.conn
            .execute(
                "UPDATE sessions SET finished_ts=?2, requests_after=?3, request_delta=?4,
                 tier=?5, end_reason=?6, finished=1 WHERE id=?1",
                params![id, ts, req_after, delta, tier, end_reason],
            )
            .map_err(|e| format!("session_finish: {e}"))?;
        Ok(())
    }

    pub fn insert_pass(&mut self, p: &PassRow<'_>) -> Result<(), String> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO passes(session,pass_index,byte_start,byte_end,records,positions,layers,k,skipped_bytes,resyncs,seq)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                params![
                    p.session,
                    p.pass_index as i64,
                    p.byte_start as i64,
                    p.byte_end as i64,
                    p.records as i64,
                    p.positions as i64,
                    p.layers as i64,
                    p.k,
                    p.skipped_bytes as i64,
                    p.resyncs as i64,
                    p.seq
                ],
            )
            .map_err(|e| format!("insert_pass: {e}"))?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn insert_routing(
        &mut self,
        session: &str,
        pass_index: u64,
        position: u64,
        layer: i32,
        k: i32,
        ids: &[i32],
        weights: &[f32],
        byte_offset: u64,
        seq: i64,
    ) -> Result<(), String> {
        let mut idb = Vec::with_capacity(ids.len() * 4);
        for v in ids {
            idb.extend_from_slice(&v.to_le_bytes());
        }
        let mut wb = Vec::with_capacity(weights.len() * 4);
        for v in weights {
            wb.extend_from_slice(&v.to_le_bytes());
        }
        self.conn
            .execute(
                "INSERT INTO routing(session,pass_index,position,layer,k,expert_ids,weights,byte_offset,seq)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![
                    session,
                    pass_index as i64,
                    position as i64,
                    layer,
                    k,
                    idb,
                    wb,
                    byte_offset as i64,
                    seq
                ],
            )
            .map_err(|e| format!("insert_routing: {e}"))?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn insert_perf(
        &mut self,
        session: Option<&str>,
        ts: f64,
        requests: i64,
        prompt_tokens: i64,
        output_tokens: i64,
        drafts_offered: i64,
        drafts_accepted: i64,
        state: Option<&str>,
        tok_s: Option<f64>,
    ) -> Result<(), String> {
        self.conn
            .execute(
                "INSERT INTO perf(session,ts,requests,prompt_tokens,output_tokens,drafts_offered,drafts_accepted,state,tok_s)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![
                    session, ts, requests, prompt_tokens, output_tokens,
                    drafts_offered, drafts_accepted, state, tok_s
                ],
            )
            .map_err(|e| format!("insert_perf: {e}"))?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn insert_hw(
        &mut self,
        ts: f64,
        gpu_util: Option<f64>,
        gpu_mem: Option<i64>,
        gpu_temp: Option<f64>,
        gpu_power: Option<f64>,
        load1: Option<f64>,
        mem_avail: Option<i64>,
    ) -> Result<(), String> {
        self.conn
            .execute(
                "INSERT INTO hw(ts,gpu_util,gpu_mem_used_mib,gpu_temp_c,gpu_power_w,load1,mem_avail_mib)
                 VALUES(?1,?2,?3,?4,?5,?6,?7)",
                params![ts, gpu_util, gpu_mem, gpu_temp, gpu_power, load1, mem_avail],
            )
            .map_err(|e| format!("insert_hw: {e}"))?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn insert_status(
        &mut self,
        ts: f64,
        events: i64,
        subscribers: i64,
        drops: i64,
        skipped: i64,
        db_ok: bool,
        queue_max: i64,
    ) -> Result<(), String> {
        self.conn
            .execute(
                "INSERT INTO status(ts,events_total,subscribers,drops_total,skipped_bytes,db_ok,queue_max)
                 VALUES(?1,?2,?3,?4,?5,?6,?7)",
                params![ts, events, subscribers, drops, skipped, db_ok as i64, queue_max],
            )
            .map_err(|e| format!("insert_status: {e}"))?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn insert_gap(
        &mut self,
        ts: f64,
        session: Option<&str>,
        reason: &str,
        byte_start: Option<u64>,
        byte_end: Option<u64>,
        bytes: u64,
        records_unattributed: u64,
        detail: &str,
    ) -> Result<(), String> {
        self.conn
            .execute(
                "INSERT INTO gaps(ts,session,reason,byte_start,byte_end,bytes,records_unattributed,detail)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                params![
                    ts,
                    session,
                    reason,
                    byte_start.map(|v| v as i64),
                    byte_end.map(|v| v as i64),
                    bytes as i64,
                    records_unattributed as i64,
                    detail
                ],
            )
            .map_err(|e| format!("insert_gap: {e}"))?;
        Ok(())
    }

    pub fn set_cursor(
        &mut self,
        trace_path: &str,
        offset: u64,
        anchor: u64,
        run_id: &str,
        ts: f64,
    ) -> Result<(), String> {
        self.conn
            .execute(
                "INSERT INTO cursor(id,trace_path,offset,anchor,run_id,updated_ts) VALUES(1,?1,?2,?3,?4,?5)
                 ON CONFLICT(id) DO UPDATE SET trace_path=excluded.trace_path, offset=excluded.offset,
                 anchor=excluded.anchor, run_id=excluded.run_id, updated_ts=excluded.updated_ts",
                params![trace_path, offset as i64, anchor as i64, run_id, ts],
            )
            .map_err(|e| format!("set_cursor: {e}"))?;
        Ok(())
    }

    pub fn get_cursor(&self) -> Option<(String, u64, u64)> {
        self.conn
            .query_row(
                "SELECT trace_path, offset, anchor FROM cursor WHERE id=1",
                [],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)? as u64,
                        r.get::<_, i64>(2)? as u64,
                    ))
                },
            )
            .ok()
    }

    pub fn max_seq(&self) -> i64 {
        self.conn
            .query_row("SELECT COALESCE(MAX(seq),0) FROM events", [], |r| r.get(0))
            .unwrap_or(0)
    }

    pub fn insert_run(
        &mut self,
        run_id: &str,
        ts: f64,
        pid: u32,
        argv: &str,
        start_offset: u64,
    ) -> Result<(), String> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO runs(run_id,started_ts,pid,argv,start_offset) VALUES(?1,?2,?3,?4,?5)",
                params![run_id, ts, pid, argv, start_offset as i64],
            )
            .map_err(|e| format!("insert_run: {e}"))?;
        Ok(())
    }

    pub fn end_run(&mut self, run_id: &str, end_offset: u64, reason: &str) -> Result<(), String> {
        self.conn
            .execute(
                "UPDATE runs SET end_offset=?2, end_reason=?3 WHERE run_id=?1",
                params![run_id, end_offset as i64, reason],
            )
            .map_err(|e| format!("end_run: {e}"))?;
        Ok(())
    }

    pub fn prune(&mut self, days: i64) -> Result<(usize, usize), String> {
        let cutoff = crate::now_unix() - (days as f64) * 86400.0;
        let hw = self
            .conn
            .execute("DELETE FROM hw WHERE ts < ?1", params![cutoff])
            .map_err(|e| e.to_string())?;
        let st = self
            .conn
            .execute("DELETE FROM status WHERE ts < ?1", params![cutoff])
            .map_err(|e| e.to_string())?;
        Ok((hw, st))
    }

    /// Replay source: the canonical NDJSON lines for one session, ambient
    /// (telemetry_status / hw_sample) lines excluded so a replay compares equal
    /// to the session-scoped slice of the live stream.
    pub fn replay_lines(&self, session: &str) -> Result<Vec<String>, String> {
        let mut st = self
            .conn
            .prepare(
                "SELECT line FROM events WHERE session=?1 AND kind NOT IN ('telemetry_status','hw_sample')
                 ORDER BY seq",
            )
            .map_err(|e| e.to_string())?;
        let it = st
            .query_map(params![session], |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for l in it {
            out.push(l.map_err(|e| e.to_string())?);
        }
        Ok(out)
    }

    /// Tail replay for a reconnecting subscriber: last `limit` session-scoped
    /// events of `session`, ordered by seq, excluding ambient lines. Returns the
    /// (seq, line) pairs plus the total number of session events stored, so the
    /// caller can report truncation honestly and set the live floor to the last
    /// replayed seq (never to a higher value, which would silently drop events
    /// that the bounded replay did not include).
    pub fn tail(&self, session: &str, limit: usize) -> Result<(Vec<(i64, String)>, u64), String> {
        // One snapshot for both queries: otherwise the event count can be read
        // after new commits and the "truncated" figure would be inflated by
        // events that arrived between the two statements. When the caller (the
        // producer) already holds a write transaction, its own snapshot is used.
        let own_tx = !self.tx;
        if own_tx {
            self.conn
                .execute_batch("BEGIN DEFERRED")
                .map_err(|e| format!("tail begin: {e}"))?;
        }
        let r = self.tail_inner(session, limit);
        if own_tx {
            let _ = self.conn.execute_batch("COMMIT");
        }
        r
    }

    fn tail_inner(&self, session: &str, limit: usize) -> Result<(Vec<(i64, String)>, u64), String> {
        let mut st = self
            .conn
            .prepare(
                "SELECT seq, line FROM events WHERE session=?1 AND kind NOT IN ('telemetry_status','hw_sample')
                 ORDER BY seq DESC LIMIT ?2",
            )
            .map_err(|e| e.to_string())?;
        let it = st
            .query_map(params![session, limit as i64], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for l in it {
            out.push(l.map_err(|e| e.to_string())?);
        }
        out.reverse();
        let total = self.session_event_count(session)?;
        Ok((out, total))
    }

    /// How many session-scoped events exist for this session (to report an
    /// honest replay truncation instead of losing events silently).
    pub fn session_event_count(&self, session: &str) -> Result<u64, String> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE session=?1 AND kind NOT IN ('telemetry_status','hw_sample')",
                params![session],
                |r| r.get::<_, i64>(0),
            )
            .map(|v| v as u64)
            .map_err(|e| e.to_string())
    }

    pub fn wal_bytes(&self) -> u64 {
        std::fs::metadata(format!("{}-wal", self.db_path))
            .map(|m| m.len())
            .unwrap_or(0)
    }

}
