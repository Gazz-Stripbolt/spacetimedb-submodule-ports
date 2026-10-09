// @spacetimedb/cron port: the upstream integration scenarios (scripts/test-module-local.mjs
// at the pinned commit) against the demo module and the real scheduler.
import assert from 'node:assert/strict';
import { call, callErr, sql, test, done, until, sleep, procErr, unwrap } from './lib.mjs';

const opt = (o) => (o && o[0] === 0 ? o[1] : null);
const job = async (name) => {
  const [r] = await sql(`SELECT * FROM cron_job WHERE name = '${name}'`);
  if (!r) return null;
  return { ...r, disabled_reason: opt(r.disabled_reason), next_run_at: opt(r.next_run_at), last_run_at: opt(r.last_run_at), fire_count: Number(r.fire_count), generation: Number(r.generation) };
};
const fires = async (name) => [
  ...(await sql(`SELECT * FROM cron_fire WHERE job_name = '${name}'`)),
  ...(await sql(`SELECT * FROM cron_procedure_fire WHERE job_name = '${name}'`)),
];
const runs = async (name) => (await sql(`SELECT * FROM cron_run WHERE job_name = '${name}'`)).sort((a, b) => Number(a.sequence) - Number(b.sequence));
const status = (r) => ['Ok', 'Failed'][r.status[0]];
const view = async (name) => (await sql(`SELECT * FROM cron_jobs WHERE name = '${name}'`))[0];
const nowMicros = () => BigInt(Date.now()) * 1000n;

await test('init seeds heartbeat (every 30 s) with one pending trigger and a public view without arguments', async () => {
  const hb = await job('heartbeat');
  assert.ok(hb && hb.enabled, 'heartbeat scheduled by init');
  assert.equal((await fires('heartbeat')).length, 1);
  const v = await view('heartbeat');
  assert.ok(v && v.enabled);
  assert.ok(!('args' in v), 'the public view has no arguments column');
  const inSecs = (hb.next_run_at[0] - Date.now() * 1000) / 1e6;
  assert.ok(inSecs > -2 && inSecs <= 31, `next run within the 30 s interval (${inSecs.toFixed(1)} s)`);
});

await test('schedule() validates its input', async () => {
  const sc = (expr, tz = 'UTC', name = 'flaky') => callErr('schedule_cron', [name, expr, tz, 0]);
  assert.equal(await sc(''), 'cron.invalid_expression:empty');
  assert.equal(await sc('x'.repeat(257)), 'cron.invalid_expression:too_long');
  assert.match(await sc('invalid'), /^cron\.invalid_expression:/);
  assert.match(await sc('0 0 30 2 *'), /^cron\.invalid_expression:Invalid explicit day of month definition/);
  assert.equal(await sc('0 0 * * L'), 'cron.unsatisfiable_expression');
  assert.equal(await sc('0 9 * * *', 'Mars/Olympus_Mons'), 'cron.invalid_timezone:Mars/Olympus_Mons');
  assert.equal(await sc('0 9 * * *', 'x'.repeat(129)), 'cron.invalid_timezone:too_long');
  assert.match(await callErr('schedule_every', ['flaky', 0, 0]), /^cron\.invalid_interval:/);
  assert.match(await callErr('schedule_every', ['flaky', 31_536_001, 0]), /^cron\.invalid_interval:/);
  assert.equal(await sc('0 9 * * *', 'UTC', 'nope'), 'test.unknown_job:nope');
});

await test('a calendar job in a time zone arms its next occurrence there', async () => {
  await call('schedule_cron', ['flaky', '0 9 * * *', 'America/New_York', 0]);
  const f = await job('flaky');
  const expected = unwrap(await call('cron_next_fire_after', ['0 9 * * *', Number(nowMicros()), 'America/New_York']));
  assert.equal(String(f.next_run_at[0]), String(opt(expected)));
  const [fire] = await fires('flaky');
  assert.deepEqual(opt(fire.target_at), f.next_run_at, 'the trigger carries its target');
  await call('unschedule_job', ['flaky']);
});

await test('management calls repair a lost trigger and record it as a lost_fire failure', async () => {
  await call('schedule_every', ['heartbeat', 30, 0]);
  const before = await job('heartbeat');
  await call('drop_heartbeat_fire_for_test');
  assert.equal((await fires('heartbeat')).length, 0);
  await call('schedule_every', ['flaky', 600, 0]); // any management call reconciles every job
  assert.equal((await fires('heartbeat')).length, 1, 'trigger restored');
  const after = await job('heartbeat');
  assert.equal(after.consecutive_failures, 1);
  assert.equal(after.fire_count, before.fire_count + 1);
  const lost = (await runs('heartbeat')).at(-1);
  assert.equal(status(lost), 'Failed');
  assert.equal(opt(lost.error), 'lost_fire');
  await call('unschedule_job', ['flaky']);
});

await test('the reconcile sweep (every 2 s) repairs it without a management call', async () => {
  await call('schedule_every', ['heartbeat', 30, 0]);
  await call('drop_heartbeat_fire_for_test');
  await until(async () => (await fires('heartbeat')).length === 1, 6000, 'reconcile to restore heartbeat');
  assert.equal(opt((await runs('heartbeat')).at(-1).error), 'lost_fire');
});

