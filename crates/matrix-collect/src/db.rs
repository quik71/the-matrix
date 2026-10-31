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
use std::io::Write;
use std::path::Path;

/// v2 adds `routing.ts` and `routing.byte_len`: the two fields of the
/// `expert_selected` line that the routing projection did not already carry.
/// With them the canonical line can be rebuilt byte-for-byte, which is what
/// makes event-line compaction (Stage 7) lossless.
pub const SCHEMA_VERSION: i64 = 2;

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
    /// routing.ts/byte_len exist: only then can a compacted event line be
    /// rebuilt byte-for-byte. A v1 database opened read-only reports false and
    /// the export simply has nothing to reconstruct.
    pub has_ts: bool,
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
                byte_offset INTEGER NOT NULL, seq INTEGER,
                ts REAL, byte_len INTEGER);
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
            has_ts: true,
        };
        s.conn
            .execute(
                "INSERT INTO schema_meta(k,v) VALUES('schema_version',?1)
                 ON CONFLICT(k) DO UPDATE SET v=excluded.v",
                params![SCHEMA_VERSION.to_string()],
            )
            .map_err(|e| e.to_string())?;
        // ---- v1 -> v2 migration: routing.ts / routing.byte_len.
        // The backfill copies the two fields the events table already holds for
        // the same seq, so every pre-v2 routing row becomes reconstructable.
        // Guarded by a schema_meta flag: a NULL ts can only mean "no event line
        // was ever stored for this seq", and rescanning a multi-million-row
        // table on every open would be a startup cost for no gain.
        let done: Option<String> = s
            .conn
            .query_row(
                "SELECT v FROM schema_meta WHERE k='routing_v2_backfill'",
                [],
                |r| r.get(0),
            )
            .ok();
        if done.is_none() {
            let has_ts: i64 = s
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM pragma_table_info('routing') WHERE name='ts'",
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            if has_ts == 0 {
                s.conn
                    .execute_batch("ALTER TABLE routing ADD COLUMN ts REAL; ALTER TABLE routing ADD COLUMN byte_len INTEGER;")
                    .map_err(|e| format!("migrate routing: {e}"))?;
            }
            s.conn
                .execute(
                    "UPDATE routing SET ts=(SELECT ts FROM events WHERE events.seq=routing.seq),
                     byte_len=8+8*k WHERE ts IS NULL",
                    [],
                )
                .map_err(|e| format!("backfill routing: {e}"))?;
            s.conn
                .execute(
                    "INSERT INTO schema_meta(k,v) VALUES('routing_v2_backfill','1')
                     ON CONFLICT(k) DO UPDATE SET v=excluded.v",
                    [],
                )
                .map_err(|e| e.to_string())?;
        }
        Ok(s)
    }

    /// Read-only open for `--export`: never migrates, never writes. A live
    /// collector may hold the WAL, so the caller is expected to export a
    /// consistent copy (or accept the snapshot read).
    pub fn open_readonly(path: &str) -> Result<Store, String> {
        use rusqlite::OpenFlags;
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )
        .map_err(|e| format!("open {path} (ro): {e}"))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|e| e.to_string())?;
        let has_ts: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('routing') WHERE name='ts'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        Ok(Store {
            conn,
            tx: false,
            db_path: path.to_string(),
            has_ts: has_ts > 0,
        })
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
        ts: f64,
    ) -> Result<(), String> {
        let mut idb = Vec::with_capacity(ids.len() * 4);
        for v in ids {
            idb.extend_from_slice(&v.to_le_bytes());
        }
        let mut wb = Vec::with_capacity(weights.len() * 4);
        for v in weights {
            wb.extend_from_slice(&v.to_le_bytes());
        }
        let byte_len = (8 + 8 * k) as i64;
        self.conn
            .execute(
                "INSERT INTO routing(session,pass_index,position,layer,k,expert_ids,weights,byte_offset,seq,ts,byte_len)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                params![
                    session,
                    pass_index as i64,
                    position as i64,
                    layer,
                    k,
                    idb,
                    wb,
                    byte_offset as i64,
                    seq,
                    ts,
                    byte_len
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

    // ------------------------------------------------------- Stage 7: lifecycle
    pub fn db_bytes(&self) -> u64 {
        let pc: i64 = self
            .conn
            .query_row("PRAGMA page_count", [], |r| r.get(0))
            .unwrap_or(0);
        let ps: i64 = self
            .conn
            .query_row("PRAGMA page_size", [], |r| r.get(0))
            .unwrap_or(0);
        (pc * ps).max(0) as u64
    }

    /// Per-table page bytes from SQLite's own `dbstat` virtual table. Returns
    /// an empty vector when the build has no dbstat; callers fall back to
    /// whole-file sizes and say which one they used.
    pub fn table_bytes(&self) -> Vec<(String, u64)> {
        let mut out = Vec::new();
        let mut st = match self
            .conn
            .prepare("SELECT name, SUM(pgsize) FROM dbstat GROUP BY name ORDER BY name")
        {
            Ok(s) => s,
            Err(_) => return out,
        };
        if let Ok(it) = st.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64))
        }) {
            for x in it.flatten() {
                out.push(x);
            }
        }
        out
    }

    pub fn counts(&self) -> Counts {
        let g = |sql: &str| -> u64 {
            self.conn
                .query_row(sql, [], |r| r.get::<_, i64>(0))
                .unwrap_or(0) as u64
        };
        Counts {
            events: g("SELECT COUNT(*) FROM events"),
            expert_events: g("SELECT COUNT(*) FROM events WHERE kind='expert_selected'"),
            sessions: g("SELECT COUNT(*) FROM sessions"),
            passes: g("SELECT COUNT(*) FROM passes"),
            routing: g("SELECT COUNT(*) FROM routing"),
            perf: g("SELECT COUNT(*) FROM perf"),
            hw: g("SELECT COUNT(*) FROM hw"),
            status: g("SELECT COUNT(*) FROM status"),
            gaps: g("SELECT COUNT(*) FROM gaps"),
        }
    }

    /// Compact the canonical `expert_selected` lines of *finished* sessions that
    /// ended more than `hours` ago. This is a storage optimisation, not a
    /// retention cut: `routing` keeps every row and carries `ts`/`byte_len`, so
    /// the exact line is rebuilt on replay/export (proven byte-identical on a
    /// fixture). A session is only touched when the routing projection provably
    /// accounts for every one of its event lines, so nothing can be lost.
    pub fn compact_event_lines(&mut self, hours: f64, vacuum: bool) -> Result<Compact, String> {
        let cutoff = crate::now_unix() - hours.max(0.0) * 3600.0;
        let before = self.db_bytes();
        let mut c = Compact::default();
        let ids: Vec<String> = {
            let mut st = self
                .conn
                .prepare(
                    "SELECT s.id FROM sessions s
                     WHERE s.finished=1 AND COALESCE(s.finished_ts, s.started_ts) < ?1
                       AND EXISTS(SELECT 1 FROM events e WHERE e.session=s.id AND e.kind='expert_selected')
                     ORDER BY COALESCE(s.finished_ts, s.started_ts) DESC",
                )
                .map_err(|e| format!("compact prepare: {e}"))?;
            let it = st
                .query_map(params![cutoff], |r| r.get::<_, String>(0))
                .map_err(|e| format!("compact query: {e}"))?;
            it.flatten().collect()
        };
        self.conn
            .execute_batch("BEGIN IMMEDIATE")
            .map_err(|e| format!("compact begin: {e}"))?;
        let run = (|| -> Result<(), String> {
            for id in &ids {
                let (ev, rt, nul): (i64, i64, i64) = self
                    .conn
                    .query_row(
                        "SELECT (SELECT COUNT(*) FROM events WHERE session=?1 AND kind='expert_selected'),
                                (SELECT COUNT(*) FROM routing WHERE session=?1),
                                (SELECT COUNT(*) FROM routing WHERE session=?1 AND (ts IS NULL OR byte_len IS NULL))",
                        params![id],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .map_err(|e| format!("compact counts: {e}"))?;
                if ev == 0 || ev != rt || nul != 0 {
                    c.sessions_skipped += 1;
                    continue;
                }
                let n = self
                    .conn
                    .execute(
                        "DELETE FROM events WHERE session=?1 AND kind='expert_selected'",
                        params![id],
                    )
                    .map_err(|e| format!("compact delete: {e}"))?;
                c.sessions += 1;
                c.lines_removed += n as u64;
            }
            Ok(())
        })();
        match run {
            Ok(()) => {
                self.conn
                    .execute_batch("COMMIT")
                    .map_err(|e| format!("compact commit: {e}"))?;
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                return Err(e);
            }
        }
        let _ = self.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)");
        if vacuum {
            let _ = self.conn.execute_batch("VACUUM");
            let _ = self.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)");
        }
        c.bytes_before = before;
        c.bytes_after = self.db_bytes();
        c.cutoff_ts = cutoff;
        Ok(c)
    }

    /// Rebuild one `expert_selected` line exactly as `Core::emit` wrote it.
    fn recon_line(
        &self,
        session: &str,
        seq: i64,
        ts: f64,
        pass_index: i64,
        position: i64,
        layer: i64,
        k: i64,
        ids_b: &[u8],
        w_b: &[u8],
        byte_offset: i64,
        byte_len: i64,
    ) -> String {
        let mut ids = String::new();
        for (i, c) in ids_b.chunks_exact(4).enumerate() {
            if i > 0 {
                ids.push(',');
            }
            ids.push_str(&i32::from_le_bytes([c[0], c[1], c[2], c[3]]).to_string());
        }
        let mut ws = String::new();
        for (i, c) in w_b.chunks_exact(4).enumerate() {
            if i > 0 {
                ws.push(',');
            }
            ws.push_str(&format!("{:?}", f32::from_le_bytes([c[0], c[1], c[2], c[3]])));
        }
        format!(
            "{{\"v\":{},\"seq\":{},\"ts\":{:.3},\"type\":\"expert_selected\",\"session\":\"{}\",\"pass_index\":{},\"position\":{},\"layer\":{},\"k\":{},\"expert_ids\":[{}],\"weights\":[{}],\"byte_offset\":{},\"byte_len\":{}}}",
            crate::SCHEMA_V,
            seq,
            ts,
            crate::json::esc(session),
            pass_index,
            position,
            layer,
            k,
            ids,
            ws,
            byte_offset,
            byte_len
        )
    }

    fn missing_routing(&self, session: &str, limit: Option<usize>) -> Result<Vec<(i64, String)>, String> {
        if !self.has_ts {
            return Ok(Vec::new());
        }
        let sql = match limit {
            Some(_) => "SELECT seq,ts,pass_index,position,layer,k,expert_ids,weights,byte_offset,byte_len
                        FROM routing WHERE session=?1
                          AND seq NOT IN (SELECT seq FROM events WHERE session=?1 AND kind='expert_selected')
                        ORDER BY seq DESC LIMIT ?2",
            None => "SELECT seq,ts,pass_index,position,layer,k,expert_ids,weights,byte_offset,byte_len
                     FROM routing WHERE session=?1
                       AND seq NOT IN (SELECT seq FROM events WHERE session=?1 AND kind='expert_selected')
                     ORDER BY seq DESC",
        };
        let mut st = self.conn.prepare(sql).map_err(|e| e.to_string())?;
        let map = |r: &rusqlite::Row| -> rusqlite::Result<(i64, String)> {
            let seq: i64 = r.get(0)?;
            let ts: Option<f64> = r.get(1)?;
            let ids: Vec<u8> = r.get(6)?;
            let w: Vec<u8> = r.get(7)?;
            let line = match ts {
                Some(ts) => self.recon_line(
                    session,
                    seq,
                    ts,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, i64>(5)?,
                    &ids,
                    &w,
                    r.get::<_, i64>(8)?,
                    r.get::<_, i64>(9)?,
                ),
                None => String::new(),
            };
            Ok((seq, line))
        };
        let rows: Vec<(i64, String)> = match limit {
            Some(n) => {
                let it = st
                    .query_map(params![session, n as i64], map)
                    .map_err(|e| e.to_string())?;
                it.flatten().collect()
            }
            None => {
                let it = st.query_map(params![session], map).map_err(|e| e.to_string())?;
                it.flatten().collect()
            }
        };
        let mut rows: Vec<(i64, String)> = rows.into_iter().filter(|(_, l)| !l.is_empty()).collect();
        rows.reverse();
        Ok(rows)
    }

    /// Every canonical NDJSON line of one session, ambient lines excluded,
    /// ordered by seq. Lines still in `events` come first-class; lines compacted
    /// out of `events` are rebuilt from `routing`. This is the one source both
    /// `--replay` and the subscriber preload use, so a compacted session replays
    /// exactly like a live one.
    pub fn replay_lines(&self, session: &str) -> Result<Vec<String>, String> {
        let mut st = self
            .conn
            .prepare(
                "SELECT seq, line FROM events WHERE session=?1 AND kind NOT IN ('telemetry_status','hw_sample')
                 ORDER BY seq",
            )
            .map_err(|e| e.to_string())?;
        let it = st
            .query_map(params![session], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(|e| e.to_string())?;
        let mut all: Vec<(i64, String)> = it.flatten().collect();
        all.extend(self.missing_routing(session, None)?);
        all.sort_by_key(|(s, _)| *s);
        Ok(all.into_iter().map(|(_, l)| l).collect())
    }

    /// Tail replay for a reconnecting subscriber: last `limit` session-scoped
    /// events of `session`, ordered by seq, excluding ambient lines. Returns the
    /// (seq, line) pairs plus the total number of session events (stored lines +
    /// reconstructed ones), so the caller can report truncation honestly.
    pub fn tail(&self, session: &str, limit: usize) -> Result<(Vec<(i64, String)>, u64), String> {
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
        let mut all: Vec<(i64, String)> = it.flatten().collect();
        let total = self.session_event_count(session)?;
        let stored = all.len() as u64;
        if total > stored {
            all.extend(self.missing_routing(session, Some(limit))?);
            all.sort_by_key(|(s, _)| *s);
            if all.len() > limit {
                let cut = all.len() - limit;
                all.drain(0..cut);
            }
        }
        // Always ascending: the preload is a run of consecutive seqs and the
        // caller takes its floor from the LAST line. Returning the DESC fetch
        // order (a bug caught by S3-A4b) made the preload descend, so the app
        // correctly counted the tail as already-seen and the floor was the
        // *smallest* replayed seq — a hole at the boundary.
        all.sort_by_key(|(s, _)| *s);
        Ok((all, total))
    }

    /// Session-scoped event total: stored lines plus routing rows whose line was
    /// compacted away. Counting only `events` would under-report a compacted
    /// session and make `replay_truncated` lie.
    pub fn session_event_count(&self, session: &str) -> Result<u64, String> {
        self.conn
            .query_row(
                "SELECT (SELECT COUNT(*) FROM events WHERE session=?1 AND kind NOT IN ('telemetry_status','hw_sample'))
                      + (SELECT COUNT(*) FROM routing WHERE session=?1
                           AND seq NOT IN (SELECT seq FROM events WHERE session=?1 AND kind='expert_selected'))",
                params![session],
                |r| r.get::<_, i64>(0),
            )
            .map(|v| v as u64)
            .map_err(|e| e.to_string())
    }

    /// The most recent `gap` lines, oldest first. Gap events carry no session,
    /// so a subscriber that attaches later would otherwise never learn that the
    /// collector lost or skipped bytes before it connected.
    pub fn recent_gaps(&self, limit: usize) -> Result<Vec<String>, String> {
        let mut st = self
            .conn
            .prepare("SELECT line FROM events WHERE kind='gap' ORDER BY seq DESC LIMIT ?1")
            .map_err(|e| e.to_string())?;
        let it = st
            .query_map(params![limit as i64], |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for l in it {
            out.push(l.map_err(|e| e.to_string())?);
        }
        out.reverse();
        Ok(out)
    }

    pub fn wal_bytes(&self) -> u64 {
        std::fs::metadata(format!("{}-wal", self.db_path))
            .map(|m| m.len())
            .unwrap_or(0)
    }

    pub fn check_integrity(&self) -> String {
        self.conn
            .query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
            .unwrap_or_else(|e| format!("integrity_check failed: {e}"))
    }

    // ------------------------------------------------------------- Stage 7: export
    pub fn export(&self, dir: &str) -> Result<Export, String> {
        std::fs::create_dir_all(dir).map_err(|e| format!("mkdir {dir}: {e}"))?;
        let mut ex = Export::default();
        let mut files: Vec<(String, u64, u64)> = Vec::new();
        let mut add = |p: String, n: u64| {
            let (_, b) = count_lines(&p);
            files.push((p, n, b));
        };

        let evpath = format!("{dir}/events.ndjson");
        ex.events_lines = self.export_events(&evpath)?;
        ex.reconstructed = if !self.has_ts { 0 } else { self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM routing r WHERE NOT EXISTS
                   (SELECT 1 FROM events e WHERE e.seq=r.seq AND e.kind='expert_selected')",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0) as u64 };
        add(evpath, ex.events_lines);

        ex.sessions = self.dump_sessions(&format!("{dir}/sessions.ndjson"))?;
        add(format!("{dir}/sessions.ndjson"), ex.sessions);
        ex.passes = self.dump_passes(&format!("{dir}/passes.ndjson"))?;
        add(format!("{dir}/passes.ndjson"), ex.passes);
        ex.routing = self.dump_routing(&format!("{dir}/routing.ndjson"))?;
        add(format!("{dir}/routing.ndjson"), ex.routing);
        ex.perf = self.dump_simple(
            &format!("{dir}/perf.ndjson"),
            "SELECT ts,COALESCE(session,''),requests,prompt_tokens,output_tokens,drafts_offered,
                    drafts_accepted,COALESCE(state,''),tok_s FROM perf ORDER BY ts",
            &["ts", "session", "requests", "prompt_tokens", "output_tokens", "drafts_offered",
              "drafts_accepted", "state", "tok_s"],
        )?;
        add(format!("{dir}/perf.ndjson"), ex.perf);
        ex.hw = self.dump_simple(
            &format!("{dir}/hw.ndjson"),
            "SELECT ts,gpu_util,gpu_mem_used_mib,gpu_temp_c,gpu_power_w,load1,mem_avail_mib FROM hw ORDER BY ts",
            &["ts", "gpu_util", "gpu_mem_used_mib", "gpu_temp_c", "gpu_power_w", "load1", "mem_avail_mib"],
        )?;
        add(format!("{dir}/hw.ndjson"), ex.hw);
        ex.status = self.dump_simple(
            &format!("{dir}/status.ndjson"),
            "SELECT ts,events_total,subscribers,drops_total,skipped_bytes,db_ok,queue_max FROM status ORDER BY ts",
            &["ts", "events_total", "subscribers", "drops_total", "skipped_bytes", "db_ok", "queue_max"],
        )?;
        add(format!("{dir}/status.ndjson"), ex.status);
        ex.gaps = self.dump_simple(
            &format!("{dir}/gaps.ndjson"),
            "SELECT ts,COALESCE(session,''),reason,byte_start,byte_end,bytes,records_unattributed,COALESCE(detail,'')
             FROM gaps ORDER BY ts",
            &["ts", "session", "reason", "byte_start", "byte_end", "bytes", "records_unattributed", "detail"],
        )?;
        add(format!("{dir}/gaps.ndjson"), ex.gaps);

        ex.files = files;
        ex.summary = self.build_summary(&mut ex)?;
        let sumpath = format!("{dir}/summary.json");
        let mut f = std::fs::File::create(&sumpath).map_err(|e| format!("summary: {e}"))?;
        f.write_all(ex.summary.as_bytes()).map_err(|e| e.to_string())?;
        f.write_all(b"\n").ok();
        f.flush().ok();
        Ok(ex)
    }

    /// Merge the stored event lines with reconstructed ones as a streaming
    /// two-cursor merge, so a multi-million-row DB exports in constant memory.
    fn export_events(&self, path: &str) -> Result<u64, String> {
        if !self.has_ts {
            // v1 database: no reconstruction possible, archive stored lines only
            let f = std::fs::File::create(path).map_err(|e| format!("{path}: {e}"))?;
            let mut w = std::io::BufWriter::with_capacity(1 << 20, f);
            let mut n = 0u64;
            let mut st = self.conn.prepare("SELECT line FROM events ORDER BY seq").map_err(|e| e.to_string())?;
            let mut it = st.query([]).map_err(|e| e.to_string())?;
            while let Some(r) = it.next().map_err(|e| e.to_string())? {
                let l: String = r.get(0).map_err(|e| e.to_string())?;
                w.write_all(l.as_bytes()).map_err(|e| e.to_string())?;
                w.write_all(b"\n").map_err(|e| e.to_string())?;
                n += 1;
            }
            w.flush().map_err(|e| e.to_string())?;
            return Ok(n);
        }
        let f = std::fs::File::create(path).map_err(|e| format!("{path}: {e}"))?;
        let mut w = std::io::BufWriter::with_capacity(1 << 20, f);
        let mut ev_st = self
            .conn
            .prepare("SELECT seq, line FROM events ORDER BY seq")
            .map_err(|e| e.to_string())?;
        let mut rt_st = self
            .conn
            .prepare(
                "SELECT seq, ts, session, pass_index, position, layer, k, expert_ids, weights,
                        byte_offset, byte_len FROM routing WHERE seq NOT IN
                   (SELECT seq FROM events WHERE kind='expert_selected') ORDER BY seq",
            )
            .map_err(|e| e.to_string())?;
        let mut ev_it = ev_st.query([]).map_err(|e| e.to_string())?;
        let mut rt_it = rt_st.query([]).map_err(|e| e.to_string())?;
        let mut ev = next_ev(&mut ev_it)?;
        let mut rt = next_rt(&mut rt_it, self)?;
        let mut n = 0u64;
        loop {
            let take_ev = match (&ev, &rt) {
                (None, None) => break,
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (Some((a, _)), Some((b, _))) => a <= b,
            };
            let line = if take_ev {
                let l = ev.take().map(|(_, l)| l).unwrap_or_default();
                ev = next_ev(&mut ev_it)?;
                l
            } else {
                let l = rt.take().map(|(_, l)| l).unwrap_or_default();
                rt = next_rt(&mut rt_it, self)?;
                l
            };
            if line.is_empty() {
                continue;
            }
            w.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
            w.write_all(b"\n").map_err(|e| e.to_string())?;
            n += 1;
        }
        w.flush().map_err(|e| e.to_string())?;
        Ok(n)
    }

    fn dump_sessions(&self, path: &str) -> Result<u64, String> {
        let f = std::fs::File::create(path).map_err(|e| format!("{path}: {e}"))?;
        let mut w = std::io::BufWriter::new(f);
        let mut st = self
            .conn
            .prepare(
                "SELECT id,COALESCE(run_id,''),started_ts,finished_ts,requests_before,requests_after,
                        request_delta,tier,COALESCE(end_reason,''),passes,records,positions,
                        trace_byte_start,trace_byte_end,first_seq,last_seq,finished
                 FROM sessions ORDER BY first_seq",
            )
            .map_err(|e| e.to_string())?;
        let it = st.query_map([], |r| {
            Ok(format!(
                "{{\"id\":\"{}\",\"run_id\":\"{}\",\"started_ts\":{},\"finished_ts\":{},\"requests_before\":{},\"requests_after\":{},\"request_delta\":{},\"tier\":\"{}\",\"end_reason\":\"{}\",\"passes\":{},\"records\":{},\"positions\":{},\"trace_byte_start\":{},\"trace_byte_end\":{},\"first_seq\":{},\"last_seq\":{},\"finished\":{}}}",
                crate::json::esc(&r.get::<_, String>(0)?),
                r.get::<_, String>(1)?,
                numf(r.get::<_, Option<f64>>(2)?),
                numf(r.get::<_, Option<f64>>(3)?),
                numi(r.get::<_, Option<i64>>(4)?),
                numi(r.get::<_, Option<i64>>(5)?),
                numi(r.get::<_, Option<i64>>(6)?),
                r.get::<_, String>(7)?,
                r.get::<_, String>(8)?,
                r.get::<_, Option<i64>>(9)?.unwrap_or(0),
                r.get::<_, Option<i64>>(10)?.unwrap_or(0),
                r.get::<_, Option<i64>>(11)?.unwrap_or(0),
                numi(r.get::<_, Option<i64>>(12)?),
                numi(r.get::<_, Option<i64>>(13)?),
                numi(r.get::<_, Option<i64>>(14)?),
                numi(r.get::<_, Option<i64>>(15)?),
                r.get::<_, Option<i64>>(16)?.unwrap_or(0)
            ))
        })
        .map_err(|e| e.to_string())?;
        let mut n = 0u64;
        for l in it.flatten() {
            w.write_all(l.as_bytes()).ok();
            w.write_all(b"\n").ok();
            n += 1;
        }
        w.flush().ok();
        Ok(n)
    }

    fn dump_passes(&self, path: &str) -> Result<u64, String> {
        let f = std::fs::File::create(path).map_err(|e| format!("{path}: {e}"))?;
        let mut w = std::io::BufWriter::new(f);
        let mut st = self
            .conn
            .prepare(
                "SELECT session,pass_index,byte_start,byte_end,records,positions,layers,k,skipped_bytes,resyncs,seq
                 FROM passes ORDER BY session,pass_index",
            )
            .map_err(|e| e.to_string())?;
        let it = st
            .query_map([], |r| {
                Ok(format!(
                    "{{\"session\":\"{}\",\"pass_index\":{},\"byte_start\":{},\"byte_end\":{},\"records\":{},\"positions\":{},\"layers\":{},\"k\":{},\"skipped_bytes\":{},\"resyncs\":{},\"seq\":{}}}",
                    crate::json::esc(&r.get::<_, String>(0)?),
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, i64>(6)?,
                    numi(r.get::<_, Option<i64>>(7)?),
                    r.get::<_, Option<i64>>(8)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(9)?.unwrap_or(0),
                    numi(r.get::<_, Option<i64>>(10)?)
                ))
            })
            .map_err(|e| e.to_string())?;
        let mut n = 0u64;
        for l in it.flatten() {
            w.write_all(l.as_bytes()).ok();
            w.write_all(b"\n").ok();
            n += 1;
        }
        w.flush().ok();
        Ok(n)
    }

    fn dump_routing(&self, path: &str) -> Result<u64, String> {
        let f = std::fs::File::create(path).map_err(|e| format!("{path}: {e}"))?;
        let mut w = std::io::BufWriter::with_capacity(1 << 20, f);
        let cols = if self.has_ts {
            "ts,byte_len"
        } else {
            "NULL AS ts,NULL AS byte_len"
        };
        let mut st = self
            .conn
            .prepare(&format!(
                "SELECT session,pass_index,position,layer,k,expert_ids,weights,byte_offset,seq,{cols}
                 FROM routing ORDER BY seq"
            ))
            .map_err(|e| e.to_string())?;
        let it = st
            .query_map([], |r| {
                let ids: Vec<u8> = r.get(5)?;
                let ws: Vec<u8> = r.get(6)?;
                Ok(format!(
                    "{{\"session\":\"{}\",\"pass_index\":{},\"position\":{},\"layer\":{},\"k\":{},\"expert_ids\":[{}],\"weights\":[{}],\"byte_offset\":{},\"seq\":{},\"ts\":{},\"byte_len\":{}}}",
                    crate::json::esc(&r.get::<_, String>(0)?),
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                    blob_i32_json(&ids),
                    blob_f32_json(&ws),
                    r.get::<_, i64>(7)?,
                    numi(r.get::<_, Option<i64>>(8)?),
                    numf(r.get::<_, Option<f64>>(9)?),
                    numi(r.get::<_, Option<i64>>(10)?)
                ))
            })
            .map_err(|e| e.to_string())?;
        let mut n = 0u64;
        for l in it.flatten() {
            w.write_all(l.as_bytes()).ok();
            w.write_all(b"\n").ok();
            n += 1;
        }
        w.flush().ok();
        Ok(n)
    }

    fn dump_simple(&self, path: &str, sql: &str, names: &[&str]) -> Result<u64, String> {
        let f = std::fs::File::create(path).map_err(|e| format!("{path}: {e}"))?;
        let mut w = std::io::BufWriter::with_capacity(1 << 16, f);
        let cols = names.len();
        let mut st = self.conn.prepare(sql).map_err(|e| e.to_string())?;
        let it = st.query_map([], |r| {
            let mut o = String::from("{");
            for i in 0..cols {
                if i > 0 {
                    o.push(',');
                }
                o.push_str(&format!("\"{}\":", names.get(i).copied().unwrap_or("f")));
                let v = r.get_ref(i).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
                o.push_str(&value_json(v));
            }
            o.push('}');
            Ok(o)
        })
        .map_err(|e| e.to_string())?;
        let mut n = 0u64;
        for l in it.flatten() {
            w.write_all(l.as_bytes()).ok();
            w.write_all(b"\n").ok();
            n += 1;
        }
        w.flush().ok();
        Ok(n)
    }

    fn build_summary(&self, ex: &mut Export) -> Result<String, String> {
        let c = self.counts();
        let mut s = String::new();
        s.push_str("{\"tool\":\"matrix-collect export\",\"schema_version\":");
        s.push_str(&SCHEMA_VERSION.to_string());
        s.push_str(&format!(",\"generated_ts\":{:.3}", crate::now_unix()));
        s.push_str(&format!(",\"db\":\"{}\",\"db_bytes\":{}", crate::json::esc(&self.db_path), self.db_bytes()));
        s.push_str(&format!(
            ",\"counts\":{{\"events_rows\":{},\"expert_event_rows\":{},\"reconstructed_event_lines\":{},\"events_lines\":{},\"sessions\":{},\"passes\":{},\"routing\":{},\"perf\":{},\"hw\":{},\"status\":{},\"gaps\":{}}}",
            c.events, c.expert_events, ex.reconstructed, ex.events_lines, c.sessions, c.passes,
            c.routing, c.perf, c.hw, c.status, c.gaps
        ));
        // identity: every logical event appears exactly once in events.ndjson,
        // and each per-class archive equals its own table row count.
        let logical = c.events + ex.reconstructed;
        ex.logical_events = logical;
        let ident_ok = ex.events_lines == logical
            && ex.routing == c.routing
            && ex.passes == c.passes
            && ex.hw == c.hw
            && ex.status == c.status
            && ex.perf == c.perf
            && ex.gaps == c.gaps
            && ex.sessions == c.sessions;
        ex.identity_equal = ident_ok;
        s.push_str(&format!(
            ",\"identity\":{{\"logical_events\":{},\"events_ndjson_lines\":{},\"stored_event_rows\":{},\"reconstructed_lines\":{},\"equal\":{},\"classes\":{{\"sessions\":[{},{}],\"passes\":[{},{}],\"routing\":[{},{}],\"perf\":[{},{}],\"hw\":[{},{}],\"status\":[{},{}],\"gaps\":[{},{}]}},\"rule\":\"events_rows + routing rows without a stored line = events.ndjson lines; every per-class archive line count equals its table row count (file,sqlite)\"}}",
            logical, ex.events_lines, c.events, ex.reconstructed, ident_ok,
            ex.sessions, c.sessions, ex.passes, c.passes, ex.routing, c.routing,
            ex.perf, c.perf, ex.hw, c.hw, ex.status, c.status, ex.gaps, c.gaps
        ));
        // per-session summary
        s.push_str(",\"sessions\":[");
        if let Ok(mut st) = self.conn.prepare(
            "SELECT s.id,s.tier,s.finished,COALESCE(s.end_reason,''),COALESCE(s.request_delta,-1),
                    s.passes,s.records,s.positions,COALESCE(s.finished_ts,s.started_ts)-s.started_ts,
                    COALESCE(s.trace_byte_end,0)-COALESCE(s.trace_byte_start,0),
                    (SELECT COUNT(*) FROM routing r WHERE r.session=s.id)
             FROM sessions s ORDER BY s.started_ts",
        ) {
            if let Ok(it) = st.query_map([], |r| {
                Ok(format!(
                    "{{\"id\":\"{}\",\"tier\":\"{}\",\"finished\":{},\"end_reason\":\"{}\",\"request_delta\":{},\"passes\":{},\"records\":{},\"positions\":{},\"duration_s\":{:.3},\"trace_bytes\":{},\"routing_rows\":{}}}",
                    crate::json::esc(&r.get::<_, String>(0)?),
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<i64>>(4)?.unwrap_or(-1),
                    r.get::<_, Option<i64>>(5)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(6)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(7)?.unwrap_or(0),
                    r.get::<_, Option<f64>>(8)?.unwrap_or(0.0),
                    r.get::<_, Option<i64>>(9)?.unwrap_or(0),
                    r.get::<_, i64>(10)?
                ))
            }) {
                let rows: Vec<String> = it.flatten().collect();
                s.push_str(&rows.join(","));
            }
        }
        s.push(']');
        // expert + layer histograms from the routing projection
        let (mut hist, mut layers, mut total) = (vec![0u64; 512], vec![0u64; 64], 0u64);
        if let Ok(mut st) = self.conn.prepare("SELECT layer, expert_ids FROM routing") {
            if let Ok(it) = st.query_map([], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?))
            }) {
                for (l, b) in it.flatten() {
                    if (0..64).contains(&l) {
                        layers[l as usize] += 1;
                    }
                    for ch in b.chunks_exact(4) {
                        let e = i32::from_le_bytes([ch[0], ch[1], ch[2], ch[3]]);
                        if (0..512).contains(&e) {
                            hist[e as usize] += 1;
                        }
                        total += 1;
                    }
                }
            }
        }
        let mut top: Vec<(usize, u64)> = hist.iter().copied().enumerate().collect();
        top.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        s.push_str(",\"routing_stats\":{\"expert_selections\":");
        s.push_str(&total.to_string());
        s.push_str(",\"layer_histogram\":[");
        s.push_str(&layers.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(","));
        s.push_str("],\"top_experts\":[");
        s.push_str(
            &top.iter()
                .take(20)
                .map(|(e, n)| format!("{{\"expert\":{e},\"selections\":{n}}}"))
                .collect::<Vec<_>>()
                .join(","),
        );
        s.push_str("]}");
        // perf / hw / status / gaps aggregates
        s.push_str(&format!(
            ",\"perf\":{{\"samples\":{},\"tok_s_min\":{},\"tok_s_max\":{},\"tok_s_avg\":{},\"requests_first\":{},\"requests_last\":{}}}",
            c.perf,
            optf(self.conn.query_row("SELECT MIN(tok_s) FROM perf", [], |r| r.get::<_, Option<f64>>(0)).unwrap_or(None)),
            optf(self.conn.query_row("SELECT MAX(tok_s) FROM perf", [], |r| r.get::<_, Option<f64>>(0)).unwrap_or(None)),
            optf(self.conn.query_row("SELECT AVG(tok_s) FROM perf WHERE tok_s IS NOT NULL", [], |r| r.get::<_, Option<f64>>(0)).unwrap_or(None)),
            self.conn.query_row("SELECT COALESCE(MIN(requests),0) FROM perf", [], |r| r.get::<_, i64>(0)).unwrap_or(0),
            self.conn.query_row("SELECT COALESCE(MAX(requests),0) FROM perf", [], |r| r.get::<_, i64>(0)).unwrap_or(0)
        ));
        s.push_str(&format!(
            ",\"hw\":{{\"samples\":{},\"gpu_util_min\":{},\"gpu_util_max\":{},\"gpu_temp_max\":{},\"gpu_power_max\":{},\"mem_avail_min_mib\":{}}}",
            c.hw,
            optf(self.conn.query_row("SELECT MIN(gpu_util) FROM hw", [], |r| r.get::<_, Option<f64>>(0)).unwrap_or(None)),
            optf(self.conn.query_row("SELECT MAX(gpu_util) FROM hw", [], |r| r.get::<_, Option<f64>>(0)).unwrap_or(None)),
            optf(self.conn.query_row("SELECT MAX(gpu_temp_c) FROM hw", [], |r| r.get::<_, Option<f64>>(0)).unwrap_or(None)),
            optf(self.conn.query_row("SELECT MAX(gpu_power_w) FROM hw", [], |r| r.get::<_, Option<f64>>(0)).unwrap_or(None)),
            self.conn.query_row("SELECT COALESCE(MIN(mem_avail_mib),0) FROM hw", [], |r| r.get::<_, i64>(0)).unwrap_or(0)
        ));
        s.push_str(&format!(
            ",\"status\":{{\"samples\":{},\"queue_max\":{},\"drops_total_max\":{}}}",
            c.status,
            self.conn.query_row("SELECT COALESCE(MAX(queue_max),0) FROM status", [], |r| r.get::<_, i64>(0)).unwrap_or(0),
            self.conn.query_row("SELECT COALESCE(MAX(drops_total),0) FROM status", [], |r| r.get::<_, i64>(0)).unwrap_or(0)
        ));
        s.push_str(",\"gaps\":[");
        if let Ok(mut st) = self.conn.prepare(
            "SELECT reason,COUNT(*),COALESCE(SUM(bytes),0) FROM gaps GROUP BY reason ORDER BY reason",
        ) {
            if let Ok(it) = st.query_map([], |r| {
                Ok(format!(
                    "{{\"reason\":\"{}\",\"count\":{},\"bytes\":{}}}",
                    crate::json::esc(&r.get::<_, String>(0)?),
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?
                ))
            }) {
                let rows: Vec<String> = it.flatten().collect();
                s.push_str(&rows.join(","));
            }
        }
        s.push(']');
        s.push_str(",\"files\":[");
        let fl: Vec<String> = ex
            .files
            .iter()
            .map(|(p, n, b)| {
                format!(
                    "{{\"file\":\"{}\",\"lines\":{},\"bytes\":{}}}",
                    crate::json::esc(p.rsplit('/').next().unwrap_or(p)),
                    n,
                    b
                )
            })
            .collect();
        s.push_str(&fl.join(","));
        s.push_str("]}");
        Ok(s)
    }
}

