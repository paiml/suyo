//! Carbon-style retention: keep raw points for a while, then fold them into
//! coarser buckets, then drop them. A policy file maps series globs to a
//! retention list like Graphite's storage-schemas.conf:
//!
//! ```toml
//! [[policy]]
//! name = "business"
//! pattern = "newsletter.**"
//! retentions = "1d:6mon,1w:5y"   # raw daily for 6 months, weekly to 5 years, then gone
//! ```
//!
//! The first entry describes the raw points (its resolution is informational,
//! raw points are never re-bucketed); each later entry is a coarser archive.
//! Archived rows keep count/sum/min/max/last rather than one consolidated
//! value, so avg, sum, min, max, count and last all stay exact after
//! downsampling, and no per-policy aggregation method is needed.
//!
//! Only whole buckets are folded: a week straddling the cutoff stays raw until
//! all of it is older than the cutoff. Running a compaction twice is a no-op.

use crate::eval::match_paths;
use crate::store::{agg_row, rebuild_rollups, Agg, SeriesMeta, Store, ROLLUPS};
use crate::timeutil::{Bucket, Span, Zone};
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;

#[derive(Debug, Deserialize, Default)]
pub struct PoliciesFile {
    #[serde(default, rename = "policy")]
    pub policies: Vec<PolicySpec>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct PolicySpec {
    pub name: String,
    /// Series glob; the first policy whose pattern matches a series wins.
    pub pattern: String,
    /// `resolution:keep` pairs, finest first, e.g. "1d:6mon,1w:5y".
    pub retentions: String,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Level {
    pub res: Bucket,
    pub keep: Span,
}

#[derive(Clone, Debug)]
pub struct Policy {
    pub name: String,
    pub pattern: String,
    pub levels: Vec<Level>,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct CompactReport {
    pub policy: String,
    pub series: usize,
    /// Raw points folded into the first archive.
    pub points_archived: usize,
    /// Archive rows folded into a coarser archive.
    pub rows_rolled: usize,
    /// Points and archive rows past the last level's keep, deleted.
    pub expired: usize,
}

pub fn parse_retentions(s: &str) -> Result<Vec<Level>> {
    let mut out: Vec<Level> = Vec::new();
    for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (r, k) = part
            .split_once(':')
            .with_context(|| format!("expected resolution:keep, got '{part}'"))?;
        let lv = Level {
            res: Bucket::parse(r)?,
            keep: Span::parse(k)?,
        };
        if lv.keep.n <= 0 {
            bail!("keep must be positive in '{part}'");
        }
        if lv.keep.approx_secs() < lv.res.nominal() {
            bail!("'{part}' keeps less than one bucket");
        }
        if let Some(prev) = out.last() {
            if lv.res.nominal() <= prev.res.nominal() {
                bail!("'{part}': each resolution must be coarser than the one before it");
            }
            if lv.keep.approx_secs() <= prev.keep.approx_secs() {
                bail!("'{part}': each keep must be longer than the one before it");
            }
        }
        out.push(lv);
    }
    if out.is_empty() {
        bail!("empty retentions");
    }
    Ok(out)
}

pub fn parse(text: &str) -> Result<Vec<Policy>> {
    let file: PoliciesFile = toml::from_str(text)?;
    file.policies
        .into_iter()
        .map(|p| {
            let levels = parse_retentions(&p.retentions)
                .with_context(|| format!("policy '{}'", p.name))?;
            Ok(Policy {
                name: p.name,
                pattern: p.pattern,
                levels,
            })
        })
        .collect()
}

pub fn load(path: &str) -> Result<Vec<Policy>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
    parse(&text).with_context(|| format!("in {path}"))
}

/// Apply every policy as of `now`. With `dry_run` the work is done inside a
/// transaction that is rolled back, so the report shows what would change.
pub fn compact(
    store: &mut Store,
    policies: &[Policy],
    now: i64,
    zone: Zone,
    dry_run: bool,
) -> Result<Vec<CompactReport>> {
    let mut remaining: Vec<SeriesMeta> = store.series()?;
    let tx = store.conn.transaction()?;
    let mut reports = Vec::new();
    for p in policies {
        let matched: Vec<SeriesMeta> = match_paths(&p.pattern, &remaining)?
            .into_iter()
            .cloned()
            .collect();
        remaining.retain(|m| !matched.iter().any(|x| x.id == m.id));
        let mut rep = CompactReport {
            policy: p.name.clone(),
            series: matched.len(),
            ..Default::default()
        };
        for m in &matched {
            compact_series(&tx, m.id, &p.levels, now, zone, &mut rep)?;
        }
        reports.push(rep);
    }
    if dry_run {
        tx.rollback()?;
    } else {
        tx.commit()?;
    }
    Ok(reports)
}

fn compact_series(
    conn: &Connection,
    id: i64,
    levels: &[Level],
    now: i64,
    zone: Zone,
    rep: &mut CompactReport,
) -> Result<()> {
    for k in 1..levels.len() {
        let target = levels[k].res;
        let boundary = target.start_of(minus(levels[k - 1].keep, now, zone), zone);
        if k == 1 {
            rep.points_archived += archive_points(conn, id, target, boundary, zone)?;
        }
        rep.rows_rolled += roll_archive(conn, id, target, boundary, zone)?;
    }
    let last = levels[levels.len() - 1];
    let cutoff = minus(last.keep, now, zone);
    let mut expired = 0;
    if levels.len() == 1 {
        expired += delete_points_before(conn, id, cutoff)?;
    } else {
        let edge = last.res.start_of(cutoff, zone);
        expired += conn.execute(
            "DELETE FROM archive WHERE series_id=?1 AND ts<?2",
            params![id, edge],
        )?;
        expired += delete_points_before(conn, id, edge)?;
    }
    if expired > 0 {
        conn.execute(
            "UPDATE series SET first_ts=(SELECT min(t) FROM (
                SELECT min(ts) t FROM points WHERE series_id=?1
                UNION ALL SELECT min(ts) FROM archive WHERE series_id=?1)) WHERE id=?1",
            [id],
        )?;
    }
    rep.expired += expired;
    Ok(())
}