await test('lost triggers count toward maxFailures and can disable a job', async () => {
  await call('schedule_every', ['heartbeat', 30, 1]);
  await call('drop_heartbeat_fire_for_test');
  const hb = await until(async () => {
    const j = await job('heartbeat');
    return j && !j.enabled ? j : null;
  }, 6000, 'heartbeat to disable');
  assert.equal(hb.disabled_reason, 'failed_1_consecutive_times:lost_fire');
  assert.equal(opt((await view('heartbeat')).disabled_reason), 'lost_fire_threshold_reached');
  assert.equal((await fires('heartbeat')).length, 0);
  await call('schedule_every', ['heartbeat', 30, 0]);
});

await test('a calendar job with arguments fires on cadence; every fire gets the arguments and a unique invocation id', async () => {
  await call('schedule_report', ['*/1 * * * * *', 'UTC', 0, 'nightly', 42]);
  const gen = (await job('report')).generation;
  const logs = await until(async () => {
    const r = await sql("SELECT * FROM argument_log WHERE job_name = 'report'");
    return r.length >= 3 ? r : null;
  }, 8000, 'three report fires');
  assert.ok(logs.every((l) => l.value === 'nightly' && l.count === 42));
  const ids = logs.map((l) => l.invocation_id);
  assert.equal(new Set(ids).size, ids.length, 'unique invocation ids');
  assert.ok(ids.every((id) => id.startsWith(`report:${gen}:`)));
  assert.equal((await fires('report')).length, 1, 'one pending trigger');
  const r = await job('report');
  assert.ok(r.enabled && r.fire_count >= 3 && r.consecutive_failures === 0);
  assert.ok((await runs('report')).length <= 5, 'history capped at 5');
});

await test('the scheduler-only reducer refuses direct calls', async () => {
  assert.equal(await callErr('forge_report_fire_for_test'), 'cron.not_authorized');
  await call('unschedule_job', ['report']);
});

await test('a failing calendar job rolls back its writes, records the failure via recovery, and disables at the threshold', async () => {
  await call('set_flaky_failing', [true]);
  const ticksBefore = (await sql("SELECT * FROM tick_log WHERE job_name = 'flaky'")).length;
  await call('schedule_cron', ['flaky', '*/1 * * * * *', 'UTC', 2]);
  const f = await until(async () => {
    const j = await job('flaky');
    return j && !j.enabled ? j : null;
  }, 8000, 'flaky to disable');
  assert.equal(f.consecutive_failures, 2);
  assert.equal(f.disabled_reason, 'failed_2_consecutive_times:flaky.failure');
  assert.equal(opt((await view('flaky')).disabled_reason), 'failure_threshold_reached');
  assert.equal((await sql("SELECT * FROM tick_log WHERE job_name = 'flaky'")).length, ticksBefore, 'failed writes rolled back');
  const failed = (await runs('flaky')).filter((r) => opt(r.error) === 'flaky.failure');
  assert.ok(failed.length >= 2 && failed.every((r) => status(r) === 'Failed'));
  assert.equal((await fires('flaky')).length, 0, 'disabled jobs have no trigger');
});

await test('a failing interval job is recorded and disabled the same way', async () => {
  await call('schedule_every', ['flaky', 1, 1]);
  const f = await until(async () => {
    const j = await job('flaky');
    return j && !j.enabled ? j : null;
  }, 6000, 'interval flaky to disable');
  assert.equal(f.disabled_reason, 'failed_1_consecutive_times:flaky.failure');
});

await test('fixed and rescheduled, the job resumes; rescheduling starts a new generation', async () => {
  await call('set_flaky_failing', [false]);
  await call('schedule_every', ['flaky', 1, 0]);
  await until(async () => (await sql("SELECT * FROM tick_log WHERE job_name = 'flaky'")).length > 0, 5000, 'flaky to tick');
  const g1 = (await job('flaky')).generation;
  await call('schedule_every', ['flaky', 2, 0]);
  const j = await job('flaky');
  assert.equal(j.generation, g1 + 1);
  const pending = await fires('flaky');
  assert.equal(pending.length, 1);
  assert.equal(Number(pending[0].generation), g1 + 1);
  await call('unschedule_job', ['flaky']);
});

await test('a procedure job fires with its arguments through two transactions', async () => {
  await call('schedule_probe', ['*/1 * * * * *', 'UTC', 0, 'feed']);
  const logs = await until(async () => {
    const r = await sql("SELECT * FROM argument_log WHERE job_name = 'probe'");
    return r.length >= 2 ? r : null;
  }, 8000, 'two probe runs');
  assert.ok(logs.every((l) => l.value === 'feed'));
  assert.equal((await sql("SELECT * FROM cron_procedure_fire WHERE job_name = 'probe'")).length, 1);
  assert.ok((await runs('probe')).every((r) => status(r) === 'Ok'));
});

await test('unschedule disarms and disables without deleting state', async () => {
  await call('unschedule_job', ['probe']);
  const p = await job('probe');
  assert.equal(p.enabled, false);
  assert.equal(p.disabled_reason, 'disabled_by_operator');
  assert.equal(opt((await view('probe')).disabled_reason), 'disabled_by_operator');
  assert.equal((await fires('probe')).length, 0);
  assert.ok(p.fire_count >= 2, 'history kept');
});

await test('procedure failures are recorded in a follow-up transaction and disable at the threshold', async () => {
  await call('set_flaky_failing', [true]);
  await call('schedule_probe', ['*/1 * * * * *', 'UTC', 1, 'feed']);
  const p = await until(async () => {
    const j = await job('probe');
    return j && !j.enabled ? j : null;
  }, 8000, 'probe to disable');
  assert.equal(p.disabled_reason, 'failed_1_consecutive_times:probe.failure');
  assert.equal(opt((await runs('probe')).at(-1).error), 'probe.failure');
  await call('set_flaky_failing', [false]);
});

done();