fn optf(v: Option<f64>) -> String {
    match v {
        Some(f) if f.is_finite() => format!("{f}"),
        _ => "null".into(),
    }
}

fn numi(v: Option<i64>) -> String {
    match v {
        Some(i) => i.to_string(),
        None => "null".into(),
    }
}

fn numf(v: Option<f64>) -> String {
    match v {
        Some(f) if f.is_finite() => format!("{f}"),
        _ => "null".into(),
    }
}

fn value_json(v: rusqlite::types::ValueRef<'_>) -> String {
    use rusqlite::types::ValueRef;
    match v {
        ValueRef::Null => "null".into(),
        ValueRef::Integer(i) => i.to_string(),
        ValueRef::Real(f) => numf(Some(f)),
        ValueRef::Text(t) => format!("\"{}\"", crate::json::esc(&String::from_utf8_lossy(t))),
        ValueRef::Blob(b) => format!("\"{}\"", b.iter().map(|x| format!("{x:02x}")).collect::<String>()),
    }
}

fn blob_i32_json(b: &[u8]) -> String {
    b.chunks_exact(4)
        .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]).to_string())
        .collect::<Vec<_>>()
        .join(",")
}

fn blob_f32_json(b: &[u8]) -> String {
    b.chunks_exact(4)
        .map(|c| format!("{:?}", f32::from_le_bytes([c[0], c[1], c[2], c[3]])))
        .collect::<Vec<_>>()
        .join(",")
}