/// `now` minus a calendar span, in `zone`.
fn minus(span: Span, now: i64, zone: Zone) -> i64 {
    Span {
        n: -span.n,
        unit: span.unit,
    }
    .add_to(now, zone)
}

/// Fold raw points older than `boundary` into `target` buckets.
fn archive_points(conn: &Connection, id: i64, target: Bucket, boundary: i64, zone: Zone) -> Result<usize> {
    let pts: Vec<(i64, Agg)> = {
        let mut st = conn.prepare_cached(
            "SELECT ts, value FROM points WHERE series_id=?1 AND ts<?2 ORDER BY ts",
        )?;
        let rows = st.query_map(params![id, boundary], |r| {
            Ok((r.get::<_, i64>(0)?, Agg::one(r.get(1)?)))
        })?;
        rows.collect::<std::result::Result<_, _>>()?
    };
    if pts.is_empty() {
        return Ok(0);
    }
    merge_into(conn, id, target, &pts, zone)?;
    delete_points_before(conn, id, boundary)
}

/// Fold archive rows finer than `target` and older than `boundary` into it.
/// Rows left behind at any finer resolution (say, after a policy change) are
/// picked up the same way as the previous level's.
fn roll_archive(conn: &Connection, id: i64, target: Bucket, boundary: i64, zone: Zone) -> Result<usize> {
    let res = target.nominal();
    let rows: Vec<(i64, Agg)> = {
        let mut st = conn.prepare_cached(
            "SELECT ts, cnt, sum, min, max, last FROM archive
             WHERE series_id=?1 AND res<?2 AND ts<?3 ORDER BY ts",
        )?;
        let q = st.query_map(params![id, res, boundary], |r| Ok((r.get(0)?, agg_row(r)?)))?;
        q.collect::<std::result::Result<_, _>>()?
    };
    if rows.is_empty() {
        return Ok(0);
    }
    merge_into(conn, id, target, &rows, zone)?;
    Ok(conn.execute(
        "DELETE FROM archive WHERE series_id=?1 AND res<?2 AND ts<?3",
        params![id, res, boundary],
    )?)
}