fn next_rt(it: &mut rusqlite::Rows<'_>, st: &Store) -> Result<Option<(i64, String)>, String> {
    match it.next().map_err(|e| e.to_string())? {
        Some(r) => {
            let seq: i64 = r.get(0).map_err(|e| e.to_string())?;
            let ts: Option<f64> = r.get(1).map_err(|e| e.to_string())?;
            let sess: String = r.get(2).map_err(|e| e.to_string())?;
            let ids: Vec<u8> = r.get(7).map_err(|e| e.to_string())?;
            let w: Vec<u8> = r.get(8).map_err(|e| e.to_string())?;
            let line = match ts {
                Some(ts) => st.recon_line(
                    &sess,
                    seq,
                    ts,
                    r.get::<_, i64>(3).map_err(|e| e.to_string())?,
                    r.get::<_, i64>(4).map_err(|e| e.to_string())?,
                    r.get::<_, i64>(5).map_err(|e| e.to_string())?,
                    r.get::<_, i64>(6).map_err(|e| e.to_string())?,
                    &ids,
                    &w,
                    r.get::<_, i64>(9).map_err(|e| e.to_string())?,
                    r.get::<_, i64>(10).map_err(|e| e.to_string())?,
                ),
                None => String::new(),
            };
            Ok(Some((seq, line)))
        }
        None => Ok(None),
    }
}

fn next_ev(it: &mut rusqlite::Rows<'_>) -> Result<Option<(i64, String)>, String> {
    match it.next().map_err(|e| e.to_string())? {
        Some(r) => Ok(Some((
            r.get::<_, i64>(0).map_err(|e| e.to_string())?,
            r.get::<_, String>(1).map_err(|e| e.to_string())?,
        ))),
        None => Ok(None),
    }
}

fn count_lines(path: &str) -> (u64, u64) {
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return (0, 0),
    };
    use std::io::Read;
    let mut buf = vec![0u8; 1 << 20];
    let (mut n, mut bytes, mut last) = (0u64, 0u64, 0u8);
    loop {
        match f.read(&mut buf) {
            Ok(0) => break,
            Ok(k) => {
                bytes += k as u64;
                n += buf[..k].iter().filter(|&&c| c == b'\n').count() as u64;
                last = buf[k - 1];
            }
            Err(_) => break,
        }
    }
    if bytes > 0 && last != b'\n' {
        n += 1;
    }
    (n, bytes)
}

#[derive(Default)]
pub struct Counts {
    pub events: u64,
    pub expert_events: u64,
    pub sessions: u64,
    pub passes: u64,
    pub routing: u64,
    pub perf: u64,
    pub hw: u64,
    pub status: u64,
    pub gaps: u64,
}

#[derive(Default)]
pub struct Compact {
    pub sessions: u64,
    pub sessions_skipped: u64,
    pub lines_removed: u64,
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub cutoff_ts: f64,
}

#[derive(Default)]
pub struct Export {
    pub events_lines: u64,
    pub reconstructed: u64,
    pub sessions: u64,
    pub passes: u64,
    pub routing: u64,
    pub perf: u64,
    pub hw: u64,
    pub status: u64,
    pub gaps: u64,
    pub files: Vec<(String, u64, u64)>,
    pub summary: String,
    pub identity_equal: bool,
    pub logical_events: u64,
}