/// Group time-ordered aggregates by `target` bucket and merge each group into
/// the archive row for that bucket (creating it if needed).
fn merge_into(conn: &Connection, id: i64, target: Bucket, rows: &[(i64, Agg)], zone: Zone) -> Result<()> {
    let res = target.nominal();
    let mut groups: Vec<(i64, Agg)> = Vec::new();
    for (ts, a) in rows {
        let b = target.start_of(*ts, zone);
        match groups.last_mut() {
            Some((gb, g)) if *gb == b => g.merge(a),
            _ => groups.push((b, *a)),
        }
    }
    let mut get = conn.prepare_cached(
        "SELECT ts, cnt, sum, min, max, last FROM archive WHERE series_id=?1 AND res=?2 AND ts=?3",
    )?;
    let mut put = conn.prepare_cached(
        "INSERT OR REPLACE INTO archive(series_id, res, ts, cnt, sum, min, max, last)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )?;
    for (b, g) in groups {
        let mut a = get
            .query_row(params![id, res, b], agg_row)
            .optional()?
            .unwrap_or_default();
        a.merge(&g);
        put.execute(params![id, res, b, a.cnt as i64, a.sum, a.min, a.max, a.last])?;
    }
    Ok(())
}

/// Delete raw points before `ts`, keeping the rollup tables in step.
fn delete_points_before(conn: &Connection, id: i64, ts: i64) -> Result<usize> {
    let n = conn.execute(
        "DELETE FROM points WHERE series_id=?1 AND ts<?2",
        params![id, ts],
    )?;
    if n > 0 {
        for res in ROLLUPS {
            conn.execute(
                &format!("DELETE FROM rollup_{res} WHERE series_id=?1 AND ts<?2"),
                params![id, ts],
            )?;
        }
        // The rollup bucket straddling `ts` now covers fewer points.
        rebuild_rollups(conn, id, ts, ts)?;
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const DAY: i64 = 86400;

    fn utc() -> Zone {
        Zone::parse("UTC").unwrap()
    }

    // 2026-11-23 00:00 UTC, a Monday.
    const NOW: i64 = 1_795_392_000;

    /// One point a day for `days` days ending yesterday, value = day index.
    fn daily(store: &mut Store, name: &str, days: i64) -> Vec<(i64, f64)> {
        let pts: Vec<(String, i64, f64)> = (0..days)
            .map(|i| (name.to_string(), NOW - (days - i) * DAY, i as f64))
            .collect();
        store.write(&pts, &HashMap::new()).unwrap();
        pts.into_iter().map(|(_, t, v)| (t, v)).collect()
    }

    fn policy(pattern: &str, retentions: &str) -> Policy {
        Policy {
            name: pattern.into(),
            pattern: pattern.into(),
            levels: parse_retentions(retentions).unwrap(),
        }
    }

    fn total(store: &Store, name: &str, bucket: &str) -> Agg {
        let m = store.series_by_name(name).unwrap().unwrap();
        let mut a = Agg::default();
        for (_, b) in store
            .fetch(m.id, NOW - 20 * 365 * DAY, NOW, Bucket::parse(bucket).unwrap(), utc())
            .unwrap()
        {
            if let Some(b) = b {
                a.merge(&b);
            }
        }
        a
    }

    fn count(store: &Store, sql: &str) -> i64 {
        store.conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn parses_retentions() {
        let l = parse_retentions("1d:6mon, 1w:5y").unwrap();
        assert_eq!(l.len(), 2);
        assert_eq!(l[1].res, Bucket::parse("1w").unwrap());
        assert_eq!(l[1].keep, Span::parse("5y").unwrap());
        assert!(parse_retentions("1w:5y,1d:6mon").is_err(), "finer after coarser");
        assert!(parse_retentions("1d:1y,1w:6mon").is_err(), "shorter keep later");
        assert!(parse_retentions("1w:3d").is_err(), "keep shorter than a bucket");
        assert!(parse_retentions("1d").is_err());
        assert!(parse_retentions("").is_err());
        let p = parse("[[policy]]\nname='b'\npattern='a.**'\nretentions='1d:6mon,1w:5y'\n").unwrap();
        assert_eq!(p[0].levels.len(), 2);
        assert!(parse("[[policy]]\nname='b'\npattern='a.*'\nretentions='bad'\n").is_err());
    }

    #[test]
    fn downsamples_old_points_into_exact_weekly_aggregates() {
        let mut s = Store::open(":memory:").unwrap();
        let pts = daily(&mut s, "biz.signups", 400);
        let before_week_avg = {
            // A whole week (Mon..Sun) well inside the archived range.
            let wk = Bucket::parse("1w").unwrap();
            let start = wk.start_of(NOW - 300 * DAY, utc());
            let vals: Vec<f64> = pts
                .iter()
                .filter(|(t, _)| *t >= start && *t < start + 7 * DAY)
                .map(|(_, v)| *v)
                .collect();
            assert_eq!(vals.len(), 7);
            (start, vals.iter().sum::<f64>() / 7.0)
        };
        let reps = compact(&mut s, &[policy("biz.**", "1d:180d,1w:5y")], NOW, utc(), false).unwrap();
        assert_eq!(reps[0].series, 1);
        assert!(reps[0].points_archived > 0);

        // Nothing raw is left older than the (week-aligned) 180-day cutoff,
        // and everything newer is untouched.
        let edge = Bucket::parse("1w").unwrap().start_of(NOW - 180 * DAY, utc());
        assert_eq!(count(&s, &format!("SELECT count(*) FROM points WHERE ts<{edge}")), 0);
        let kept = pts.iter().filter(|(t, _)| *t >= edge).count() as i64;
        assert_eq!(count(&s, "SELECT count(*) FROM points"), kept);
        assert_eq!(reps[0].points_archived as i64, 400 - kept);

        // Totals are exact whichever table a query reads from: raw (1min),
        // the hourly rollup (1d) or the archive (1w).
        let want: f64 = pts.iter().map(|(_, v)| v).sum();
        for b in ["1min", "1d", "1w", "1mon"] {
            let a = total(&s, "biz.signups", b);
            assert_eq!(a.sum, want, "sum via {b}");
            assert_eq!(a.cnt, 400.0, "count via {b}");
        }

        // A weekly mean over archived data is the mean of its seven days.
        let m = s.series_by_name("biz.signups").unwrap().unwrap();
        let (start, avg) = before_week_avg;
        let wk = s
            .fetch(m.id, start, start + 7 * DAY, Bucket::parse("1w").unwrap(), utc())
            .unwrap();
        assert_eq!(wk[0].1.unwrap().value(crate::store::AggFn::Avg), avg);
        assert_eq!(wk[0].1.unwrap().cnt, 7.0);
    }

    #[test]
    fn compaction_is_idempotent_and_dry_run_changes_nothing() {
        let mut s = Store::open(":memory:").unwrap();
        daily(&mut s, "biz.a", 400);
        let pol = [policy("biz.*", "1d:90d,1w:1y,1mon:5y")];
        let snapshot = |s: &Store| {
            (
                count(s, "SELECT count(*) FROM points"),
                count(s, "SELECT count(*) FROM archive"),
                count(s, "SELECT count(*) FROM rollup_3600"),
            )
        };
        let orig = snapshot(&s);
        let dry = compact(&mut s, &pol, NOW, utc(), true).unwrap();
        assert!(dry[0].points_archived > 0);
        assert_eq!(snapshot(&s), orig, "dry run must roll back");

        compact(&mut s, &pol, NOW, utc(), false).unwrap();
        let after = snapshot(&s);
        let again = compact(&mut s, &pol, NOW, utc(), false).unwrap();
        assert_eq!(snapshot(&s), after);
        assert_eq!(again[0].points_archived + again[0].rows_rolled + again[0].expired, 0);

        // Data older than 1y moved on from weekly to monthly rows.
        let mon = Bucket::parse("1mon").unwrap().nominal();
        assert!(count(&s, &format!("SELECT count(*) FROM archive WHERE res={mon}")) > 0);
        assert_eq!(total(&s, "biz.a", "1d").cnt, 400.0);
    }

    #[test]
    fn expires_data_past_the_last_level() {
        let mut s = Store::open(":memory:").unwrap();
        let pts = daily(&mut s, "x.raw", 100);
        daily(&mut s, "x.arch", 800);
        let pol = [policy("x.raw", "1d:30d"), policy("x.arch", "1d:30d,1w:1y")];
        let reps = compact(&mut s, &pol, NOW, utc(), false).unwrap();

        // Raw-only policy: points older than 30 days are deleted outright.
        assert_eq!(reps[0].expired, 70);
        let a = total(&s, "x.raw", "1d");
        assert_eq!(a.cnt, 30.0);
        assert_eq!(a.sum, pts[70..].iter().map(|(_, v)| v).sum::<f64>());
        let m = s.series_by_name("x.raw").unwrap().unwrap();
        assert_eq!(m.first, Some(NOW - 30 * DAY));

        // Archived policy: nothing survives past ~1 year.
        let b = total(&s, "x.arch", "1w");
        assert!(b.cnt > 360.0 && b.cnt < 372.0, "kept {}", b.cnt);
    }

    #[test]
    fn first_matching_policy_wins_and_unmatched_series_are_untouched() {
        let mut s = Store::open(":memory:").unwrap();
        daily(&mut s, "a.special", 200);
        daily(&mut s, "a.other", 200);
        daily(&mut s, "b.free", 200);
        let pol = [policy("a.special", "1d:150d"), policy("a.*", "1d:30d")];
        let reps = compact(&mut s, &pol, NOW, utc(), false).unwrap();
        assert_eq!((reps[0].series, reps[1].series), (1, 1));
        assert_eq!(total(&s, "a.special", "1d").cnt, 150.0);
        assert_eq!(total(&s, "a.other", "1d").cnt, 30.0);
        assert_eq!(total(&s, "b.free", "1d").cnt, 200.0);
    }

    #[test]
    fn late_write_into_an_archived_range_is_counted() {
        let mut s = Store::open(":memory:").unwrap();
        daily(&mut s, "l.x", 300);
        let pol = [policy("l.x", "1d:60d,1w:2y")];
        compact(&mut s, &pol, NOW, utc(), false).unwrap();
        let before = total(&s, "l.x", "1w");
        // A backfilled point for a day that is already archived.
        s.write(&[("l.x".into(), NOW - 200 * DAY + 3600, 1000.0)], &HashMap::new())
            .unwrap();
        assert_eq!(total(&s, "l.x", "1w").sum, before.sum + 1000.0);
        compact(&mut s, &pol, NOW, utc(), false).unwrap();
        let after = total(&s, "l.x", "1w");
        assert_eq!((after.sum, after.cnt), (before.sum + 1000.0, before.cnt + 1.0));
        assert_eq!(after.max, 1000.0);
    }
}
